use super::{kernel, src};

const OBSERVER: &str = r#"return { name = "raw-observer", apply = function(ctx)
    mare._raw_observer = ctx
end }"#;

async fn refuses(source: &'static str, service: &str) {
    let k = kernel();
    k.set_defs(vec![
        k.load_def(&src("raw-provider", source)).unwrap(),
        k.load_def(&src("raw-observer", OBSERVER)).unwrap(),
    ]);
    k.start().await.unwrap();
    let (code, message, absent): (Option<String>, Option<String>, bool) = k
        .lua()
        .load("return mare._raw_code, mare._raw_message, mare._raw_observer:get(...) == nil")
        .call(service)
        .unwrap();
    assert_eq!(code.as_deref(), Some("invalid_arguments"), "{}", k.problems().join("\n"));
    assert_eq!(
        message.as_deref(),
        Some(format!("service `{service}` is declared: provide it through mare.Service (mare_lib.service)").as_str())
    );
    assert!(!crate::common::plugin_active(&k, "raw-provider"));
    assert!(absent, "another plugin must not see the rejected value");
    assert!(k.service(service).is_none(), "the host must not reach the rejected value");
}

// guarantee: undeclared-raw-provide-refused
#[tokio::test]
async fn raw_provide_declared_by_provide_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(refuses(
            r#"return {name="raw-provider", provide={"listed_probe"}, apply=function(ctx)
      local ok, err = pcall(function() ctx:provide("listed_probe", {run=function() return "leak" end}) end)
      if not ok then mare._raw_code, mare._raw_message = err.code, err.message; error(err, 0) end
    end}"#,
            "listed_probe",
        ))
        .await;
}

// guarantee: undeclared-raw-provide-refused
#[tokio::test]
async fn raw_provide_declared_only_by_contract_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(refuses(
            r#"return {name="raw-provider", contracts={contract_probe={methods={}}}, apply=function(ctx)
      local ok, err = pcall(function() ctx:provide("contract_probe", {run=function() return "leak" end}) end)
      if not ok then mare._raw_code, mare._raw_message = err.code, err.message; error(err, 0) end
    end}"#,
            "contract_probe",
        ))
        .await;
}

#[tokio::test]
async fn raw_cordis_service_under_a_declared_name_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(refuses(
            r#"return {name="raw-provider", provide={"class_probe"}, apply=function(ctx)
      local Raw = require("cordis").class("Raw", require("cordis").Service)
      function Raw:constructor(c) Raw.super.constructor(self, c, "class_probe") end
      local ok, err = pcall(function() Raw.new(ctx) end)
      if not ok then mare._raw_code, mare._raw_message = err.code, err.message; error(err, 0) end
    end}"#,
            "class_probe",
        ))
        .await;
}

#[tokio::test]
async fn raw_service_in_a_named_child_of_a_plugin_row_is_refused() {
    tokio::task::LocalSet::new().run_until(refuses(r#"return {name="raw-provider", provide={"child_raw_probe"}, apply=function(ctx)
      local cordis = require("cordis")
      local Raw = cordis.class("NamedRawChild", cordis.Service)
      function Raw:constructor(c)
        local ok, err = pcall(function() Raw.super.constructor(self, c, "child_raw_probe") end)
        if not ok then mare._raw_code, mare._raw_message = err.code, err.message; mare._raw_error = err; error(err, 0) end
      end
      cordis.await(ctx:plugin(Raw))
      if mare._raw_error then error(mare._raw_error, 0) end
    end}"#, "child_raw_probe")).await;
}

/// Review a-raw-provide (should-fix 1): the admitted mark is bound to the name the instance was
/// admitted under; the same instance raw-provided under another declared name is refused.
#[tokio::test]
async fn an_admitted_instance_raw_provided_under_another_declared_name_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(refuses(
            r#"return {name="raw-provider", provide={"first_probe", "second_probe"},
      contracts={first_probe={methods={run={doc="run",result={type="string"}}}}, second_probe={methods={run={doc="run",result={type="string"}}}}}, apply=function(ctx)
      local A = require("cordis").class("FirstProbe", mare.Service)
      function A:constructor(c) A.super.constructor(self, c, "first_probe") end
      function A:run() return "first" end
      local inst = A.new(ctx)
      local ok, err = pcall(function() ctx:provide("second_probe", inst) end)
      if not ok then mare._raw_code, mare._raw_message = err.code, err.message; error(err, 0) end
    end}"#,
            "second_probe",
        ))
        .await;
}

#[tokio::test]
async fn a_contract_on_another_disabled_definition_still_guards_its_name() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let disabled = k
                .load_def(&src(
                    "disabled-declaration",
                    r#"return {name="disabled-declaration", contracts={disabled_probe={methods={}}}}"#,
                ))
                .unwrap();
            k.set_declared(vec![disabled.declared()]);
            let raw = src(
                "raw-provider",
                r#"return {name="raw-provider", apply=function(ctx)
          local ok, err = pcall(function() ctx:provide("disabled_probe", {}) end)
          if not ok then mare._raw_code = err.code; error(err, 0) end
        end}"#,
            );
            k.set_defs(vec![k.load_def(&raw).unwrap()]);
            k.start().await.unwrap();
            let code: Option<String> = k.lua().load("return mare._raw_code").eval().unwrap();
            assert_eq!(code.as_deref(), Some("invalid_arguments"), "{}", k.problems().join("\n"));
            assert!(!crate::common::plugin_active(&k, "raw-provider"));
            assert!(k.service("disabled_probe").is_none());
        })
        .await;
}

#[tokio::test]
async fn raw_undeclared_provide_remains_reachable() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let raw = src(
                "raw-provider",
                r#"return {name="raw-provider", apply=function(ctx)
          ctx:provide("unlisted_probe", {run=function() return "reachable" end})
        end}"#,
            );
            k.set_defs(vec![k.load_def(&raw).unwrap(), k.load_def(&src("raw-observer", OBSERVER)).unwrap()]);
            k.start().await.unwrap();
            assert!(crate::common::plugin_active(&k, "raw-provider"));
            let value = k.service("unlisted_probe").expect("raw undeclared service is reachable");
            k.lua().globals().set("__raw_service", value).unwrap();
            let (host, other): (String, String) = k
                .lua()
                .load("return __raw_service.run(), mare._raw_observer:get('unlisted_probe').run()")
                .eval()
                .unwrap();
            assert_eq!((host.as_str(), other.as_str()), ("reachable", "reachable"));
        })
        .await;
}

#[tokio::test]
async fn declared_services_work_through_all_mare_service_entry_points() {
    tokio::task::LocalSet::new().run_until(async {
        let k = kernel();
        let package: mlua::Table = k.lua().globals().get("package").unwrap();
        let preload: mlua::Table = package.get("preload").unwrap();
        let mare_lib = k.lua().load(include_str!("../../../../mare-plugin/preset/plugins/lua/mare_lib.lua")).into_function().unwrap();
        preload.set("mare_lib", mare_lib).unwrap();
        let provider = src("allowed-provider", r#"return {name="allowed-provider", provide={"inline_probe", "child_probe", "helper_probe"}, contracts={helper_probe={methods={run={doc="run",result={type="string"}}}}}, apply=function(ctx)
          local cordis = require("cordis")
          local Inline = cordis.class("Inline", mare.Service)
          function Inline:constructor(c) Inline.super.constructor(self, c, "inline_probe", {contract={methods={run={doc="run",result={type="string"}}}}}) end
          function Inline:run() return "inline" end
          Inline.new(ctx)
          local Child = cordis.class("Child", mare.Service)
          function Child:constructor(c) Child.super.constructor(self, c, "child_probe", {contract={methods={run={doc="run",result={type="string"}}}}}) end
          function Child:run() return "child" end
          ctx:plugin(Child)
          require("mare_lib").service(ctx, "helper_probe", {run=function() return "helper" end})
        end}"#);
        k.set_defs(vec![k.load_def(&provider).unwrap()]);
        k.start().await.unwrap();
        for (name, expected) in [("inline_probe", "inline"), ("child_probe", "child"), ("helper_probe", "helper")] {
            let value = k.service(name).unwrap_or_else(|| panic!("{name}: {}", k.problems().join("\n")));
            k.lua().globals().set("__allowed_service", value).unwrap();
            let result: String = k.lua().load("return __allowed_service:run()").eval().unwrap();
            assert_eq!(result, expected);
        }
    }).await;
}

#[tokio::test]
async fn raw_provide_on_root_ctx_under_a_declared_name_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(refuses(
            r#"return {name="raw-provider", provide={"root_raw_probe"}, apply=function(ctx)
      local root_ctx = ctx[require("cordis").Context.root]
      local ok, err = pcall(function() root_ctx:provide("root_raw_probe", {run=function() return "leak" end}) end)
      if not ok then mare._raw_code, mare._raw_message = err.code, err.message; error(err, 0) end
    end}"#,
            "root_raw_probe",
        ))
        .await;
}

#[tokio::test]
async fn ctx_set_cannot_swap_a_declared_service_for_an_unguarded_value() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let provider = src(
                "set-provider",
                r#"return {name="set-provider", provide={"set_probe"}, contracts={set_probe={methods={run={doc="run",result={type="string"}}}}}, apply=function(ctx)
          local cordis = require("cordis")
          local Svc = cordis.class("SetProbe", mare.Service)
          function Svc:constructor(c) Svc.super.constructor(self, c, "set_probe") end
          function Svc:run() return "legit" end
          Svc.new(ctx)
          local ok1, err1 = pcall(function() ctx:set("set_probe", {run=function() return "bypassed" end}) end)
          local ok2, err2 = pcall(function() ctx.set_probe = {run=function() return "bypassed" end} end)
          mare._set_ok1 = ok1
          mare._set_code1 = (not ok1 and type(err1) == "table") and err1.code or nil
          mare._set_ok2 = ok2
          mare._set_code2 = (not ok2 and type(err2) == "table") and err2.code or nil
        end}"#,
            );
            k.set_defs(vec![k.load_def(&provider).unwrap(), k.load_def(&src("raw-observer", OBSERVER)).unwrap()]);
            k.start().await.unwrap();
            let (ok1, code1, ok2, code2, seen): (bool, Option<String>, bool, Option<String>, String) = k
                .lua()
                .load(
                    r#"return mare._set_ok1, mare._set_code1, mare._set_ok2, mare._set_code2, mare._raw_observer:get("set_probe"):run()"#,
                )
                .eval()
                .unwrap();
            assert!(!ok1, "ctx:set must refuse raw replacement of a declared service");
            assert_eq!(code1.as_deref(), Some("invalid_arguments"));
            assert!(!ok2, "ctx.<name> = ... must refuse raw replacement of a declared service");
            assert_eq!(code2.as_deref(), Some("invalid_arguments"));
            assert_eq!(seen, "legit", "admitted mare.Service must remain intact");
        })
        .await;
}

#[tokio::test]
async fn ctx_set_on_undeclared_service_emits_internal_provide_and_updates_value() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let provider = src(
                "unset-provider",
                r#"return {name="unset-provider", apply=function(ctx)
          mare._provide_events = {}
          ctx[require("cordis").Context.root]:on("internal/provide", function(_, name, value)
            if name == "unlisted_set_probe" then
              mare._provide_events[#mare._provide_events + 1] = value.tag
            end
          end, { global = true })
          ctx:provide("unlisted_set_probe", {tag="v1"})
          ctx:set("unlisted_set_probe", {tag="v2"})
        end}"#,
            );
            k.set_defs(vec![k.load_def(&provider).unwrap(), k.load_def(&src("raw-observer", OBSERVER)).unwrap()]);
            k.start().await.unwrap();
            let (tag, count, ev1, ev2): (String, usize, String, String) = k
                .lua()
                .load(
                    r#"return mare._raw_observer:get("unlisted_set_probe").tag, #mare._provide_events, mare._provide_events[1], mare._provide_events[2]"#,
                )
                .eval()
                .unwrap();
            assert_eq!(tag, "v2");
            assert_eq!(count, 2, "expected internal/provide on both provide and set, got {count}");
            assert_eq!((ev1.as_str(), ev2.as_str()), ("v1", "v2"));
        })
        .await;
}

#[tokio::test]
async fn pre_provide_write_on_root_ctx_under_a_declared_name_is_refused() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let writer = src(
                "root-writer",
                r#"return {name="root-writer", contracts={root_write_probe={methods={run={doc="run",result={type="string"}}}}}, apply=function(ctx)
          local root_ctx = ctx[require("cordis").Context.root]
          local ok, err = pcall(function() root_ctx.root_write_probe = {run=function() return "poison" end} end)
          mare._write_ok = ok
          mare._write_code = (not ok and type(err) == "table") and err.code or nil
          mare._root_seen = root_ctx.root_write_probe ~= nil
        end}"#,
            );
            k.set_defs(vec![k.load_def(&writer).unwrap(), k.load_def(&src("raw-observer", OBSERVER)).unwrap()]);
            k.start().await.unwrap();
            let (ok, code, root_seen): (bool, Option<String>, bool) =
                k.lua().load("return mare._write_ok, mare._write_code, mare._root_seen").eval().unwrap();
            assert!(!ok, "root_ctx[declared] = raw before provide must be refused");
            assert_eq!(code.as_deref(), Some("invalid_arguments"));
            assert!(!root_seen, "refused root write must not leave a value on root");
        })
        .await;
}

#[tokio::test]
async fn accessor_and_mixin_cannot_preempt_a_declared_service_name() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            let hijacker = src(
                "acc-hijacker",
                r#"return {name="acc-hijacker", provide={"acc_probe","mix_probe"}, contracts={acc_probe={methods={ping={doc="ping",result={type="string"}}}},mix_probe={methods={ping={doc="ping",result={type="string"}}}}}, apply=function(ctx)
          ctx:provide("mixin_source", { m = function() return "hijacked" end })
          local ok1, err1 = pcall(function() ctx:accessor("acc_probe", { get = function() return { ping = function() return "hijacked" end } end }) end)
          local ok2, err2 = pcall(function() ctx:mixin("mixin_source", { m = "mix_probe" }) end)
          mare._acc_ok1 = ok1
          mare._acc_code1 = (not ok1 and type(err1) == "table") and err1.code or nil
          mare._acc_ok2 = ok2
          mare._acc_code2 = (not ok2 and type(err2) == "table") and err2.code or nil
          local cordis = require("cordis")
          local A = cordis.class("AccProbe", mare.Service)
          function A:constructor(c) A.super.constructor(self, c, "acc_probe") end
          function A:ping() return "legit_acc" end
          A.new(ctx)
          local M = cordis.class("MixProbe", mare.Service)
          function M:constructor(c) M.super.constructor(self, c, "mix_probe") end
          function M:ping() return "legit_mix" end
          M.new(ctx)
        end}"#,
            );
            k.set_defs(vec![k.load_def(&hijacker).unwrap(), k.load_def(&src("raw-observer", OBSERVER)).unwrap()]);
            k.start().await.unwrap();
            let (ok1, c1, ok2, c2): (bool, Option<String>, bool, Option<String>) =
                k.lua().load("return mare._acc_ok1, mare._acc_code1, mare._acc_ok2, mare._acc_code2").eval().unwrap();
            assert!(!ok1, "ctx:accessor on declared service name must be refused (problems: {:?})", k.problems());
            assert_eq!(c1.as_deref(), Some("invalid_arguments"));
            assert!(!ok2, "ctx:mixin alias on declared service name must be refused (problems: {:?})", k.problems());
            assert_eq!(c2.as_deref(), Some("invalid_arguments"));
            let (s1, s2): (String, String) = k
                .lua()
                .load(r#"return mare._raw_observer:get("acc_probe"):ping(), mare._raw_observer:get("mix_probe"):ping()"#)
                .eval()
                .unwrap_or_else(|e| panic!("failed reading services ({e}); problems: {:?}", k.problems()));
            assert_eq!(s1, "legit_acc");
            assert_eq!(s2, "legit_mix");
        })
        .await;
}

#[tokio::test]
async fn reserved_host_builtin_service_names_are_refused_at_load_without_breaking_boot() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for reserved in ["task", "bus", "rpc", "contracts", "loader"] {
                let k = kernel();
                let p_code: &'static str = Box::leak(
                    format!(r#"return {{name="bad-provide", provide={{ "{reserved}" }}, apply=function() end}}"#)
                        .into_boxed_str(),
                );
                let bad_provide = src("bad-provide", p_code);
                let err_p = k
                    .load_def(&bad_provide)
                    .err()
                    .unwrap_or_else(|| panic!("expected load_def refusal for provide=`{reserved}`"));
                assert!(
                    err_p.to_string().contains(reserved),
                    "expected reserved host service error for provide=`{reserved}`, got: {err_p}"
                );
                let c_code: &'static str = Box::leak(
                    format!(
                        r#"return {{name="bad-contract", contracts={{ {reserved} = {{methods={{}}}} }}, apply=function() end}}"#
                    )
                    .into_boxed_str(),
                );
                let bad_contract = src("bad-contract", c_code);
                let err_c = k
                    .load_def(&bad_contract)
                    .err()
                    .unwrap_or_else(|| panic!("expected load_def refusal for contracts=`{reserved}`"));
                assert!(
                    err_c.to_string().contains(reserved),
                    "expected reserved host service error for contracts=`{reserved}`, got: {err_c}"
                );
            }

            // A disabled user plugin declaring `loader` must fail load_def before entering `declared`,
            // so host startup still succeeds cleanly (regression check for disabled plugins).
            let k = kernel();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("evil.lua");
            std::fs::write(&path, r#"return { name = "evil", provide = { "loader" }, apply = function() end }"#)
                .unwrap();
            let bad_src = mare_kernel::PluginSource {
                origin: mare_kernel::Origin::User,
                path: Some(path),
                name_hint: "evil".into(),
                code: None,
            };
            if let Ok(def) = k.load_def(&bad_src) {
                k.set_declared(vec![def.declared()]);
                panic!("disabled plugin declaring reserved host service `loader` must fail at load_def");
            }
            k.set_defs(vec![k.load_def(&src("raw-observer", OBSERVER)).unwrap()]);
            k.start().await.unwrap();
        })
        .await;
}
