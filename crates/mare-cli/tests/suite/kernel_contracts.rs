use super::{Origin, kernel, src, src_at};

#[tokio::test]
async fn redeclaring_a_builtin_contract_rejects_only_the_offending_plugin() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let bad = src("bad-schema", r#"return { name = "bad-schema", contracts = { svc = { methods = {} } } }"#);
            let error = k.load_def(&bad).err().expect("the plugin must not redeclare the built-in contract");
            let error = error.to_string();
            assert!(error.contains("bad-schema") && error.contains("svc"), "{error}");
            let unrelated = src("schema", r#"return { name = "schema", contracts = { example = { methods = {} } } }"#);
            let provider = src_at(
                "builtin-svc",
                Origin::Builtin,
                r#"return { name = "builtin-svc", provide = { "svc" }, apply = function(ctx)
                    local Svc = require("cordis").class("Svc", mare.Service)
                    function Svc:constructor(c) Svc.super.constructor(self, c, "svc") end
                    function Svc:run() return "ok" end
                    ctx:plugin(Svc)
                end }"#,
            );
            k.set_defs(vec![k.load_def(&unrelated).unwrap(), k.load_def(&provider).unwrap()]);
            k.start().await.unwrap();
            // the built-in's service is admitted and reachable (an unrelated plugin's contracts
            // do not refuse it)
            let svc = k.service("svc").expect("svc is provided");
            k.lua().globals().set("__builtin_svc", svc).unwrap();
            assert_eq!(k.lua().load("return __builtin_svc:run()").eval::<String>().unwrap(), "ok");
        })
        .await;
}

#[tokio::test]
async fn baseline_contract_rules_and_replacement_refusal() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            // 1. baseline = true requires expose = "lua"
            let bad_expose = src(
                "bad-baseline-expose",
                r#"return {
                    name = "bad-baseline-expose",
                    contracts = {
                        svc = {
                            methods = {
                                cut = {
                                    doc = "bad",
                                    expose = "both",
                                    access = "peer",
                                    scope = "host",
                                    baseline = true,
                                    result = { type = "object" },
                                },
                            },
                        },
                    },
                }"#,
            );
            let err = k.load_def(&bad_expose).err().expect("baseline = true requires expose = 'lua'");
            assert!(err.to_string().contains("lua") || err.to_string().contains("baseline"), "{err}");

            // 2. baseline = true cannot be optional = true
            let bad_optional = src(
                "bad-baseline-opt",
                r#"return {
                    name = "bad-baseline-opt",
                    contracts = {
                        svc = {
                            methods = {
                                cut = {
                                    doc = "bad",
                                    expose = "lua",
                                    scope = "host",
                                    baseline = true,
                                    optional = true,
                                    result = { type = "object" },
                                },
                            },
                        },
                    },
                }"#,
            );
            let err = k.load_def(&bad_optional).err().expect("baseline = true cannot be optional");
            assert!(err.to_string().contains("optional") || err.to_string().contains("baseline"), "{err}");

            // 3. Built-in typed contract with `baseline_cut = { ..., expose = "lua", baseline = true }`
            // discovers baseline methods and refuses replacement providers omitting them.
            let owner_def = src_at(
                "schema-owner",
                Origin::Builtin,
                r#"return {
                    name = "schema-owner",
                    contracts = {
                        sample = {
                            methods = {
                                ping = { doc = "ping", result = { type = "string" } },
                                baseline_cut = {
                                    doc = "Snapshot cut",
                                    expose = "lua",
                                    scope = "host",
                                    baseline = true,
                                    result = { type = "object" },
                                },
                            },
                        },
                    },
                }"#,
            );
            let valid_provider = src_at(
                "valid-prov",
                Origin::Builtin,
                r#"return {
                    name = "valid-prov",
                    provide = { "sample" },
                    apply = function(ctx)
                        local Svc = require("cordis").class("SampleSvc", mare.Service)
                        function Svc:constructor(c) Svc.super.constructor(self, c, "sample") end
                        function Svc:ping() return "pong" end
                        function Svc:baseline_cut() return { parts = {} } end
                        ctx:plugin(Svc)
                    end,
                }"#,
            );
            k.set_defs(vec![k.load_def(&owner_def).unwrap(), k.load_def(&valid_provider).unwrap()]);
            k.start().await.unwrap();
            assert!(k.problems().is_empty(), "unexpected kernel problems: {:?}", k.problems());
            assert_eq!(k.baseline_methods(), vec![("sample".to_string(), "baseline_cut".to_string())]);

            // Replacement plugin omitting `baseline_cut` must be refused by contract admission.
            let bad_repl = src(
                "valid-prov",
                r#"return {
                    name = "valid-prov",
                    override = true,
                    provide = { "sample" },
                    apply = function(ctx)
                        local Svc = require("cordis").class("BadSampleSvc", mare.Service)
                        function Svc:constructor(c) Svc.super.constructor(self, c, "sample") end
                        function Svc:ping() return "pong2" end
                        ctx:plugin(Svc)
                    end,
                }"#,
            );
            let k2 = kernel();
            k2.set_defs(vec![
                k2.load_def(&owner_def).unwrap(),
                k2.load_def(&valid_provider).unwrap(),
                k2.load_def(&bad_repl).unwrap(),
            ]);
            k2.start().await.unwrap();
            let problems = k2.problems();
            assert!(
                problems.iter().any(|p| p.contains("baseline_cut")),
                "expected baseline_cut refusal in problems: {problems:?}"
            );
            assert!(k2.baseline_methods().is_empty());
        })
        .await;
}

#[test]
fn spi_expose_is_rejected_with_a_migration_hint() {
    let lua = mlua::Lua::new();
    let method_val: mlua::Value = lua
        .load(
            r#"return {
                demo = {
                    methods = {
                        internal_fn = { doc = "d", expose = "spi", result = { type = "boolean" } },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    let method_err = mare_kernel::contracts::parse_contracts(&lua, method_val)
        .expect_err("method expose = \"spi\" must be rejected");
    let method_msg = method_err.to_string();
    assert!(method_msg.contains("lua"), "expected migration hint to `lua`: {method_msg}");

    let event_val: mlua::Value = lua
        .load(
            r#"return {
                demo = {
                    methods = {},
                    events = {
                        ["internal-ev"] = { doc = "d", mode = "emit", expose = "spi" },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    let event_err =
        mare_kernel::contracts::parse_contracts(&lua, event_val).expect_err("event expose = \"spi\" must be rejected");
    let event_msg = event_err.to_string();
    assert!(event_msg.contains("process"), "expected migration hint to `process`: {event_msg}");
}

#[test]
fn baseline_method_must_be_lua_only() {
    let lua = mlua::Lua::new();
    let ok_val: mlua::Value = lua
        .load(
            r#"return {
                demo = {
                    methods = {
                        cut = {
                            doc = "snapshot cut",
                            expose = "lua",
                            scope = "host",
                            baseline = true,
                            result = { type = "object" },
                        },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    mare_kernel::contracts::parse_contracts(&lua, ok_val).expect("baseline = true with expose = \"lua\" must parse");

    let bad_val: mlua::Value = lua
        .load(
            r#"return {
                demo = {
                    methods = {
                        cut = {
                            doc = "bad",
                            expose = "both",
                            scope = "host",
                            baseline = true,
                            result = { type = "object" },
                        },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    let err = mare_kernel::contracts::parse_contracts(&lua, bad_val)
        .expect_err("baseline = true with expose = \"both\" must fail");
    let msg = err.to_string();
    assert!(msg.contains("lua") && msg.contains("baseline"), "{msg}");
}

#[test]
fn remote_expose_is_rejected() {
    let lua = mlua::Lua::new();
    let bad_val: mlua::Value = lua
        .load(
            r#"return {
                demo = {
                    methods = {
                        op = {
                            doc = "remote-only is gone",
                            expose = "remote",
                            access = "peer",
                            result = { type = "object" },
                        },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    let err = mare_kernel::contracts::parse_contracts(&lua, bad_val)
        .expect_err("expose = \"remote\" must be rejected once Expose::Remote is removed");
    let msg = err.to_string();
    assert!(msg.contains("remote"), "{msg}");
}
