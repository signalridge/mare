//! model, auth, capability discovery, scoped selection, and usage/rate-limit JSON exports.
use crate::common::*;
use serde_json::{Value, json};

fn rpc_login_client(root: &std::path::Path) -> crate::common::rpc::RpcClient {
    rpc_login_client_with_key(root, None)
}

/// `openai_key`: `None` removes OPENAI_API_KEY from the child's environment, `Some(v)` sets it.
fn rpc_login_client_with_key(root: &std::path::Path, openai_key: Option<&str>) -> crate::common::rpc::RpcClient {
    use std::collections::BTreeMap;

    let home = root.join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    let script = root.join("faux.json");
    std::fs::write(&script, "[]").unwrap();
    let mut env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let removed: &[&str] = match openai_key {
        Some(v) => {
            env.insert("OPENAI_API_KEY".to_string(), v.to_string());
            &[]
        }
        None => &["OPENAI_API_KEY"],
    };
    crate::common::rpc::RpcClient::spawn_with_env_removed(
        &["--rpc", "--peer", "--yolo", "--model", "faux/echo"],
        &env,
        root,
        removed,
    )
}

fn rpc_openai_provider(client: &mut crate::common::rpc::RpcClient) -> Value {
    let providers = client.call("llm.list_providers", json!({}));
    providers["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["provider"] == "openai")
        .expect("openai provider")
        .clone()
}

#[test]
fn rpc_login_without_a_ui_or_key_is_refused_with_needs_ui() {
    let root = tempfile::tempdir().unwrap();
    let mut client = rpc_login_client(root.path());
    client.call("initialize", json!({"protocol_version": "3.0"}));

    let error = client.call_err("llm.login", json!({ "provider": "openai", "method": "api_key" }));
    assert_eq!(error["data"]["code"], "llm.needs_ui", "{error}");
    assert_eq!(rpc_openai_provider(&mut client)["logged_in"], false);
    assert!(!root.path().join("home/config/auth.json").exists(), "refused login wrote auth.json");
    client.shutdown_and_wait();
}

/// An empty OPENAI_API_KEY is unset (api_key ignores it too; Codex login/src/auth/manager.rs:1053-1057
/// drops empty values): openai is not configured and a key-less login needs a UI.
#[test]
fn rpc_empty_openai_api_key_is_not_configured_and_login_needs_ui() {
    let root = tempfile::tempdir().unwrap();
    let mut client = rpc_login_client_with_key(root.path(), Some(""));
    client.call("initialize", json!({"protocol_version": "3.0"}));

    assert_eq!(rpc_openai_provider(&mut client)["logged_in"], false, "an empty key counts as configured");
    let error = client.call_err("llm.login", json!({ "provider": "openai", "method": "api_key" }));
    assert_eq!(error["data"]["code"], "llm.needs_ui", "{error}");
    assert!(!root.path().join("home/config/auth.json").exists(), "refused login wrote auth.json");
    client.shutdown_and_wait();
}

#[test]
fn rpc_login_prompts_for_the_key_through_ui_input() {
    use std::sync::{Arc, Mutex};

    const KEY: &str = "sk-test-ui-secret-1234567890";
    let root = tempfile::tempdir().unwrap();
    let mut client = rpc_login_client(root.path());
    let prompts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = prompts.clone();
    client.on_method = Some(Box::new(move |client, msg| {
        if msg["method"] == "ui.input" {
            seen.lock().unwrap().push(msg["params"].clone());
            client.send(json!({ "id": msg["id"], "result": { "text": KEY } }));
            return true;
        }
        false
    }));
    client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "ui": true } }));

    assert_eq!(client.call("llm.login", json!({ "provider": "openai", "method": "api_key" })), json!({}));
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0]["title"].as_str().unwrap().to_lowercase().contains("openai"), "{}", prompts[0]);
    assert_eq!(prompts[0]["secret"], true, "{}", prompts[0]);

    let providers = client.call("llm.list_providers", json!({}));
    assert!(!providers.to_string().contains(KEY), "provider listing leaked the key: {providers}");
    let openai = providers["data"].as_array().unwrap().iter().find(|p| p["provider"] == "openai").unwrap();
    assert_eq!(openai["logged_in"], true, "{openai}");
    assert_eq!(openai["account"], "sk-…7890", "{openai}");

    let auth_path = root.path().join("home/config/auth.json");
    let auth: Value = serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
    assert_eq!(auth["openai"]["key"], KEY);
    let logout = client.call("llm.logout", json!({ "provider": "openai" }));
    assert_eq!(logout["removed"], json!(["openai"]), "{logout}");
    let auth: Value = serde_json::from_slice(&std::fs::read(auth_path).unwrap()).unwrap();
    assert!(auth.get("openai").is_none(), "logout left an openai credential");
    assert_eq!(rpc_openai_provider(&mut client)["logged_in"], false);
    client.shutdown_and_wait();
}

#[test]
fn rpc_login_cancelled_in_the_ui_leaves_no_credential() {
    use std::sync::{Arc, Mutex};

    let root = tempfile::tempdir().unwrap();
    let mut client = rpc_login_client(root.path());
    let prompts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = prompts.clone();
    client.on_method = Some(Box::new(move |client, msg| {
        if msg["method"] == "ui.input" {
            seen.lock().unwrap().push(msg["params"].clone());
            client.send(json!({ "id": msg["id"], "result": { "text": null } }));
            return true;
        }
        false
    }));
    client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "ui": true } }));

    let error = client.call_err("llm.login", json!({ "provider": "openai", "method": "api_key" }));
    assert_eq!(error["data"]["code"], "llm.login_failed", "{error}");
    assert_eq!(prompts.lock().unwrap().len(), 1, "login must reach the UI prompt");
    assert_eq!(rpc_openai_provider(&mut client)["logged_in"], false);
    assert!(!root.path().join("home/config/auth.json").exists(), "cancelled login wrote auth.json");
    client.shutdown_and_wait();
}

#[tokio::test]
async fn llm_and_usage_contracts_expose_only_remote_methods() {
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().builtins(&["usage"]).faux(json!([])).build().await;
        expose(&host);

        let exports = host.kernel.service_exports();
        let llm = exports.iter().find(|e| e["name"] == "llm").expect("llm service exported");
        let llm_methods = llm["methods"].as_object().expect("llm methods object");
        let names: Vec<&str> = llm_methods.keys().map(String::as_str).collect();
        // only the contract's remote methods; lua-only methods (auth_get, auth_set, stream …) never are
        assert_eq!(
            names,
            [
                "get_model",
                "list_auth_keys",
                "list_models",
                "list_providers",
                "login",
                "logout",
                "register_provider",
                "set_effort",
                "set_model"
            ]
        );
        assert_eq!(llm_methods["login"]["access"], "primary", "credentials are never grantable to a peer");
        assert_eq!(llm_methods["set_model"]["scope"], "session");

        let usage = exports.iter().find(|e| e["name"] == "usage").expect("usage service exported");
        let usage_methods = usage["methods"].as_object().expect("usage methods object");
        assert_eq!(usage_methods.keys().collect::<Vec<_>>(), ["get"]);
        assert!(usage_methods["get"]["parameters"]["required"].as_array().unwrap().contains(&json!("session_id")));

        host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn capabilities_detach_param_functions_and_report_unavailable_models() {
    local(async {
        let env = TestEnv::new().await;
        let host = env
            .host().builtins(&["usage"])
            .config(json!({
                "model": "probe/ready-model",
                "plugins": { "provider-faux": false }
            }))
            .build()
            .await;
        expose(&host);
        global(&host, "LLM", host.service_table("llm").unwrap());

        exec(
            &host,
            r#"
            LLM:register_provider({
                provider = "probe",
                label = "Probe",
                no_key = true,
                models = {
                    {
                        model_id = "ready-model",
                        name = "Ready Model",
                        context_window = 64000,
                        max_output = 4096,
                        reasoning = true,
                        input = { "text" },
                        efforts = { "low", "custom_max" },
                    },
                    {
                        model_id = "down-model",
                        name = "Down Model",
                        unavailable = "maintenance window",
                    },
                    { model_id = "secret-model", name = "Secret Model", hidden = true, reasoning = true },
                },
                stream = function() return { role = "assistant", content = {}, stop_reason = "stop" } end,
            })
            LLM:define_params({
                provider = "probe",
                define = {
                    effort = {
                        options = {
                            { value = "low", name = "Low" },
                            { value = "custom_max", name = "Custom Max", wire = { chat = function() end } },
                        },
                        only = { "low", "custom_max" },
                        default = "custom_max",
                        cycleable = true,
                        wire = { chat = function() end },
                    },
                    verbosity = {
                        type = "enum",
                        options = { "terse", "full" },
                        default = "full",
                        cycleable = false,
                        wire = { chat = { verbosity = "$value" } },
                    },
                },
            })
            "#,
        )
        .await;

        // by ref through the Lua contract proxy, with no change of selection (the RPC path is in get_model_by_ref_over_real_rpc)
        let by_ref = |r: &str| format!(r#"return mare.json.encode(C.cordis:get("llm"):get_model("{r}"))"#);
        let cap_json: String = eval(&host, &by_ref("probe/ready-model")).await;
        let cap: Value = serde_json::from_str(&cap_json).unwrap();
        assert_eq!(cap["ref"], "probe/ready-model");
        assert_eq!(cap["model_id"], "ready-model");
        assert_eq!(cap["available"], true);
        assert_eq!(cap["ready"], true);
        assert_eq!(cap["reasoning"], true);
        assert_eq!(cap["images"], false);
        assert!(cap.get("video").is_none(), "model descriptor must omit video: {cap}");
        assert!(cap.get("realtime").is_none(), "model descriptor must omit realtime: {cap}");
        assert_eq!(cap["default_effort"], "custom_max");
        assert_eq!(cap["efforts"], json!(["low", "custom_max"]));
        let params = cap["parameters"].as_array().unwrap();
        assert_eq!(params[0]["parameter_id"], "effort");
        assert_eq!(params[0]["values"], json!(["low", "custom_max"]));
        let param = |id: &str| params.iter().find(|p| p["parameter_id"] == id).unwrap().clone();
        // false is a value, not "unset": both values survive, on the RPC path and in Lua
        assert_eq!(param("effort")["cycleable"], true, "{cap}");
        assert_eq!(param("verbosity")["cycleable"], false, "{cap}");
        let lua_cycleable: (bool, bool) = eval(
            &host,
            r#"local d = LLM:capabilities("probe/ready-model")
               local by = {}
               for _, p in ipairs(d.parameters) do by[p.parameter_id] = p end
               -- strict: a missing value (nil) must not pass as false
               return by.effort.cycleable == true, by.verbosity.cycleable == false"#,
        )
        .await;
        assert_eq!(lua_cycleable, (true, true));

        let down_json: String = eval(&host, &by_ref("probe/down-model")).await;
        let down: Value = serde_json::from_str(&down_json).unwrap();
        assert_eq!(down["available"], false);
        assert_eq!(down["unavailable_reason"], "maintenance window");
        assert_eq!(down["ready"], false);

        // a hidden model is not listed but can be described; a catalogued provider rejects unlisted ids
        // while a catalog-less provider serves arbitrary ids
        let listed: String = eval(
            &host,
            r#"local refs = {} for _, m in ipairs(C.cordis:get("llm"):list_models().data) do refs[#refs + 1] = m.ref end return table.concat(refs, ",")"#,
        )
        .await;
        assert!(!listed.contains("probe/secret-model") && !listed.contains("probe/unlisted"), "{listed}");
        let hidden: Value = serde_json::from_str(&eval::<String>(&host, &by_ref("probe/secret-model")).await).unwrap();
        assert_eq!((hidden["ref"].clone(), hidden["reasoning"].clone()), (json!("probe/secret-model"), json!(true)), "{hidden}");
        let unlisted_rejected: bool =
            eval(&host, r#"return pcall(function() return C.cordis:get("llm"):get_model("probe/unlisted-1") end)"#).await;
        assert!(!unlisted_rejected, "catalogued provider must reject unlisted model id");
        exec(
            &host,
            r#"LLM:register_provider({ provider = "free", no_key = true, stream = function() return { role = "assistant", content = {}, stop_reason = "stop" } end })"#,
        )
        .await;
        let unlisted: Value = serde_json::from_str(&eval::<String>(&host, &by_ref("free/unlisted-1")).await).unwrap();
        assert_eq!((unlisted["ref"].clone(), unlisted["provider"].clone()), (json!("free/unlisted-1"), json!("free")), "{unlisted}");
        let unknown_described: bool =
            eval(&host, r#"return pcall(function() return C.cordis:get("llm"):get_model("nonexistent/missing") end)"#).await;
        assert!(!unknown_described, "a ref no provider resolves is an error");
        let current: String = eval(&host, r#"return C.cordis:get("llm"):get_model().ref"#).await;
        assert_eq!(current, "probe/ready-model", "describing models never changes the selection");

        let unknown_ok: bool =
            eval(&host, r#"return pcall(function() return C.cordis:get("llm"):set_model("nonexistent/missing") end)"#)
                .await;
        assert!(!unknown_ok, "unknown model ref must fail");

        host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn scoped_model_and_effort_selection_validate_and_preserve_global_defaults() {
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().builtins(&["usage"]).config(json!({ "model": "probe/m1" })).build().await;
        expose(&host);
        global(&host, "LLM", host.service_table("llm").unwrap());

        exec(
            &host,
            r#"
            LLM:register_provider({
                provider = "probe",
                no_key = true,
                models = {
                    { model_id = "m1", reasoning = true, efforts = { "low", "medium", "high" } },
                    { model_id = "m2", reasoning = true, efforts = { "low", "medium", "high" } },
                },
                stream = function() return { role = "assistant", content = {}, stop_reason = "stop" } end,
            })
            -- through the Lua proxy, as plugins call it (the contract checks the options)
            local P = C.cordis:get("llm")
            P:set_model("probe/m1", { target = "global" })
            P:set_effort("high", { target = "global" })
            P:set_effort("low", { target = "session", scope_name = "plan" })
            assert(P:scoped_effort("plan") == "low")

            -- Invalid targets / conflicting flags must fail.
            local ok, err = pcall(P.set_model, P, "probe/m2", { target = "invalid" })
            assert(not ok and err.code == "invalid_arguments", tostring(err))
            assert(not pcall(P.set_model, P, "probe/m2", { target = "session", save = true }))
            assert(not pcall(P.set_effort, P, "low", { target = "invalid" }))
            assert(not pcall(P.set_effort, P, "low", { target = "session", save = true }))
            assert(not pcall(P.set_model, P, "probe/m2", { restoring = true }), "restoration flags are spi only")

            -- Session-scoped model & effort through the contract do not overwrite state.json defaults.
            local m_res = C.cordis:get("llm"):set_model("probe/m2", { target = "session" })
            assert(m_res.ref == "probe/m2" and m_res.model_id == "m2" and m_res.provider == "probe")
            assert(next(C.cordis:get("llm"):set_effort("low", { target = "session" })) == nil)
            assert(LLM:effort() == "low")
            assert(C.cordis:get("llm"):get_model().ref == "probe/m2")
            "#,
        )
        .await;

        let (state_model, state_m2_effort, current_ref): (Option<String>, Option<String>, String) = eval(
            &host,
            r#"
            local st = mare.state.read()
            local m2_eff = st.model_params and st.model_params["probe/m2"] and st.model_params["probe/m2"].effort
            return st.model, m2_eff, LLM:current().ref
            "#,
        )
        .await;
        assert_eq!(state_model.as_deref(), Some("probe/m1"));
        assert_eq!(state_m2_effort, None);
        assert_eq!(current_ref, "probe/m2");

        // Global target updates state.json.
        exec(
            &host,
            r#"
            local m_res = C.cordis:get("llm"):set_model("probe/m2", { target = "global" })
            assert(m_res.ref == "probe/m2")
            C.cordis:get("llm"):set_effort("low", { target = "global" })
            assert(LLM:effort() == "low")
            "#,
        )
        .await;
        let (state_model2, state_m2_effort2): (Option<String>, Option<String>) = eval(
            &host,
            r#"
            local st = mare.state.read()
            local m2_eff = st.model_params and st.model_params["probe/m2"] and st.model_params["probe/m2"].effort
            return st.model, m2_eff
            "#,
        )
        .await;
        assert_eq!(state_model2.as_deref(), Some("probe/m2"));
        assert_eq!(state_m2_effort2.as_deref(), Some("low"));

        host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn providers_login_logout_and_usage_through_contracts() {
    local(async {
        let env = TestEnv::new().await;
        let host = env.host().builtins(&["usage"]).faux(json!([])).build().await;
        expose(&host);
        global(&host, "LLM", host.service_table("llm").unwrap());

        exec(
            &host,
            r#"
            local fake_secret = "sk-fake-secret-9876543210"
            LLM:register_provider({
                provider = "fakeauth",
                label = "Fake Auth",
                models = { { model_id = "m1" } },
                is_configured = function()
                    return LLM:auth_get("fakeauth") ~= nil
                end,
                login = function(io)
                    LLM:login_api_key("fakeauth", "Fake API key", io)
                end,
                stream = function() return { role = "assistant", content = {}, stop_reason = "stop" } end,
            })

            local function status()
                for _, p in ipairs(C.cordis:get("llm"):list_providers().data) do
                    if p.provider == "fakeauth" then return p end
                end
            end
            local st0 = mare.json.encode(status())
            assert(not st0:find(fake_secret, 1, true))

            local login_res = C.cordis:get("llm"):login("fakeauth", { method = "api_key", key = fake_secret })
            assert(next(login_res) == nil)
            local st1 = status()
            assert(st1.logged_in == true and st1.account == "sk-…3210")
            local st1_json = mare.json.encode(C.cordis:get("llm"):list_providers())
            assert(not st1_json:find(fake_secret, 1, true), "list_providers leaked secret")

            -- Lua contract proxy check: interactive login without key in RPC mode without capabilities.ui fails closed.
            local prev_mode = mare.mode
            mare.mode = "rpc"
            local ok_rpc, err_rpc = pcall(function() return C.cordis:get("llm"):login("fakeauth") end)
            mare.mode = prev_mode
            assert(not ok_rpc and tostring(err_rpc):find("connected UI", 1, true), tostring(err_rpc))

            local out = C.cordis:get("llm"):logout("fakeauth")
            assert(#out.removed == 1 and out.removed[1] == "fakeauth")
            assert(status().logged_in == false)

            -- Lua callers keep passing their own I/O functions through the proxy
            local P = C.cordis:get("llm")
            local said = {}
            P:login("fakeauth", { method = "api_key", key = fake_secret, say = function(t) said[#said + 1] = t end })
            assert(status().logged_in == true)
            P:logout("fakeauth")

            -- A pasted key must be masked and identify the provider; prefixes alone can be ambiguous.
            local hints, secret = {}, false
            P:login("fakeauth", { method = "api_key", say = function(t) hints[#hints + 1] = t end,
                ask = function(title, opts) secret = opts.secret == true; return fake_secret end })
            local hint = table.concat(hints, " ")
            assert(secret and hint:find("fakeauth", 1, true) and hint:find("prefix", 1, true), hint)
            assert(not hint:find(fake_secret, 1, true), "key leaked in login hint")
            P:logout("fakeauth")
            local isolated = {auth_set=function() error("injected storage error " .. fake_secret, 0) end,
                invalidate_api_key=function() end}
            require('llm.login')({ctx={on=function() end}, providers={}, llm=isolated,
                auth_store={read_auth=function() end}, api_keys={key_cache={}, known_envs={}},
                credentials={auth_keys=function() end}, all_providers=function() return {} end})
            local stored, store_err = pcall(isolated.login_api_key, isolated, nil, "fakeauth", "Fake API key", { key = fake_secret })
            assert(not stored and tostring(store_err):find("injected storage error", 1, true), "fixture must reach storage")
            assert(not tostring(store_err):find(fake_secret, 1, true), "key leaked in login error")

            -- Lua contract proxy check: usage & rate_limits reflect session-owned usage state.
            C.cordis:get("bus"):emit("llm/rate-limits", {
                provider = "fakeauth",
                limits = { primary = { used_percent = 42, window_minutes = 60 }, credits = "unlimited" },
            })
            local usage = C.cordis:get("usage"):get()
            assert(usage.rate_limits.fakeauth.primary.used_percent == 42)
            assert(usage.rate_limits.fakeauth.credits == "unlimited")
            assert(usage.totals.requests == 0 and usage.context_tokens == 0)
            "#,
        )
        .await;

        // JSON export entry point (`Kernel::call_service_export`): quota return value arrives with
        // the exact contract wire shape without touching Lua callers.
        let owner = host.kernel.open_owner("<root>", "root").unwrap();
        let usage_json = host
            .kernel
            .call_service_export(owner, None, "usage", "get", json!({}), tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(usage_json["rate_limits"]["fakeauth"]["primary"]["used_percent"], 42, "{usage_json}");
        assert_eq!(usage_json["rate_limits"]["fakeauth"]["credits"], "unlimited", "{usage_json}");
        assert_eq!(usage_json["totals"]["requests"], 0, "{usage_json}");
        assert_eq!(usage_json["context_tokens"], 0, "{usage_json}");

        host.shutdown().await;
    })
    .await
}

/// The same queries over a real RPC connection (`mare --rpc`): `llm.get_model` with the session's
/// `session_id` and a `ref` describes hidden and unlisted models without changing the selection,
/// and `cycleable` arrives as true and as false.
#[test]
fn get_model_by_ref_over_real_rpc() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    std::fs::write(
        home.path().join("config/plugins/probe.lua"),
        r#"
        return { name = "probe", inject = { "llm" }, apply = function(ctx)
            ctx.llm:register_provider({
                provider = "probe", no_key = true,
                models = {
                    { model_id = "ready-model", context_window = 64000, max_output = 4096 },
                    { model_id = "selected-model", context_window = 128000, max_output = 4096 },
                    { model_id = "secret-model", hidden = true, reasoning = true },
                },
                stream = function() return { role = "assistant", content = {}, stop_reason = "stop" } end,
            })
            ctx.llm:define_params({ provider = "probe", define = {
                mode = { type = "enum", options = { "a", "b" }, default = "a", cycleable = true },
                verbosity = { type = "enum", options = { "terse", "full" }, default = "full", cycleable = false },
            } })
        end }
        "#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "probe/ready-model"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let session = init["result"]["session"]["session_id"].clone();
    assert!(session.is_string(), "{init}");
    let get = |rpc: &mut RpcClient, params: Value| {
        let r = rpc.call_raw("llm.get_model", params, |_| json!(null));
        assert!(r["error"].is_null(), "{r}");
        r["result"].clone()
    };
    let ready = get(&mut rpc, json!({ "session_id": session, "ref": "probe/ready-model" }));
    assert_eq!(ready["model_id"], "ready-model", "{ready}");
    let param = |d: &Value, id: &str| {
        d["parameters"].as_array().unwrap().iter().find(|p| p["parameter_id"] == id).unwrap().clone()
    };
    assert_eq!(param(&ready, "mode")["cycleable"], true, "{ready}");
    assert_eq!(param(&ready, "verbosity")["cycleable"], false, "{ready}");
    let hidden = get(&mut rpc, json!({ "session_id": session, "ref": "probe/secret-model" }));
    assert_eq!(
        (hidden["ref"].clone(), hidden["model_id"].clone(), hidden["reasoning"].clone()),
        (json!("probe/secret-model"), json!("secret-model"), json!(true)),
        "{hidden}"
    );
    let unlisted =
        rpc.call_raw("llm.get_model", json!({ "session_id": session, "ref": "probe/unlisted-1" }), |_| json!(null));
    assert_eq!(unlisted["error"]["data"]["code"], "llm.unknown_model", "{unlisted}");
    let listed = rpc.call_raw("llm.list_models", json!({}), |_| json!(null));
    let refs: Vec<&str> =
        listed["result"]["data"].as_array().unwrap().iter().filter_map(|m| m["ref"].as_str()).collect();
    assert!(!refs.contains(&"probe/secret-model") && !refs.contains(&"probe/unlisted-1"), "{refs:?}");
    let unknown =
        rpc.call_raw("llm.get_model", json!({ "session_id": session, "ref": "nowhere/missing" }), |_| json!(null));
    assert_eq!(unknown["error"]["data"]["code"], "llm.unknown_model", "{unknown}");
    assert_eq!(get(&mut rpc, json!({ "session_id": session }))["ref"], "probe/ready-model", "selection unchanged");
    // Retired option keys are rejected at the RPC boundary, before changing the selection.
    for (method, params) in [
        ("llm.set_model", json!({ "session_id": session, "ref": "probe/selected-model", "scope": "session" })),
        ("llm.set_effort", json!({ "session_id": session, "effort": "high", "persistence": "session" })),
        ("llm.set_effort", json!({ "session_id": session, "effort": "high", "scope": "session" })),
    ] {
        let rejected = rpc.call_raw(method, params, |_| json!(null));
        assert_eq!(rejected["error"]["code"], -32602, "{method}: {rejected}");
        assert_eq!(rejected["error"]["data"]["code"], "invalid_arguments", "{method}: {rejected}");
    }
    assert_eq!(get(&mut rpc, json!({ "session_id": session }))["ref"], "probe/ready-model");
    let selected = rpc.call_raw(
        "llm.set_model",
        json!({ "session_id": session, "ref": "probe/selected-model", "target": "session" }),
        |_| json!(null),
    );
    assert!(selected["error"].is_null(), "{selected}");
    assert_eq!(selected["result"]["ref"], "probe/selected-model", "{selected}");
    assert_eq!(get(&mut rpc, json!({ "session_id": session }))["ref"], "probe/selected-model");

    let shutdown = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    assert!(shutdown["error"].is_null(), "{shutdown}");
    rpc.wait_exit();
}

/// Real RPC (`mare --rpc`, no `capabilities.ui`): `llm.login` without an explicit key fails closed
/// with `needs_ui`, and `usage.get` returns the session's rate limits and totals through the JSON
/// adapter.
#[test]
fn headless_login_and_usage_over_real_rpc() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    std::fs::write(
        home.path().join("config/plugins/probe.lua"),
        r#"
        return { name = "probe", inject = { "llm", "usage" }, apply = function(ctx)
            ctx.llm:register_provider({
                provider = "fakeauth",
                label = "Fake Auth",
                models = { { model_id = "m1" } },
                is_configured = function() return ctx.llm:auth_get("fakeauth") ~= nil end,
                login = function(io) ctx.llm:login_api_key("fakeauth", "Fake API key", io) end,
                stream = function() return { role = "assistant", content = {}, stop_reason = "stop" } end,
            })
        end }
        "#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let session = init["result"]["session"]["session_id"].clone();
    assert!(session.is_string(), "{init}");

    let login = rpc.call_raw("llm.login", json!({ "provider": "fakeauth" }), |_| json!(null));
    assert!(login["error"].is_object(), "headless rpc login must fail without UI: {login}");
    let err_msg = login["error"]["message"].as_str().unwrap_or("");
    assert!(err_msg.contains("connected UI"), "{login}");

    let usage = rpc.call_raw("usage.get", json!({ "session_id": session }), |_| json!(null));
    assert!(usage["error"].is_null(), "{usage}");
    let res = &usage["result"];
    assert_eq!(res["totals"]["requests"], 0, "{usage}");
    assert_eq!(res["context_tokens"], 0, "{usage}");

    let shutdown = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    assert!(shutdown["error"].is_null(), "{shutdown}");
    rpc.wait_exit();

    // Also verify `rate_limits` projection over JSON-RPC when `llm/rate-limits` arrives from root.
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(local(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["usage"]).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("llm/rate-limits", {
                provider = "fakeauth",
                limits = { primary = { used_percent = 42, window_minutes = 60 }, credits = "unlimited" },
            })"#,
        )
        .await;
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (srv_r, srv_w) = tokio::io::split(server_io);
        let (cli_r, mut cli_w) = tokio::io::split(client_io);
        let stop = tokio_util::sync::CancellationToken::new();
        let stop_c = stop.clone();
        let srv_task = tokio::task::spawn_local(mare_cli::rpc::serve(h.clone(), srv_r, srv_w, true, async move {
            stop_c.cancelled().await;
        }));
        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        let mut next_id = 0u64;
        let mut call = async |method: &str, params: Value| -> Value {
            next_id += 1;
            let id = next_id;
            let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            cli_w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
            cli_w.flush().await.unwrap();
            loop {
                let line = lines.next_line().await.unwrap().unwrap();
                let msg: Value = serde_json::from_str(&line).unwrap();
                if msg["id"] == id {
                    return msg;
                }
            }
        };
        let init = call("initialize", json!({"protocol_version": "3.0"})).await;
        let sid = init["result"]["session"]["session_id"].clone();
        let usage = call("usage.get", json!({ "session_id": sid })).await;
        assert!(usage["error"].is_null(), "{usage}");
        assert_eq!(usage["result"]["rate_limits"]["fakeauth"]["primary"]["used_percent"], 42, "{usage}");
        assert_eq!(usage["result"]["rate_limits"]["fakeauth"]["credits"], "unlimited", "{usage}");
        call("shutdown", json!({})).await;
        drop((cli_w, lines));
        stop.cancel();
        let _ = srv_task.await;
        h.shutdown().await;
    }));
}
