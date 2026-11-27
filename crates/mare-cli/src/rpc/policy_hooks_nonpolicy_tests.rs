use super::*;
use crate::rpc::rpc_harness::{HOST_LOCK, TestRpc, rpc_read, rpc_send, test_host};

/// Non-policy RPC callbacks (`emit_checked`, `tool/rewrite`, ordinary `emit`) must fail closed on
/// incompatible `policy` subscriptions and propagate transport/protocol errors instead of returning `nil`.
#[tokio::test]
async fn rpc_non_policy_hooks_fail_closed_on_policy_subscription_and_transport_errors() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocol_version":"3.0","capabilities":{"callbacks":true},"callback_timeout_ms":25}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 {
                    break;
                }
            }

            // 1. Unknown/unlisted event is refused at subscription (`invalid_arguments`).
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"hooks.subscribe","params":{"scope":"all","event":"custom/unlisted","callbacks":["handle"]}}),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    assert_eq!(msg["error"]["code"], json!(codes::INVALID_PARAMS), "{msg}");
                    break;
                }
            }

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            lua.globals().set("np_test_ctx", ctx).unwrap();

            // Exercise the actual Lua `unavailable` error at the subscribe boundary.
            lua.globals().set("np_rpc", host.service_table("rpc").unwrap()).unwrap();
            lua.load(
                r#"np_id, np_saved_ctx = next(np_rpc.contexts)
                   assert(np_id ~= nil)
                   np_orig_get = np_saved_ctx.get
                   np_saved_ctx.get = function(self, n)
                       if n == "policy" then return nil end
                       return np_orig_get(self, n)
                   end"#,
            )
            .exec()
            .unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"hooks.subscribe",
                "params":{"scope":"all","event":"tool/before","callbacks":["handle"]}}),
            )
            .await;
            let unavailable = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 3 {
                    break msg;
                }
            };
            lua.load("np_saved_ctx.get = np_orig_get").exec().unwrap();
            // The protocol currently lists no parallel event; advertise a test-only one via a
            // contract event declaration to exercise the same remote hook path.
            let audit_def = host
                .kernel
                .load_def(&mare_kernel::PluginSource {
                    name_hint: "audit-parallel-probe".into(),
                    path: None,
                    code: Some(
                        r#"return {
                            name = "audit-parallel-probe",
                            contracts = {
                                audit = {
                                    methods = {},
                                    events = {
                                        parallel = {
                                            doc = "Test parallel remote hook.",
                                            mode = "parallel",
                                            expose = "both",
                                            delivery = "required",
                                            ignorable = true,
                                            data = { type = "object" },
                                            example = {},
                                        },
                                    },
                                },
                            },
                        }"#
                        .as_bytes(),
                    ),
                    origin: mare_kernel::Origin::User,
                })
                .unwrap();
            let (_, refused) = host.kernel.merge_defs(vec![audit_def]);
            assert!(refused.is_empty(), "{refused:?}");

            // 2. The protocol's remote hooks (request/prepare serial, tool/rewrite waterfall) propagate
            // a timeout, an RPC error, a malformed reply and a disconnect to emit_checked and
            // tool/rewrite.
            for (idx, event) in ["request/prepare", "audit/parallel", "tool/rewrite"].iter().enumerate() {
                let id = 4 + idx as i64;
                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":id,"method":"hooks.subscribe","params":{"scope":"all","event":event,"callbacks":["handle"]}}),
                )
                .await;
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["id"] == id {
                        assert!(msg.get("error").is_none(), "{msg}");
                        break;
                    }
                }
            }

            // 2a. Timeout (no reply)
            let prep_timeout_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return np_test_ctx:get("bus"):emit_checked("request/prepare", { model = "m" })"#)
                        .exec_async()
                        .await
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "request/prepare");
                    break;
                }
            }
            let prep_timeout = prep_timeout_fut.await.unwrap().unwrap_err();
            assert!(prep_timeout.to_string().contains("timed out"), "{prep_timeout}");

            // 2b. RPC error reply on `tool/rewrite`
            let rw_rpc_err_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"local ok, r = pcall(function() return np_test_ctx:get("policy"):rewrite("tool/rewrite", { name = "bash", arguments = { a = 1 } }) end); assert(not ok); return type(r) == "table" and r.code or nil, type(r) == "table" and r.message or tostring(r)"#)
                        .eval_async::<(Option<String>, Option<String>)>()
                        .await
                        .unwrap()
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "tool/rewrite");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32000,"message":"boom"}}))
                        .await;
                    break;
                }
            }
            let (rw_code, rw_e) = rw_rpc_err_fut.await.unwrap();
            assert_eq!(rw_code.as_deref(), Some("policy.rewrite_refused"), "{rw_e:?}");
            assert!(rw_e.as_deref().is_some_and(|e| e.contains("boom")), "{rw_e:?}");

            // 2c. Malformed reply on `request/prepare`
            let prep_malformed_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return np_test_ctx:get("bus"):emit_checked("request/prepare", { model = "m" })"#)
                        .exec_async()
                        .await
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "request/prepare");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":"malformed"})).await;
                    break;
                }
            }
            let prep_malformed_err = prep_malformed_fut.await.unwrap().unwrap_err();
            assert!(prep_malformed_err.to_string().contains("envelope"), "{prep_malformed_err}");

            // Both modes must reject an array before it is converted into a Lua table.
            let mut array_errors = Vec::new();
            for event in ["request/prepare", "audit/parallel"] {
                let emit = tokio::task::spawn_local({
                    let lua = lua.clone();
                    async move {
                        lua.load(format!(r#"return np_test_ctx:get("bus"):emit_checked("{event}", {{ model = "m" }})"#))
                            .exec_async()
                            .await
                    }
                });
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" {
                        assert_eq!(msg["params"]["arguments"]["event"], event);
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":[]})).await;
                        break;
                    }
                }
                array_errors.push((event, emit.await.unwrap().err().map(|e| e.to_string())));
            }
            assert!(
                unavailable["error"]["data"]["code"] == "unavailable"
                    && array_errors.iter().all(|(_, error)| error.as_deref().is_some_and(|e| e.contains("envelope"))),
                "subscribe error: {unavailable}; array replies: {array_errors:?}"
            );

            // 2e. Disconnect while `callback.call` is in flight propagates error to `emit_checked` and `tool/rewrite`.
            let prep_disc_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return np_test_ctx:get("bus"):emit_checked("request/prepare", { model = "m" })"#)
                        .exec_async()
                        .await
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "request/prepare");
                    break;
                }
            }
            drop(cw);
            drop(cr);
            let prep_disc = prep_disc_fut.await.unwrap().unwrap_err();
            assert!(prep_disc.to_string().contains("disconnected"), "{prep_disc}");

            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

/// Revoking `capabilities.callbacks` via re-initialize fails closed for `emit_checked` and
/// `tool/rewrite`, denies `policy`, and refuses new subscriptions (a withdrawn client's
/// notifications are dropped).
#[tokio::test]
async fn rpc_reinitialize_revoking_hooks_fails_checked_and_rewrite_but_allows_observer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            for (idx, event) in ["request/prepare", "tool/rewrite", "tool/before"].iter().enumerate() {
                let id = 2 + idx as i64;
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "method":"hooks.subscribe",
                        "params":{"scope":"all","event":event,"prepend":true,"callbacks":["handle"]}
                    }),
                )
                .await;
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["id"] == id {
                        assert!(msg.get("error").is_none(), "{msg}");
                        break;
                    }
                }
            }

            // Revoke capabilities.callbacks via connection.set_capabilities.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 5 {
                    assert!(msg.get("error").is_none(), "re-init error: {msg}");
                    break;
                }
            }

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            lua.globals().set("rev_ctx", ctx).unwrap();

            // 1. Checked dispatch (`request/prepare` via `emit_checked`) must fail closed.
            let prep_err = lua
                .load(r#"return rev_ctx:get("bus"):emit_checked("request/prepare", { model = "m" })"#)
                .exec_async()
                .await
                .unwrap_err();
            assert!(prep_err.to_string().contains("callback.call"), "{prep_err}");

            // 2. Rewrite dispatch (`tool/rewrite`) must fail closed rather than keeping original args.
            let (rw_code, rw_err): (Option<String>, Option<String>) = lua
                .load(r#"local ok, r = pcall(function() return rev_ctx:get("policy"):rewrite("tool/rewrite", { name = "bash", arguments = { a = 1 } }) end); assert(not ok); return type(r) == "table" and r.code or nil, type(r) == "table" and r.message or tostring(r)"#)
                .eval_async()
                .await
                .unwrap();
            assert_eq!(rw_code.as_deref(), Some("policy.rewrite_refused"), "{rw_err:?}");
            assert!(
                rw_err.as_deref().is_some_and(|e| e.contains("callback.call")),
                "expected callback.call error in rewrite: {rw_err:?}"
            );

            // 3. Policy dispatch (`tool/before`) must deny.
            let vote: mlua::Table = lua
                .load(r#"return rev_ctx:get("policy"):decide("tool/before", { { name = "bash", arguments = {} } })"#)
                .eval_async()
                .await
                .unwrap();
            assert_eq!(vote.get::<String>("decision").unwrap(), "deny");
            assert!(vote.get::<String>("reason").unwrap().contains("callback.call"));

            // 4. New non-policy subscriptions are also rejected while capabilities.callbacks is false.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":6,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"request/prepare","callbacks":["handle"]}
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 6 {
                    assert!(
                        msg["error"]["message"].as_str().is_some_and(|m| m.contains("callback.call")),
                        "expected callback.call error on non-policy subscribe: {msg}"
                    );
                    break;
                }
            }

            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

/// Revoking `capabilities.callbacks` must fail closed on `waterfall_checked("tool/result", ...)` during
/// real tool execution rather than falling back to the ordinary listener and keeping the unredacted result.
#[tokio::test]
async fn revoked_hooks_capability_fails_real_tool_result_path() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":2,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/result","prepend":true,"callbacks":["handle"]}
                }),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 2 {}

            // Revoke callbacks capability via connection.set_capabilities.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 3 {}

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            let tools = host.service_table("tools").unwrap();
            lua.globals().set("rev_ctx", ctx).unwrap();
            lua.globals().set("rev_tools", tools).unwrap();

            let (is_error, error_code, content): (bool, String, String) = lua
                .load(
                    r#"
                    rev_tools:register("redact-probe", {
                        description = "probe",
                        parameters = { type = "object", properties = {} },
                        side_effect = "read",
                        idempotent = true,
                        execute = function() return { content = "SENSITIVE_SECRET" } end,
                    })
                    local r = rev_tools:execute({ id = "tr-1", name = "redact-probe", arguments = {} }, {})
                    return r.is_error == true, tostring(r.error_code), tostring(r.content)
                    "#,
                )
                .eval_async()
                .await
                .unwrap();

            assert!(is_error, "revoked hooks capability must fail closed on tool/result");
            assert_eq!(error_code, "tool_result_failed");
            assert_eq!(content, "Tool result post-processing failed.");

            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

/// Subscribing to `tool/result` over RPC must project away runtime-only `Token` userdata fields
/// (such as `retention.run_cancel` and `cancel`) by type rather than a name allowlist, so a
/// deferred tool call reaches the remote hook and succeeds without `tool_result_failed`.
#[tokio::test]
async fn remote_tool_result_hook_projects_run_cancel_token_on_defer_tool_call() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session":true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let lua = host.lua();
            let tools = host.service_table("tools").unwrap();
            let llm = host.service_table("llm").unwrap();
            lua.globals().set("defer_tools", tools).unwrap();
            lua.globals().set("defer_llm", llm).unwrap();
            lua.load(
                r#"
                defer_tools:register("defer_probe", {
                    description = "deferred tool probe",
                    parameters = { type = "object", properties = {} },
                    side_effect = "read",
                    idempotent = true,
                    execute = function(_, tctx)
                        assert(tctx.run_cancel ~= nil, "missing run_cancel on defer tool")
                        return { content = "defer-probe-ok" }
                    end,
                })
                local call_n = 0
                defer_llm:register_provider({
                    provider = "defer-prov",
                    is_configured = function() return true end,
                    models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                    stream = function()
                        call_n = call_n + 1
                        if call_n == 1 then
                            return {
                                role = "assistant",
                                stop_reason = "tool_use",
                                content = { { type = "tool_call", id = "c-defer-1", name = "defer_probe", arguments = {} } },
                            }
                        end
                        return {
                            role = "assistant",
                            stop_reason = "stop",
                            content = { { type = "text", text = "defer-done" } },
                        }
                    end,
                })
                defer_llm:set_model("defer-prov/m", { save = false })
                "#,
            )
            .exec_async()
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let h = host.clone();
            let server_task = tokio::task::spawn_local(serve(h, sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":2,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/result","prepend":true,"callbacks":["handle"]}
                }),
            )
            .await;
            let sub_reply = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 2 {
                    break m;
                }
            };
            assert!(sub_reply["error"].is_null(), "{sub_reply}");

            let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
            let responder = tokio::task::spawn_local(async move {
                let mut saw_tool_result = false;
                loop {
                    tokio::select! {
                        _ = &mut done_rx => break,
                        msg = rpc_read(&mut cr) => {
                            if msg.is_null() {
                                break;
                            }
                            if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "tool/result" {
                                saw_tool_result = true;
                                let args = msg["params"]["arguments"]["args"].as_array().unwrap();
                                let retention = &args[0]["operation"];
                                assert!(retention.get("cancel").is_none(), "cancel leaked: {retention}");
                                assert!(retention.get("run_cancel").is_none(), "run_cancel leaked: {retention}");
                                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":args[0]}})).await;
                                break;
                            }
                        }
                    }
                }
                (saw_tool_result, cr, cw)
            });

            let result = host.prompt("run defer", json!({})).await.unwrap();
            let _ = done_tx.send(());
            let (saw_tool_result, _cr, _cw) = responder.await.unwrap();
            assert!(saw_tool_result, "remote tool/result hook never received callback.call: {result}");
            assert!(result["error"].is_null(), "{result}");
            let dump = result.to_string();
            assert!(!dump.contains("tool_result_failed"), "unexpected tool_result_failed: {result}");
            assert!(dump.contains("defer-probe-ok"), "missing defer tool output: {result}");

            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

/// Real `request/build` and fallback requests carry runtime-only fields (`cancel` token and
/// `_rebuild_tool_declarations` function). Both RPC `request/build` hooks (including return-value
/// restoration of runtime fields and `model` identity) and ordinary `tap` must succeed end-to-end.
#[tokio::test]
async fn rpc_request_build_hook_and_tap_handle_primary_and_fallback_requests() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "primary-probe/m",
                    "fallback_models": ["fallback-probe/m"],
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session":true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let mut tap_cursor = host.kernel.open_cursor(Arc::new(|_| Some(true)));

            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":2,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"request/build","prepend":true,"callbacks":["handle"]}
                }),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 2 {}

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            let llm = host.service_table("llm").unwrap();
            lua.globals().set("rb_ctx", ctx).unwrap();
            lua.globals().set("rb_llm", llm).unwrap();

            lua.load(
                r#"
                SEEN_SYSTEMS = {}
                rb_llm:register_provider({
                    provider = "primary-probe",
                    is_configured = function() return true end,
                    models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                    stream = function(req)
                        SEEN_SYSTEMS[#SEEN_SYSTEMS + 1] = "primary:" .. tostring(req.system)
                        error({ message = "quota exhausted", kind = "quota", retryable = false })
                    end,
                })
                rb_llm:register_provider({
                    provider = "fallback-probe",
                    is_configured = function() return true end,
                    models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                    stream = function(req)
                        SEEN_SYSTEMS[#SEEN_SYSTEMS + 1] = "fallback:" .. tostring(req.system)
                        return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "ok-from-fallback" } } }
                    end,
                })
                rb_llm:set_model("primary-probe/m", { save = false })
                "#,
            )
            .exec_async().await
            .unwrap();

            let responder = tokio::task::spawn_local(async move {
                let mut seen_providers = Vec::new();
                while seen_providers.len() < 2 {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "request/build" {
                        let mut req = msg["params"]["arguments"]["args"][0].clone();
                        assert!(req.get("cancel").is_none(), "cancel token must be projected out: {req}");
                        assert!(
                            req.get("_rebuild_tool_declarations").is_none(),
                            "_rebuild_tool_declarations must be projected out: {req}"
                        );
                        let provider = req["model"]["provider"].as_str().unwrap_or("").to_string();
                        req["system"] = json!(format!("rpc-edited-{provider}"));
                        seen_providers.push(provider);
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":req}})).await;
                    }
                }
                (seen_providers, cr, cw)
            });

            let result = host.prompt("hello", json!({})).await.unwrap();
            assert!(result["error"].is_null(), "{result}");
            let systems: Vec<String> = lua.load("return SEEN_SYSTEMS").eval().unwrap();
            assert!(
                host.kernel.problems().is_empty(),
                "unexpected kernel problems (systems={systems:?}): {:?}",
                host.kernel.problems()
            );
            assert_eq!(systems, vec!["primary:rpc-edited-primary-probe", "fallback:rpc-edited-fallback-probe"]);

            let (seen_providers, _cr, _cw) = responder.await.unwrap();
            assert_eq!(seen_providers, vec!["primary-probe", "fallback-probe"]);

            let mut tapped_builds = Vec::new();
            while let Ok(Some(rec)) = tap_cursor.try_recv() {
                if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec
                    && ev.name == "request/build"
                {
                    tapped_builds.push(ev.payload);
                }
            }
            assert_eq!(tapped_builds.len(), 2, "expected primary and fallback request/build tap events");
            assert!(
                !host.kernel.problems().iter().any(|p| p.contains("tap") || p.contains("request/build")),
                "unexpected kernel problems: {:?}",
                host.kernel.problems()
            );

            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

/// Regression: under the unified `hooks` contract and `callback.call`, `service_rpc` binds `registration_id`,
/// `token`, and `handle` before invoking `hooks.subscribe`, and `ConnCallbacks` checks only live
/// connection/capability/token state rather than a post-subscribe `conn.tree_hooks` map. Even if a
/// hook fires after attaching on `policy` while `hooks:subscribe` has not yet returned its RPC reply,
/// the `callback.call` is accepted without an unsubscribed window; after `registrations.release` and
/// any rejected subscribe, no dangling listener remains.
#[tokio::test]
async fn hooks_subscribe_registers_live_hook_before_reply_and_rolls_back_on_error() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session":true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let h = host.clone();
            let server_task = tokio::task::spawn_local(serve(h, sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            let policy_tbl = host.service_table("policy").unwrap();
            lua.globals().set("win_ctx", ctx).unwrap();
            lua.globals().set("win_policy", policy_tbl).unwrap();
            let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let release_rx = std::rc::Rc::new(tokio::sync::Mutex::new(release_rx));
            lua.globals()
                .set("notify_win_sub_entered", lua.create_function(move |_, ()| Ok(entered_tx.send(()).is_ok())).unwrap())
                .unwrap();
            lua.globals()
                .set(
                    "wait_win_sub_release",
                    lua.create_async_function(move |_, ()| {
                        let rx = release_rx.clone();
                        async move {
                            let _ = rx.lock().await.recv().await;
                            Ok(())
                        }
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "cancel_win_sub_request",
                    lua.create_function(|_, ()| {
                        crate::rpc::request_cancel().cancel();
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.load(
                r#"
                WAIT_NEXT_REWRITER = true
                CANCEL_NEXT_REWRITER = false
                local orig_rewriter = win_policy.rewriter
                win_policy.rewriter = function(self, ...)
                    local dispose = orig_rewriter(self, ...)
                    if WAIT_NEXT_REWRITER then
                        WAIT_NEXT_REWRITER = false
                        notify_win_sub_entered()
                        wait_win_sub_release()
                    elseif CANCEL_NEXT_REWRITER then
                        CANCEL_NEXT_REWRITER = false
                        cancel_win_sub_request()
                    end
                    return dispose
                end
                "#,
            )
            .exec()
            .unwrap();

            // `tool/rewrite` rewriter attaches inside `hooks:subscribe`, then yields behind the
            // sync barrier before `hooks:subscribe` returns its disposer or replies to the client.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":2,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/rewrite","prepend":true,"callbacks":["handle"]}
                }),
            )
            .await;

            tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
                .await
                .expect("timed out waiting for rewriter attachment")
                .expect("channel closed");

            let exec_lua = lua.clone();
            let rw_fut = tokio::task::spawn_local(async move {
                exec_lua
                    .load(
                        r#"
                        local ok, r = pcall(function()
                            return win_ctx:get("policy"):rewrite("tool/rewrite", { name = "win-probe", arguments = { step = "initial" } })
                        end)
                        if not ok then return nil, type(r) == "table" and r.message or tostring(r) end
                        return r and r.step, nil
                        "#,
                    )
                    .eval_async::<(Option<String>, Option<String>)>()
                    .await
            });
            tokio::pin!(rw_fut);

            let hook_call = loop {
                tokio::select! {
                    msg = rpc_read(&mut cr) => {
                        if msg["id"] == 2 {
                            panic!("hooks.subscribe replied before sync barrier released: {msg}");
                        }
                        if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "tool/rewrite" {
                            break msg;
                        }
                    }
                    res = &mut rw_fut => {
                        panic!("rw_fut finished before tool/rewrite callback.call: {res:?}");
                    }
                }
            };
            let mut out = hook_call["params"]["arguments"]["args"][0].clone();
            out["arguments"]["step"] = json!("from-in-window-hook");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":hook_call["id"],"result":{"value":out}})).await;

            let (step, reason) = tokio::time::timeout(std::time::Duration::from_secs(5), rw_fut)
                .await
                .expect("timed out waiting for rw_fut")
                .unwrap()
                .unwrap();
            assert!(
                step.as_deref() == Some("from-in-window-hook") && reason.is_none(),
                "hook failed before hooks.subscribe replied: step={step:?}, reason={reason:?}"
            );

            let _ = release_tx.send(());
            let sub_reply = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    break msg;
                }
            };
            assert!(sub_reply["error"].is_null(), "{sub_reply}");
            let reg_id = sub_reply["result"]["registration_id"].clone();
            assert!(reg_id.is_string(), "{sub_reply}");

            // Release the registration, then cancel a second `tool/rewrite` subscribe AFTER
            // `orig_rewriter` has already attached (`release_undelivered_registration` rollback).
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"registrations.release","params":{"registration_id":reg_id}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 3 {}

            lua.load("CANCEL_NEXT_REWRITER = true").exec().unwrap();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":4,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/rewrite","prepend":true,"callbacks":["handle"]}
                }),
            )
            .await;
            let fail_reply = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 4 {
                    break msg;
                }
            };
            assert!(!fail_reply["error"].is_null(), "expected post-attach cancelled subscribe error: {fail_reply}");

            let (after_step, after_reason): (Option<String>, Option<String>) = lua
                .load(
                    r#"
                    local ok, r = pcall(function()
                        return win_ctx:get("policy"):rewrite("tool/rewrite", { name = "win-probe", arguments = { step = "after-rb" } })
                    end)
                    if not ok then return nil, type(r) == "table" and r.message or tostring(r) end
                    return r and r.step, nil
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert_eq!(after_step.as_deref(), Some("after-rb"));
            assert_eq!(after_reason, None);

            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}
