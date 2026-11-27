use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};

// Host installs a process-wide network approver; concurrent hosts would steal it.
pub(in crate::rpc) use crate::HOST_TEST_LOCK as HOST_LOCK;

pub(in crate::rpc) async fn rpc_send(writer: &mut (impl AsyncWrite + Unpin), value: J) {
    writer.write_all(format!("{value}\n").as_bytes()).await.unwrap();
}

pub(in crate::rpc) async fn rpc_read(reader: &mut (impl AsyncBufReadExt + Unpin)) -> J {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut line)).await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Exercise network authorization -> host approver -> policy -> hook.call over the real RPC server.
#[tokio::test]
async fn rpc_network_approval_requires_an_explicit_affirmative_answer() {
    tokio::task::LocalSet::new().run_until(async {
        let _host_guard = HOST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let host = Host::new(mare_host::HostOptions {
            cwd: dir.path().to_owned(), mode: mare_plugin::Mode::Print,
            config: json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
            load_init: false, project_trusted: false, args: json!({"no_session":true}), no_user_plugins: true,
        }).await.unwrap();
        let (client, server) = tokio::io::duplex(65536);
        let (sr, sw) = tokio::io::split(server);
        let h = host.clone();
        let server_task = tokio::task::spawn_local(serve(h, sr, sw, true, std::future::pending()));
        let (cr, mut cw) = tokio::io::split(client);
        let mut cr = BufReader::new(cr);
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":false},"callback_timeout_ms":100}})).await;
        loop { if rpc_read(&mut cr).await["id"] == 1 { break; } }
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"hooks.subscribe","params":{"scope":"all","event":"net/approval","callbacks":["handle"]}})).await;
        loop {
            let msg = rpc_read(&mut cr).await;
            if msg["id"] == 2 { assert!(msg.get("error").is_some(), "{msg}"); break; }
        }
        let approver = mare_binding::netauth::approver(host.lua());
        assert_eq!(host.kernel.listener_count("net/approval"), 0);
        assert!(mare_binding::netauth::NetPolicy::deny_all().authorize("8.8.8.8", 4000, approver.as_ref()).await.is_err());
        // A client may update capabilities via connection.set_capabilities; a remote participant in the decision is a voter
        // (`mode = "policy"` on the decision point).
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}})).await;
        loop { if rpc_read(&mut cr).await["id"] == 3 { break; } }
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"hooks.subscribe","params":{"scope":"all","event":"net/approval","callbacks":["handle"]}})).await;
        loop {
            let msg = rpc_read(&mut cr).await;
            if msg["id"] == 4 { assert!(msg.get("error").is_none(), "{msg}"); break; }
        }
        for (case, (reply, expected)) in [
            (Some(json!({})), "deny"),
            (Some(json!({"value":null})), "deny"),
            (Some(json!({"value":{"decision":"maybe"}})), "deny"),
            (Some(json!({"value":false})), "deny"),
            (Some(json!({"value":{"decision":"deny"}})), "deny"),
            (Some(json!({"value":{"decision":"ask"}})), "ask"),
            (None, "deny"),
            (Some(json!({"value":{"decision":"allow","grant":"allow_session"}})), "allow"),
            (Some(json!({"value":"allow"})), "allow"),
        ].into_iter().enumerate() {
            let port = 4100 + case as u16;
            let ap = approver.clone();
            let vote = tokio::task::spawn_local(async move {
                mare_binding::netauth::NetPolicy::deny_all().authorize("8.8.8.8", port, ap.as_ref()).await
            });
            // layer order: an allowing answer is followed by the post-answer policy
            // re-check, so the same policy hook is consulted twice; any other vote ends at once.
            let calls = if expected == "allow" { 2 } else { 1 };
            for _ in 0..calls {
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" {
                        assert_eq!(msg["params"]["arguments"]["mode"], "policy");
                        assert_eq!(msg["params"]["arguments"]["args"][0], json!({"host":"8.8.8.8","port":port}));
                        if let Some(reply) = reply.clone() {
                            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":reply})).await;
                        }
                        break;
                    }
                }
            }
            assert_eq!(vote.await.unwrap().is_ok(), expected == "allow", "case {case}: {expected}");
        }
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}})).await;
        loop { if rpc_read(&mut cr).await["id"] == 5 { break; } }
        assert!(mare_binding::netauth::NetPolicy::deny_all().authorize("8.8.8.8", 4200, approver.as_ref()).await.is_err(),
            "an existing policy listener must fail closed after capability withdrawal");
        drop(cw);
        server_task.abort();
        let _ = server_task.await;
        host.shutdown().await;
    }).await;
}

#[tokio::test]
async fn rpc_policy_hooks_require_the_callbacks_capability_and_fail_closed_if_withdrawn() {
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
                json!({
                    "jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocol_version":"3.0","capabilities":{"callbacks":false}}
                }),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 {
                    break;
                }
            }
            let tool_before_listeners = host.kernel.listener_count("tool/before");

            // Policy subscriptions fail registration without the capability.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":2,"method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/before","callbacks":["handle"]}
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    assert!(msg.get("error").is_some(), "{msg}");
                    break;
                }
            }
            assert_eq!(host.kernel.listener_count("tool/before"), tool_before_listeners);

            // Non-policy subscriptions also fail registration without capabilities.callbacks.
            let status_listeners = host.kernel.listener_count("status/changed");
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":3,"method":"hooks.subscribe",
                    "params":{"scope":"all","event":"status/changed","callbacks":["handle"]}
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 3 {
                    assert!(msg.get("error").is_some(), "{msg}");
                    break;
                }
            }
            assert_eq!(host.kernel.listener_count("status/changed"), status_listeners);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":4,"method":"connection.set_capabilities",
                    "params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}
                }),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 4 {
                    break;
                }
            }
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":5,"method":"hooks.subscribe",
                    "params":{"scope":"all","event":"tool/before","callbacks":["handle"]}
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 5 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }

            // Revoking the capability after registration cannot turn the policy callback into
            // `nil`/abstain; the listener must refuse the protected operation.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0","id":6,"method":"connection.set_capabilities",
                    "params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}
                }),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 6 {
                    break;
                }
            }
            let lua = host.lua();
            let call =
                mare_shared::lua::json_to_lua(lua, &json!({"id":"c1","name":"bash","arguments":{"command":"true"}}))
                    .unwrap();
            let info = mare_shared::lua::json_to_lua(lua, &json!({"side_effect":"write"})).unwrap();
            let verdict = mare_shared::lua::json_to_lua(lua, &json!({"reason":"must be refused"})).unwrap();
            let vote =
                host.kernel.policy("tool/before", MultiValue::from_vec(vec![call, info, verdict])).await.unwrap();
            assert_eq!(vote.get::<String>("decision").unwrap(), "deny", "{vote:?}");
            assert!(vote.get::<String>("reason").unwrap().contains("callback.call"), "{vote:?}");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

struct FlushFails;
impl AsyncWrite for FlushFails {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct Stalled;
impl AsyncWrite for Stalled {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn output_budget_covers_the_line_being_written() {
    let budget = Arc::new(tokio::sync::Semaphore::new(9));
    let (tx, rx) = mpsc::channel(1);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    tx.send(OutLine {
        text: "response".into(),
        _budget: budget.clone().acquire_many_owned(9).await.unwrap(),
        _slot: slots.acquire_owned().await.unwrap(),
    })
    .await
    .unwrap();
    let writer = tokio::spawn(write_output(Stalled, rx));
    tokio::task::yield_now().await;
    assert_eq!(budget.available_permits(), 0);
    writer.abort();
    let _ = writer.await;
    assert_eq!(budget.available_permits(), 9);
}

#[tokio::test]
async fn output_writer_reports_a_flush_failure() {
    let (tx, rx) = mpsc::channel(1);
    let budget = Arc::new(tokio::sync::Semaphore::new(32));
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    tx.send(OutLine {
        text: "response".into(),
        _budget: budget.acquire_many_owned(9).await.unwrap(),
        _slot: slots.acquire_owned().await.unwrap(),
    })
    .await
    .unwrap();
    let err = write_output(FlushFails, rx).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn uninitialized_requests_and_failed_initialize_are_rejected_until_valid_handshake() {
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
            let mut next_id = 1u64;
            let mut call = |method: &str, params: J| {
                let id = next_id;
                next_id += 1;
                (id, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            };
            async fn read_reply(cr: &mut (impl AsyncBufReadExt + Unpin), id: u64) -> J {
                loop {
                    let msg = rpc_read(cr).await;
                    if msg["id"] == id {
                        return msg;
                    }
                }
            }

            for (method, params) in [
                ("agent.prompt", json!({"session_id":"x","input":"hi"})),
                ("options.get", json!({})),
                ("options.set", json!({"values":{"effort":"low"}})),
                (
                    "tools.register",
                    json!({"name":"t","description":"d","parameters":{"type":"object"},"callbacks":["execute"]}),
                ),
                ("hooks.subscribe", json!({"event":"status/changed","callbacks":["handle"]})),
                ("llm.list_models", json!({})),
                ("commands.run", json!({"line":"/help"})),
                ("agent.get", json!({"session_id":"x"})),
                ("status.get", json!({})),
            ] {
                let (id, req) = call(method, params);
                rpc_send(&mut cw, req).await;
                let msg = read_reply(&mut cr, id).await;
                assert_eq!(msg["error"]["code"], codes::INVALID_REQUEST, "{method}: {msg}");
            }

            // Unknown methods still report METHOD_NOT_FOUND before initialize.
            let (id, req) = call("no/such", json!({}));
            rpc_send(&mut cw, req).await;
            let msg = read_reply(&mut cr, id).await;
            assert_eq!(msg["error"]["code"], codes::METHOD_NOT_FOUND, "{msg}");

            for bad_version in ["1.7", "2", "2.invalid", "2.0.1", "", " 2.0"] {
                let (id, req) = call("initialize", json!({"protocol_version": bad_version}));
                rpc_send(&mut cw, req).await;
                let msg = read_reply(&mut cr, id).await;
                assert_eq!(msg["error"]["data"]["code"], "unsupported_version", "{bad_version}: {msg}");

                // Connection stays uninitialized after a rejected initialize.
                let (id, req) = call("options.get", json!({}));
                rpc_send(&mut cw, req).await;
                let msg = read_reply(&mut cr, id).await;
                assert_eq!(msg["error"]["code"], codes::INVALID_REQUEST, "after {bad_version}: {msg}");
            }

            // A valid 3.0 handshake succeeds and unlocks business methods.
            let (id, req) = call("initialize", json!({"protocol_version": "3.0"}));
            rpc_send(&mut cw, req).await;
            let msg = read_reply(&mut cr, id).await;
            assert!(msg.get("result").is_some(), "{msg}");
            let sid = msg["result"]["session"]["session_id"].clone();

            let (id, req) = call("options.get", json!({ "session_id": sid }));
            rpc_send(&mut cw, req).await;
            let msg = read_reply(&mut cr, id).await;
            assert!(msg.get("result").is_some(), "{msg}");

            let (id, req) = call("llm.list_models", json!({}));
            rpc_send(&mut cw, req).await;
            let msg = read_reply(&mut cr, id).await;
            assert!(msg["result"]["data"].is_array(), "{msg}");

            let (id, req) = call("agent.get", json!({ "session_id": sid }));
            rpc_send(&mut cw, req).await;
            let msg = read_reply(&mut cr, id).await;
            assert!(msg.get("result").is_some(), "{msg}");

            for removed in ["models.list", "state.get", "ui.select.highlight"] {
                let (id, req) = call(removed, json!({}));
                rpc_send(&mut cw, req).await;
                let msg = read_reply(&mut cr, id).await;
                assert_eq!(msg["error"]["code"], codes::METHOD_NOT_FOUND, "{removed}: {msg}");
            }

            // Clean shutdown works and terminates the connection loop.
            let (id, req) = call("shutdown", json!({}));
            rpc_send(&mut cw, req).await;
            let msg = read_reply(&mut cr, id).await;
            assert_eq!(msg["result"], json!({}), "{msg}");
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

static TEST_INPUT_EOF_NOTIFY: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>> = std::sync::Mutex::new(None);

pub(super) fn notify_test_input_eof() {
    if let Some(n) = TEST_INPUT_EOF_NOTIFY.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        n.notify_waiters();
    }
}

#[tokio::test]
async fn rpc_stdin_eof_cancels_pre_start_request_before_prompt_runs() {
    use std::sync::atomic::{AtomicBool, Ordering};
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let entered_pre_start = Arc::new(tokio::sync::Notify::new());
            let release_pre_start = Arc::new(tokio::sync::Notify::new());
            let prompt_started = Arc::new(AtomicBool::new(false));
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            {
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                let owner = host.kernel.host_owner().unwrap();
                let started = prompt_started.clone();
                let start_fn = lua
                    .create_function(move |_, _: mlua::MultiValue| {
                        started.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                host.kernel
                    .host_on(&ctx, owner, "turn/started", start_fn, mare_kernel::HostOn { prepend: true })
                    .unwrap();
            }

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let h = host.clone();
            let server_task = tokio::task::spawn_local(serve(h, sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 {
                    break;
                }
            }

            // Pause inside `open_session(conn)` (`session.info`), a pre-start async
            // boundary in `services::call` before `agent.prompt` runs.
            {
                let lua = host.lua();
                let entered = entered_pre_start.clone();
                let release = release_pre_start.clone();
                let info_fn = lua
                    .create_async_function(move |lua, _: mlua::MultiValue| {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            lua.create_table()
                        }
                    })
                    .unwrap();
                if let Some(session_tbl) = host.service_table("session") {
                    session_tbl.set("info", info_fn).unwrap();
                } else {
                    lua.globals().set("rpc_test_session_info", info_fn).unwrap();
                    let source = mare_kernel::PluginSource {
                        name_hint: "rpc-test-session".into(),
                        origin: mare_kernel::Origin::User,
                        path: None,
                        code: Some(
                            br#"
                            return { name = "rpc-test-session", provide = { "session" },
                                apply = function(ctx)
                                    local Session = require("cordis").class("RpcTestSession", mare.Service)
                                    function Session:constructor(c)
                                        Session.super.constructor(self, c, "session")
                                    end
                                    function Session:info(...) return rpc_test_session_info(...) end
                                    for _, name in ipairs({
                                        "record", "records", "set_sandbox_mode", "available", "switching",
                                        "child", "children", "short_id", "open_transcript", "entries",
                                        "path_to", "leaf", "entry_id", "id_of", "file_changes",
                                        "get", "list_items", "add_record", "list_records",
                                        "set_leaf", "set_name", "update", "label_item", "archive",
                                        "unarchive", "delete", "rewind", "rewind_conversation",
                                        "list", "commit", "append_compaction", "append_prune", "transition",
                                        "search", "search_items", "get_item", "trace", "trace_item",
                                        "persist_message", "record_tool_state", "record_request",
                                        "record_request_end", "record_request_aborted",
                                        "baseline_cut",
                                    }) do
                                        Session[name] = function() end
                                    end
                                    ctx:plugin(Session)
                                end }
                        "#,
                        ),
                    };
                    let def = host.kernel.load_def(&source).unwrap();
                    let (replaced, refused) = host.kernel.merge_defs(vec![def]);
                    assert!(replaced.is_empty() && refused.is_empty());
                    host.kernel.start().await.unwrap();
                    assert!(host.service_table("session").is_some());
                }
            }
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"hello"}}),
            )
            .await;
            loop {
                tokio::select! {
                    _ = entered_pre_start.notified() => break,
                    msg = rpc_read(&mut cr) => {
                        if msg["method"] == "event" {
                            continue;
                        }
                        panic!("prompt returned early instead of entering pre-start: {msg}");
                    }
                }
            }

            // Close stdin (EOF) while the prompt request is still in its pre-start phase, and
            // wait for the server to observe stdin EOF before releasing the paused future.
            struct ClearEofHook;
            impl Drop for ClearEofHook {
                fn drop(&mut self) {
                    *TEST_INPUT_EOF_NOTIFY.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
            let eof_observed = Arc::new(tokio::sync::Notify::new());
            *TEST_INPUT_EOF_NOTIFY.lock().unwrap_or_else(|e| e.into_inner()) = Some(eof_observed.clone());
            let _clear_eof_hook = ClearEofHook;
            let mut eof_notified = std::pin::pin!(eof_observed.notified());
            eof_notified.as_mut().enable();
            cw.shutdown().await.unwrap();
            drop(cw);
            tokio::time::timeout(Duration::from_secs(5), eof_notified)
                .await
                .expect("server must observe stdin EOF before releasing pre-start barrier");
            release_pre_start.notify_one();

            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
            assert!(!prompt_started.load(Ordering::SeqCst), "pre-start prompt ran after stdin EOF");
        })
        .await;
}

#[tokio::test]
async fn rpc_shutdown_delivers_prior_events_and_response_before_closing() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
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
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 {
                    break;
                }
            }

            // Queue a burst of events that flush_events() must drain before shutdown replies.
            let total_notices = 25usize;
            for i in 0..total_notices {
                let payload = mare_shared::lua::json_to_lua(
                    host.lua(),
                    &json!({ "level": "info", "text": format!("notice-{i}") }),
                )
                .unwrap();
                let _ = host.kernel.emit("host/notice", mlua::MultiValue::from_vec(vec![payload])).await;
            }

            // Send shutdown; flush_events() must drain all prior notices before replying.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}})).await;
            cw.flush().await.unwrap();

            let mut notices = Vec::new();
            let mut replies = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                let n = tokio::time::timeout(Duration::from_secs(5), cr.read_line(&mut line)).await.unwrap().unwrap();
                if n == 0 {
                    break;
                }
                let msg: J = serde_json::from_str(&line).unwrap();
                if msg.get("method").and_then(J::as_str) == Some("event") && msg["params"]["type"] == "host/notice" {
                    assert!(replies.is_empty(), "notice arrived after shutdown reply: {msg}");
                    notices.push(msg["params"]["data"]["text"].as_str().unwrap().to_string());
                } else if msg.get("id").and_then(J::as_i64) == Some(2) {
                    assert_eq!(msg["result"], json!({}), "{msg}");
                    replies.push(2);
                }
            }

            assert_eq!(notices.len(), total_notices);
            assert_eq!(replies, vec![2]);
            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
        })
        .await;
}

pub(in crate::rpc) async fn rpc_read_reply_with_pre(
    cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    id: u64,
) -> (J, Vec<J>) {
    let mut pre = Vec::new();
    loop {
        let msg = rpc_read(cr).await;
        if msg.get("id").and_then(J::as_u64) == Some(id) {
            return (msg, pre);
        }
        pre.push(msg);
    }
}

async fn rpc_read_reply(cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>, id: i64) -> J {
    rpc_read_reply_with_pre(cr, id as u64).await.0
}

async fn rpc_read_framed<R: tokio::io::AsyncBufRead + Unpin>(lines: &mut tokio::io::Lines<R>) -> J {
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("RPC frame timed out")
        .expect("RPC frame read failed")
        .expect("RPC stream closed before reply");
    serde_json::from_str(&line).unwrap()
}

async fn rpc_read_framed_reply<R: tokio::io::AsyncBufRead + Unpin>(lines: &mut tokio::io::Lines<R>, id: i64) -> J {
    loop {
        let msg = rpc_read_framed(lines).await;
        if msg.get("id").and_then(J::as_i64) == Some(id) {
            return msg;
        }
    }
}

#[tokio::test]
async fn rpc_reconnect_retains_prompt_request_cache_for_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: tmp.path().to_path_buf(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["agent.prompt", "agent.wait", "session.create"] },
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            // First connection submits a prompt with request_id, then disconnects.
            let (client1, server1) = tokio::io::duplex(65536);
            let (sr1, sw1) = tokio::io::split(server1);
            let task1 = tokio::task::spawn_local(serve(host.clone(), sr1, sw1, false, std::future::pending()));
            let (cr1, mut cw1) = tokio::io::split(client1);
            let mut cr1 = BufReader::new(cr1);

            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let init1 = rpc_read_reply(&mut cr1, 1).await;
            assert!(init1.get("result").is_some(), "{init1}");

            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let info1 = rpc_read_reply(&mut cr1, 2).await;
            let sid = info1["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid,"input":"hello-reconnect","request_id":"r-reconn"}}),
            )
            .await;
            let first = rpc_read_reply(&mut cr1, 3).await;
            let turn_id = first["result"]["turn_id"].clone();
            assert!(turn_id.is_string(), "{first}");
            assert_ne!(first["result"]["duplicate"], true, "{first}");
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":4,"method":"agent.wait","params":{"session_id":sid,"turn_id":turn_id}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr1, 4).await;

            drop(cw1);
            drop(cr1);
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();

            // Second connection retries the same request_id: must be idempotent without re-running.
            let (client2, server2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(serve(host.clone(), sr2, sw2, false, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);

            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let init2 = rpc_read_reply(&mut cr2, 1).await;
            assert!(init2.get("result").is_some(), "{init2}");

            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"hello-reconnect","request_id":"r-reconn"}}),
            )
            .await;
            let replay = rpc_read_reply(&mut cr2, 2).await;
            assert_eq!(replay["result"]["turn_id"], turn_id, "{replay}");
            assert_eq!(replay["result"]["duplicate"], true, "{replay}");

            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid,"input":"different-payload","request_id":"r-reconn"}}),
            )
            .await;
            let conflict = rpc_read_reply(&mut cr2, 3).await;
            assert_eq!(conflict["error"]["data"]["code"], "request_id_conflict", "{conflict}");

            drop(cw2);
            drop(cr2);
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();

            // Primary connection shutdown/drop must also not clear the receipt.
            let (client3, server3) = tokio::io::duplex(65536);
            let (sr3, sw3) = tokio::io::split(server3);
            let task3 = tokio::task::spawn_local(serve(host.clone(), sr3, sw3, true, std::future::pending()));
            let (cr3, mut cw3) = tokio::io::split(client3);
            let mut cr3 = BufReader::new(cr3);

            rpc_send(&mut cw3, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let _ = rpc_read_reply(&mut cr3, 1).await;
            rpc_send(
                &mut cw3,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"hello-reconnect","request_id":"r-reconn"}}),
            )
            .await;
            let replay3 = rpc_read_reply(&mut cr3, 2).await;
            assert_eq!(replay3["result"]["duplicate"], true, "{replay3}");
            rpc_send(&mut cw3, json!({"jsonrpc":"2.0","id":3,"method":"shutdown","params":{}})).await;
            cw3.flush().await.unwrap();
            drop(cw3);
            drop(cr3);
            tokio::time::timeout(Duration::from_secs(5), task3).await.unwrap().unwrap();
        })
        .await;
}

#[tokio::test]
async fn prompt_receipts_are_isolated_per_host_instance() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let make_host = |dir: std::path::PathBuf| async move {
                Host::new(mare_host::HostOptions {
                    cwd: dir,
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap()
            };
            let d1 = tempfile::tempdir().unwrap();
            let d2 = tempfile::tempdir().unwrap();
            let h1 = make_host(d1.path().to_owned()).await;
            let h2 = make_host(d2.path().to_owned()).await;

            let (c1, s1) = tokio::io::duplex(65536);
            let (sr1, sw1) = tokio::io::split(s1);
            let t1 = tokio::task::spawn_local(serve(h1.clone(), sr1, sw1, true, std::future::pending()));
            let (cr1, mut cw1) = tokio::io::split(c1);
            let mut cr1 = BufReader::new(cr1);

            let (c2, s2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(s2);
            let t2 = tokio::task::spawn_local(serve(h2.clone(), sr2, sw2, true, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);

            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let init1 = rpc_read_reply(&mut cr1, 1).await;
            let sid1 = init1["result"]["session"]["session_id"].as_str().unwrap_or_default().to_string();
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let init2 = rpc_read_reply(&mut cr2, 1).await;
            let sid2 = init2["result"]["session"]["session_id"].as_str().unwrap_or_default().to_string();
            assert!(!sid1.is_empty() && !sid2.is_empty());

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid1,"input":"host-one","request_id":"shared-rid"}}),
            )
            .await;
            let r1 = rpc_read_reply(&mut cr1, 2).await;
            assert!(r1["error"].is_null(), "h1 prompt failed: {r1}");
            let w1 = r1["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":3,"method":"agent.wait","params":{"session_id":sid1,"turn_id":w1}})).await;
            let _ = rpc_read_reply(&mut cr1, 3).await;

            // h2 runs its own prompt with the same request_id without colliding with h1's receipts.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid2,"input":"host-two-different","request_id":"shared-rid"}}),
            )
            .await;
            let r2 = rpc_read_reply(&mut cr2, 2).await;
            assert!(
                r2["error"].is_null() && r2["result"]["duplicate"].is_null(),
                "h2 prompt receipts must be isolated from h1: {r2}"
            );
            let w2 = r2["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":3,"method":"agent.wait","params":{"session_id":sid2,"turn_id":w2}})).await;
            let _ = rpc_read_reply(&mut cr2, 3).await;

            drop(cw1);
            drop(cw2);
            t1.abort();
            t2.abort();
            let _ = t1.await;
            let _ = t2.await;
            h1.shutdown().await;
            h2.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn prompt_receipts_scope_and_idempotency_across_session_new_in_single_host() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (c, s) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(s);
            let task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(c);
            let mut cr = BufReader::new(cr);

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let init = rpc_read_reply(&mut cr, 1).await;
            let sid_a = init["result"]["session"]["session_id"].as_str().unwrap_or_default().to_string();
            assert!(!sid_a.is_empty());

            // First prompt records receipt under sid_a; exact duplicate returns cached duplicate.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid_a,"input":"first-body","request_id":"r-dup"}}),
            )
            .await;
            let p1 = rpc_read_reply(&mut cr, 2).await;
            assert!(p1["error"].is_null() && p1["result"]["duplicate"].is_null(), "{p1}");
            let w1 = p1["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":20,"method":"agent.wait","params":{"session_id":sid_a,"turn_id":w1}})).await;
            let _ = rpc_read_reply(&mut cr, 20).await;

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid_a,"input":"first-body","request_id":"r-dup"}}),
            )
            .await;
            let dup = rpc_read_reply(&mut cr, 3).await;
            assert_eq!(dup["result"]["duplicate"], true, "same session + same payload must deduplicate: {dup}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"agent.prompt","params":{"session_id":sid_a,"input":"mutated-body","request_id":"r-dup"}}),
            )
            .await;
            let conflict = rpc_read_reply(&mut cr, 4).await;
            assert!(
                !conflict["error"].is_null(),
                "same request_id with different payload must fail cleanly: {conflict}"
            );

            // Starting a new session on the same Host yields a fresh session scope so r-dup is free.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.create","params":{}})).await;
            let sn = rpc_read_reply(&mut cr, 5).await;
            assert!(sn["error"].is_null(), "session.create failed: {sn}");
            let sid_b = sn["result"]["session_id"].as_str().unwrap_or_default().to_string();
            assert!(!sid_b.is_empty() && sid_b != sid_a, "expected distinct session id, got {sid_b} vs {sid_a}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"agent.prompt","params":{"session_id":sid_b,"input":"second-body","request_id":"r-dup"}}),
            )
            .await;
            let p2 = rpc_read_reply(&mut cr, 6).await;
            assert!(
                p2["error"].is_null() && p2["result"]["duplicate"].is_null(),
                "new session on same Host must isolate prompt receipts from previous session: {p2}"
            );
            let w2 = p2["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":21,"method":"agent.wait","params":{"session_id":sid_b,"turn_id":w2}})).await;
            let _ = rpc_read_reply(&mut cr, 21).await;

            drop(cw);
            task.abort();
            let _ = task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_connection_close_cancels_and_waits_for_inflight_requests() {
    use std::sync::atomic::{AtomicBool, Ordering};
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let entered = Arc::new(tokio::sync::Notify::new());
            let release_cleanup = Arc::new(tokio::sync::Notify::new());
            let cleanup_done = Arc::new(AtomicBool::new(false));
            {
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                let owner = host.kernel.host_owner().unwrap();
                let entered_c = entered.clone();
                let release_c = release_cleanup.clone();
                let done_c = cleanup_done.clone();
                let on_new = lua
                    .create_async_function(move |_, _: mlua::MultiValue| {
                        let entered = entered_c.clone();
                        let release = release_c.clone();
                        let done = done_c.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            done.store(true, Ordering::SeqCst);
                            Ok(())
                        }
                    })
                    .unwrap();
                host.kernel.host_on(&ctx, owner, "session/new", on_new, mare_kernel::HostOn::default()).unwrap();
            }

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let mut server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            // Start a committed `session.create` so the request task stays in flight until `on_new` finishes.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            entered.notified().await;

            // Close the connection while the request is in flight.
            drop(cw);
            drop(cr);
            tokio::task::yield_now().await;
            assert!(!server_task.is_finished(), "serve must wait for in-flight request to finish");

            // Release the handler and verify `serve` completes only after cleanup_done is true.
            release_cleanup.notify_one();
            tokio::time::timeout(Duration::from_secs(5), &mut server_task).await.unwrap().unwrap();
            assert!(cleanup_done.load(Ordering::SeqCst), "serve must wait for in-flight request cleanup");
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_session_scoped_request_observes_session_cancellation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let entered = Arc::new(tokio::sync::Notify::new());

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            let init = rpc_read_reply(&mut cr, 1).await;
            let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

            {
                let lua = host.lua();
                let session_tbl = host.service_table("session").unwrap();
                let orig_info: mlua::Function = session_tbl.get("info").unwrap();
                let entered_c = entered.clone();
                let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let info_fn = lua
                    .create_async_function(move |_, args: mlua::MultiValue| {
                        let orig = orig_info.clone();
                        let entered = entered_c.clone();
                        let paused = paused.clone();
                        async move {
                            if !paused.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                entered.notify_one();
                                std::future::pending::<()>().await;
                            }
                            orig.call_async::<mlua::MultiValue>(args).await
                        }
                    })
                    .unwrap();
                session_tbl.set("info", info_fn).unwrap();
            }

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"hello"}}),
            )
            .await;
            entered.notified().await;

            // Rotate session cancel (as on session switch/close).
            host.rotate_session_cancel();
            let reply = rpc_read_reply(&mut cr, 2).await;
            assert_eq!(reply["error"]["code"], codes::CANCELLED, "{reply}");
            assert_eq!(reply["error"]["data"]["code"], "cancelled", "{reply}");
            assert_eq!(reply["error"]["message"], "connection closed", "{reply}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_post_commit_session_switch_completion_continues_after_caller_cancel() {
    use std::sync::atomic::{AtomicBool, Ordering};
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let before_id = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();

            let switch_complete = Arc::new(AtomicBool::new(false));
            let switch_complete_notify = Arc::new(tokio::sync::Notify::new());
            let hold_completion_entered = Arc::new(tokio::sync::Notify::new());
            let release_completion = Arc::new(tokio::sync::Notify::new());
            {
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                let owner = host.kernel.host_owner().unwrap();

                // Hook `session/new` (emitted inside `finish_switch` on `st.ctx` AFTER commit)
                // so we can cancel the RPC caller while post-commit completion is in progress.
                let entered = hold_completion_entered.clone();
                let release = release_completion.clone();
                let on_new = lua
                    .create_async_function(move |_, _: mlua::MultiValue| {
                        let entered = entered.clone();
                        let release = release.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            Ok(())
                        }
                    })
                    .unwrap();
                host.kernel.host_on(&ctx, owner, "session/new", on_new, mare_kernel::HostOn::default()).unwrap();

                let done = switch_complete.clone();
                let done_notify = switch_complete_notify.clone();
                let on_complete = lua
                    .create_function(move |_, _: mlua::MultiValue| {
                        done.store(true, Ordering::SeqCst);
                        done_notify.notify_waiters();
                        Ok(())
                    })
                    .unwrap();
                host.kernel
                    .host_on(&ctx, owner, "session/switch-completed", on_complete, mare_kernel::HostOn::default())
                    .unwrap();
            }

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            // Wait until `session.create` has committed and entered post-commit completion on `st.ctx`.
            hold_completion_entered.notified().await;

            // Cancel the RPC caller's request while post-commit completion is running: the
            // operation has started, so it runs to its end and the reply reports it (shutdown grace:
            // a `cancelled` reply here told the client nothing happened while the session had
            // switched, and the connection kept tracking the old session).
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":2}})).await;
            // the cancellation is delivered before the operation may continue
            tokio::task::yield_now().await;
            release_completion.notify_one();
            let reply = rpc_read_reply(&mut cr, 2).await;
            tokio::time::timeout(Duration::from_secs(5), switch_complete_notify.notified()).await.ok();
            assert!(switch_complete.load(Ordering::SeqCst), "post-commit completion must finish");

            let after_id = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            assert_ne!(after_id, before_id, "committed session switch must not roll back");
            assert!(reply.get("error").is_none(), "a started switch answers with its result: {reply}");
            assert_eq!(reply["result"]["session_id"], after_id, "the reply names the session now active: {reply}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

std::thread_local! {
    static DISCONNECT_NOTIFY: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn notify_disconnect_observed() {
    DISCONNECT_NOTIFY.with(|cell| {
        if let Some(n) = cell.borrow().as_ref() {
            n.notify_one();
        }
    });
}

fn arm_disconnect_notify() -> Arc<tokio::sync::Notify> {
    let n = Arc::new(tokio::sync::Notify::new());
    DISCONNECT_NOTIFY.with(|cell| {
        *cell.borrow_mut() = Some(n.clone());
    });
    n
}

struct GatedHost {
    _dir: tempfile::TempDir,
    host: Arc<Host>,
    turn_entered: Arc<tokio::sync::Notify>,
    turn_release: Arc<tokio::sync::Notify>,
    pre_commit_entered: Arc<tokio::sync::Notify>,
    pre_commit_release: Arc<tokio::sync::Notify>,
    pub_entered: Arc<tokio::sync::Notify>,
    pub_release: Arc<tokio::sync::Notify>,
    pub_called: Arc<std::sync::atomic::AtomicBool>,
    pub_done: Arc<std::sync::atomic::AtomicBool>,
    pub_snapshot: Arc<std::sync::Mutex<Option<J>>>,
}

async fn setup_gated_host(block_pre: bool, block_publication: bool) -> GatedHost {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new(mare_host::HostOptions {
        cwd: dir.path().to_owned(),
        mode: mare_plugin::Mode::Print,
        config: json!({
            "model": "faux/echo",
            "plugins": { "permissions": { "enabled": false }, "subagent": { "enabled": false } },
            "peer": { "allow": ["agent.steer", "agent.prompt"] }
        }),
        load_init: false,
        project_trusted: false,
        args: json!({}),
        no_user_plugins: true,
    })
    .await
    .unwrap();

    let turn_entered = Arc::new(tokio::sync::Notify::new());
    let turn_release = Arc::new(tokio::sync::Notify::new());
    let pre_commit_entered = Arc::new(tokio::sync::Notify::new());
    let pre_commit_release = Arc::new(tokio::sync::Notify::new());
    let pub_entered = Arc::new(tokio::sync::Notify::new());
    let pub_release = Arc::new(tokio::sync::Notify::new());
    let pub_called = Arc::new(AtomicBool::new(false));
    let pub_done = Arc::new(AtomicBool::new(false));
    let pub_snapshot = Arc::new(std::sync::Mutex::new(None));

    {
        let lua = host.lua();
        let ctx = host.kernel.host_ctx().unwrap();
        let owner = host.kernel.host_owner().unwrap();

        // Hold a real running turn (`A.busy == true`) at `turn/started` once.
        let te = turn_entered.clone();
        let tr = turn_release.clone();
        let gated = Arc::new(AtomicBool::new(false));
        let turn_gate = lua
            .create_async_function(move |_, _: mlua::MultiValue| {
                let te = te.clone();
                let tr = tr.clone();
                let gated = gated.clone();
                async move {
                    if !gated.swap(true, Ordering::SeqCst) {
                        te.notify_one();
                        tr.notified().await;
                    }
                    Ok(())
                }
            })
            .unwrap();
        host.kernel.host_on(&ctx, owner, "turn/started", turn_gate, mare_kernel::HostOn::default()).unwrap();

        let entered_c = pre_commit_entered.clone();
        let release_c = pre_commit_release.clone();
        let commit_gate = lua
            .create_async_function(move |lua, _: mlua::MultiValue| {
                let entered = entered_c.clone();
                let release = release_c.clone();
                async move {
                    if block_pre {
                        entered.notify_one();
                        release.notified().await;
                    }
                    mare_shared::lua::json_to_lua(&lua, &json!({ "decision": "allow" }))
                }
            })
            .unwrap();
        host.kernel
            .host_voter(
                &ctx,
                owner,
                "session/commit-ready",
                commit_gate,
                mare_kernel::HostVoter { timeout: Some(std::time::Duration::ZERO), ..Default::default() },
            )
            .unwrap();

        let pe = pub_entered.clone();
        let pr = pub_release.clone();
        let pc = pub_called.clone();
        let pd = pub_done.clone();
        let ps = pub_snapshot.clone();
        let pub_gated = Arc::new(AtomicBool::new(false));
        let on_queue = lua
            .create_async_function(move |lua, mut args: mlua::MultiValue| {
                let pe = pe.clone();
                let pr = pr.clone();
                let pc = pc.clone();
                let pd = pd.clone();
                let ps = ps.clone();
                let pub_gated = pub_gated.clone();
                let val = args.pop_front().unwrap_or(mlua::Value::Nil);
                let snap = mare_shared::lua::lua_to_json(&lua, val).ok();
                async move {
                    pc.store(true, Ordering::SeqCst);
                    *ps.lock().unwrap() = snap;
                    if block_publication && !pub_gated.swap(true, Ordering::SeqCst) {
                        pe.notify_one();
                        pr.notified().await;
                        pd.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                }
            })
            .unwrap();
        host.kernel.host_on(&ctx, owner, "queue/changed", on_queue, mare_kernel::HostOn::default()).unwrap();
    }

    GatedHost {
        _dir: dir,
        host,
        turn_entered,
        turn_release,
        pre_commit_entered,
        pre_commit_release,
        pub_entered,
        pub_release,
        pub_called,
        pub_done,
        pub_snapshot,
    }
}

async fn start_busy_turn(g: &GatedHost) -> tokio::task::JoinHandle<anyhow::Result<J>> {
    let h = g.host.clone();
    let task = tokio::task::spawn_local(async move { h.prompt("initial-turn", json!({})).await });
    tokio::time::timeout(Duration::from_secs(5), g.turn_entered.notified())
        .await
        .expect("initial turn did not reach turn/started");
    assert!(g.host.is_busy().await, "real turn must set A.busy = true");
    task
}

// Match the RPC test client's hang guard in tests/common/rpc.rs.
const RPC_TURN_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn rpc_cancel_stops_pre_commit_steer_follow_up_and_busy_prompt_and_completes_post_commit() {
    use std::sync::atomic::Ordering;
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let g = setup_gated_host(true, false).await;
            let turn_task = start_busy_turn(&g).await;
            let sid = super::super::call_service(&g.host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();

            // 1a. Pre-commit cancellation via $/cancel_request on `agent.steer`, `agent.prompt` (queue), and busy `agent.prompt`.
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let mut server_task =
                tokio::task::spawn_local(serve(g.host.clone(), sr, sw, false, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;
            // `Lines::next_line` keeps a partial frame if the commit barrier wins `select!`.
            let mut cr = cr.lines();

            for (req_id, method, params) in [
                (2, "agent.steer", json!({ "session_id": sid, "input": "cancelled-steer" })),
                (3, "agent.prompt", json!({ "session_id": sid, "input": "cancelled-follow", "when_busy": "queue" })),
                (4, "agent.prompt", json!({ "session_id": sid, "input": "cancelled-busy-prompt" })),
            ] {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":req_id,"method":method,"params":params})).await;
                tokio::select! {
                    res = tokio::time::timeout(Duration::from_secs(5), g.pre_commit_entered.notified()) => {
                        res.unwrap_or_else(|_| panic!("pre-commit barrier timed out for {method} ($/cancel_request)"));
                    }
                    reply = rpc_read_framed_reply(&mut cr, req_id) => {
                        panic!("unexpected early reply before pre-commit barrier for {method}: {reply}");
                    }
                }

                rpc_send(&mut cw, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":req_id}})).await;
                // Requests on this connection are read in order, so this reply proves the
                // preceding cancellation notification was processed before the gate opens.
                let barrier_id = req_id + 100;
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":barrier_id,"method":methods::PEER_INFO,"params":{}}))
                    .await;
                let mut early_reply = None;
                loop {
                    let msg = rpc_read_framed(&mut cr).await;
                    if msg.get("id").and_then(J::as_i64) == Some(req_id) {
                        assert!(early_reply.replace(msg).is_none(), "duplicate reply for {method}");
                    } else if msg.get("id").and_then(J::as_i64) == Some(barrier_id) {
                        assert!(msg.get("result").is_some(), "peer.info barrier failed: {msg}");
                        break;
                    }
                }
                g.pre_commit_release.notify_waiters();
                let reply = match early_reply {
                    Some(reply) => reply,
                    None => rpc_read_framed_reply(&mut cr, req_id).await,
                };
                assert_eq!(reply["error"]["code"], codes::CANCELLED, "expected cancel error for {method}: {reply}");

                let state = g.host.state().await;
                assert_eq!(state["queued"].as_u64(), Some(0), "queue must stay empty after {method} cancel: {state}");
            }
            assert!(!g.pub_called.load(Ordering::SeqCst), "pre-commit cancel must never emit queue/changed");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), &mut server_task).await.unwrap().unwrap();
            g.turn_release.notify_one();
            let _ = tokio::time::timeout(RPC_TURN_TIMEOUT, turn_task).await.unwrap().unwrap().unwrap();
            g.host.shutdown().await;

            // 1b. Pre-commit disconnect on `agent.steer` and busy `agent.prompt` must not enqueue or persist.
            for (method, expected_text, input_val) in [
                ("agent.steer", "disconnected-steer", "disconnected-steer"),
                ("agent.prompt", "disconnected-follow", "disconnected-follow"),
                ("agent.prompt", "disconnected-busy-prompt", "disconnected-busy-prompt"),
            ] {
                let g = setup_gated_host(true, false).await;
                let turn_task = start_busy_turn(&g).await;
                let sid = super::super::call_service(&g.host, "session", "info", vec![]).await.unwrap()["session_id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let params = json!({ "session_id": sid, "input": input_val });
                let (client, server) = tokio::io::duplex(65536);
                let (sr, sw) = tokio::io::split(server);
                let mut server_task =
                    tokio::task::spawn_local(serve(g.host.clone(), sr, sw, false, std::future::pending()));
                let (cr, mut cw) = tokio::io::split(client);
                let mut cr = BufReader::new(cr);

                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
                )
                .await;
                let _ = rpc_read_reply(&mut cr, 1).await;

                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":method,"params":params})).await;
                tokio::select! {
                    res = tokio::time::timeout(Duration::from_secs(5), g.pre_commit_entered.notified()) => {
                        res.unwrap_or_else(|_| panic!("pre-commit barrier timed out for {method} (disconnect)"));
                    }
                    reply = rpc_read_reply(&mut cr, 2) => {
                        panic!("unexpected early reply before pre-commit disconnect barrier for {method}: {reply}");
                    }
                }

                let disconnect_observed = arm_disconnect_notify();
                drop(cw);
                drop(cr);
                tokio::time::timeout(Duration::from_secs(5), disconnect_observed.notified())
                    .await
                    .unwrap_or_else(|_| panic!("server did not observe disconnect for {method}"));
                g.pre_commit_release.notify_waiters();
                tokio::time::timeout(Duration::from_secs(5), &mut server_task).await.unwrap().unwrap();

                assert!(
                    !g.pub_called.load(Ordering::SeqCst),
                    "queue publication must not run after {method} pre-commit disconnect"
                );
                let state = g.host.state().await;
                assert_eq!(
                    state["queued"].as_u64(),
                    Some(0),
                    "actual queue must stay empty after {method} pre-commit disconnect: {state}"
                );
                if let Some(session_path) = state["session"].as_str()
                    && let Ok(persisted) = std::fs::read_to_string(session_path)
                {
                    assert!(
                        !persisted.contains(expected_text),
                        "disconnected input {expected_text} must not be persisted in {session_path}"
                    );
                }

                g.turn_release.notify_one();
                let _ = tokio::time::timeout(RPC_TURN_TIMEOUT, turn_task).await.unwrap().unwrap().unwrap();
                g.host.shutdown().await;
            }

            // 2. Post-commit disconnect must still complete publication/finalization
            // for `agent.steer` and busy `agent.prompt`, hitting real `steer` vs `follow_up` lanes.
            for (method, queue_key, expected_text, input_val) in [
                ("agent.steer", "steer", "committed-steer", "committed-steer"),
                ("agent.prompt", "follow_up", "committed-follow", "committed-follow"),
                ("agent.prompt", "follow_up", "committed-busy-prompt", "committed-busy-prompt"),
            ] {
                let g = setup_gated_host(false, true).await;
                let turn_task = start_busy_turn(&g).await;
                let sid = super::super::call_service(&g.host, "session", "info", vec![]).await.unwrap()["session_id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let params = json!({ "session_id": sid, "input": input_val });
                let (client, server) = tokio::io::duplex(65536);
                let (sr, sw) = tokio::io::split(server);
                let mut server_task =
                    tokio::task::spawn_local(serve(g.host.clone(), sr, sw, false, std::future::pending()));
                let (cr, mut cw) = tokio::io::split(client);
                let mut cr = BufReader::new(cr);

                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
                )
                .await;
                let _ = rpc_read_reply(&mut cr, 1).await;

                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":method,"params":params})).await;
                tokio::time::timeout(Duration::from_secs(5), g.pub_entered.notified())
                    .await
                    .unwrap_or_else(|_| panic!("post-commit barrier timed out for {method}"));

                let disconnect_observed = arm_disconnect_notify();
                drop(cw);
                drop(cr);
                tokio::time::timeout(Duration::from_secs(5), disconnect_observed.notified())
                    .await
                    .unwrap_or_else(|_| panic!("server did not observe post-commit disconnect for {method}"));
                assert!(!server_task.is_finished(), "serve must wait for post-commit finalization for {method}");

                g.pub_release.notify_one();
                tokio::time::timeout(Duration::from_secs(5), &mut server_task).await.unwrap().unwrap();
                assert!(g.pub_done.load(Ordering::SeqCst), "post-commit publication must complete for {method}");

                let snap = g.pub_snapshot.lock().unwrap().clone().expect("queue/changed snapshot must be captured");
                let matched = snap["items"]
                    .as_array()
                    .and_then(|arr| arr.iter().find(|item| item["lane"] == queue_key && item["text"] == expected_text));
                assert!(
                    matched.is_some(),
                    "committed input must be in {queue_key} lane after {method} post-commit disconnect: {snap}"
                );

                g.turn_release.notify_one();
                let turn_res = tokio::time::timeout(RPC_TURN_TIMEOUT, turn_task).await.unwrap().unwrap().unwrap();
                assert_eq!(
                    turn_res["text"].as_str(),
                    Some(format!("echo: {expected_text}").as_str()),
                    "running turn must drain {queue_key} queue and produce result for {method}: {turn_res}"
                );
                g.host.shutdown().await;
            }
        })
        .await;
}

#[tokio::test]
async fn rpc_markdown_isolates_dual_connections_and_cancels_on_disconnect() {
    use mlua::ObjectLike;
    use std::sync::atomic::{AtomicBool, Ordering};
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let slow_entered = Arc::new(tokio::sync::Notify::new());
            let slow_cancelled = Arc::new(AtomicBool::new(false));
            let slow_done = Arc::new(tokio::sync::Notify::new());
            {
                let lua = host.lua();
                let ui = host.service_table("ui").unwrap();
                let register: mlua::Function = ui.get("markdown_presenter").unwrap();
                let entered_c = slow_entered.clone();
                let cancelled_c = slow_cancelled.clone();
                let done_c = slow_done.clone();
                let render_slow = lua
                    .create_async_function(move |_, req: mlua::Table| {
                        let entered = entered_c.clone();
                        let cancelled = cancelled_c.clone();
                        let done = done_c.clone();
                        async move {
                            let source: String = req.get("source")?;
                            if source.contains("wait-disconnect") {
                                let token: mlua::AnyUserData = req.get("cancel")?;
                                entered.notify_one();
                                let _ = token.call_async_method::<()>("wait", ()).await;
                                cancelled.store(true, Ordering::SeqCst);
                                done.notify_one();
                            }
                            let code_tbl = req.get::<mlua::Table>("theme")?;
                            let _ = code_tbl;
                            Ok(mlua::Value::Nil)
                        }
                    })
                    .unwrap();
                let def = lua.create_table().unwrap();
                def.set("render", render_slow).unwrap();
                register.call_async::<mlua::Value>((ui.clone(), "slowrace", def)).await.unwrap();

                let echo_fn = lua
                    .create_async_function(|lua, req: mlua::Table| async move {
                        let source: String = req.get("source")?;
                        let mare: mlua::Table = lua.globals().get("mare")?;
                        let ui_tbl: mlua::Table = mare.get("ui")?;
                        let code_fn: mlua::Function = ui_tbl.get("Code")?;
                        code_fn.call::<mlua::Value>((format!("rendered:{source}"), "text"))
                    })
                    .unwrap();
                let def2 = lua.create_table().unwrap();
                def2.set("render", echo_fn).unwrap();
                register.call_async::<mlua::Value>((ui, "dual", def2)).await.unwrap();
            }

            let (c1, s1) = tokio::io::duplex(65536);
            let (c2, s2) = tokio::io::duplex(65536);
            let (s1_r, s1_w) = tokio::io::split(s1);
            let (s2_r, s2_w) = tokio::io::split(s2);
            let (c1_r, mut c1_w) = tokio::io::split(c1);
            let (c2_r, mut c2_w) = tokio::io::split(c2);
            let mut c1_r = BufReader::new(c1_r);
            let mut c2_r = BufReader::new(c2_r);

            let srv1 = mare_shared::txn_scope::spawn_local(serve(
                host.clone(),
                s1_r,
                s1_w,
                true,
                std::future::pending(),
            ));
            let srv2 = mare_shared::txn_scope::spawn_local(serve(
                host.clone(),
                s2_r,
                s2_w,
                true,
                std::future::pending(),
            ));

            rpc_send(&mut c1_w, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let init1 = rpc_read_reply(&mut c1_r, 1).await;
            let session_id = init1["result"]["session"]["session_id"].as_str().unwrap().to_string();
            rpc_send(&mut c2_w, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            let _init2 = rpc_read_reply(&mut c2_r, 1).await;

            // Wrong session_id is rejected with unknown_session.
            rpc_send(
                &mut c1_w,
                json!({
                    "jsonrpc":"2.0","id":2,"method":"ui.get_markdown",
                    "params":{"session_id":"missing-session-id","document_id":"shared-doc"}
                }),
            )
            .await;
            let bad_sess = rpc_read_reply(&mut c1_r, 2).await;
            assert_eq!(bad_sess["error"]["data"]["code"], "unknown_session", "{bad_sess}");

            // Concurrent same `document_id` on both connections: each receives ONLY its own `ui.markdown_changed`.
            rpc_send(
                &mut c1_w,
                json!({
                    "jsonrpc":"2.0","id":3,"method":"ui.render_markdown",
                    "params":{"session_id":session_id,"document_id":"shared-doc","source":"```dual\nconn-one\n```"}
                }),
            )
            .await;
            rpc_send(
                &mut c2_w,
                json!({
                    "jsonrpc":"2.0","id":3,"method":"ui.render_markdown",
                    "params":{"session_id":session_id,"document_id":"shared-doc","source":"```dual\nconn-two\n```"}
                }),
            )
            .await;

            let mut seen1 = Vec::new();
            let mut id3_1 = None;
            let mut seen_methods_1 = Vec::new();
            while seen1.iter().all(|n: &J| n["pending"].as_u64() != Some(0)) {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(3), c1_r.read_line(&mut line))
                    .await
                    .unwrap_or_else(|_| panic!("c1 timed out waiting for ui.markdown_changed; id3={id3_1:?}; methods={seen_methods_1:?}; seen1={seen1:?}"))
                    .unwrap();
                let msg: J = serde_json::from_str(&line).unwrap();
                if msg.get("id") == Some(&json!(3)) {
                    assert!(msg.get("error").is_none(), "c1 id=3 returned error: {msg}");
                    id3_1 = Some(msg);
                } else if msg.get("method").and_then(J::as_str) == Some(methods::MARKDOWN_CHANGED) {
                    seen1.push(msg["params"].clone());
                } else if let Some(m) = msg.get("method").and_then(J::as_str) {
                    seen_methods_1.push(m.to_string());
                }
            }
            let mut seen2 = Vec::new();
            let mut id3_2 = None;
            let mut seen_methods_2 = Vec::new();
            while seen2.iter().all(|n: &J| n["pending"].as_u64() != Some(0)) {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(3), c2_r.read_line(&mut line))
                    .await
                    .unwrap_or_else(|_| panic!("c2 timed out waiting for ui.markdown_changed; id3={id3_2:?}; methods={seen_methods_2:?}; seen2={seen2:?}"))
                    .unwrap();
                let msg: J = serde_json::from_str(&line).unwrap();
                if msg.get("id") == Some(&json!(3)) {
                    assert!(msg.get("error").is_none(), "c2 id=3 returned error: {msg}");
                    id3_2 = Some(msg);
                } else if msg.get("method").and_then(J::as_str) == Some(methods::MARKDOWN_CHANGED) {
                    seen2.push(msg["params"].clone());
                } else if let Some(m) = msg.get("method").and_then(J::as_str) {
                    seen_methods_2.push(m.to_string());
                }
            }
            assert!(seen1.iter().any(|n| n.to_string().contains("rendered:conn-one")), "conn 1 missed its own notice: {seen1:?}");
            assert!(seen2.iter().any(|n| n.to_string().contains("rendered:conn-two")), "conn 2 missed its own notice: {seen2:?}");
            assert!(!seen1.iter().any(|n| n.to_string().contains("conn-two")), "conn 1 received conn 2 notice: {seen1:?}");
            assert!(!seen2.iter().any(|n| n.to_string().contains("conn-one")), "conn 2 received conn 1 notice: {seen2:?}");

            // Disconnect conn 1 while slow presenter is active -> cancels presenter token and disposes doc.
            rpc_send(
                &mut c1_w,
                json!({
                    "jsonrpc":"2.0","id":4,"method":"ui.render_markdown",
                    "params":{"session_id":session_id,"document_id":"cancel-on-eof","source":"```slowrace\nwait-disconnect\n```"}
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut c1_r, 4).await;
            tokio::time::timeout(Duration::from_secs(5), slow_entered.notified()).await.unwrap();
            drop(c1_w);
            drop(c1_r);
            tokio::time::timeout(Duration::from_secs(5), slow_done.notified()).await.unwrap();
            assert!(slow_cancelled.load(Ordering::SeqCst), "disconnect must cancel active presenter token");
            tokio::time::timeout(Duration::from_secs(5), srv1).await.unwrap().unwrap();

            rpc_send(&mut c2_w, json!({"jsonrpc":"2.0","id":9,"method":"shutdown","params":{}})).await;
            let _ = rpc_read_reply(&mut c2_r, 9).await;
            drop(c2_w);
            drop(c2_r);
            tokio::time::timeout(Duration::from_secs(5), srv2).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_session_deleted_event_clears_prompt_request_cache_for_deleted_session_only() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: tmp.path().to_path_buf(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}})).await;
            assert!(rpc_read_reply(&mut cr, 1).await.get("result").is_some());

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let info1 = rpc_read_reply(&mut cr, 2).await;
            let sid1 = info1["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"agent.prompt","params":{"session_id":sid1,"input":"hello-del","request_id":"r-del-ev"}}),
            )
            .await;
            let reply1 = rpc_read_reply(&mut cr, 3).await;
            let turn1 = reply1["result"]["turn_id"].as_str().expect("turn_id").to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"agent.wait","params":{"session_id":sid1,"turn_id":turn1}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 4).await;

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"session.create","params":{}})).await;
            let info2 = rpc_read_reply(&mut cr, 5).await;
            let sid2 = info2["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"agent.prompt","params":{"session_id":sid2,"input":"hello-other","request_id":"r-other"}}),
            )
            .await;
            let reply2 = rpc_read_reply(&mut cr, 6).await;
            let turn2 = reply2["result"]["turn_id"].as_str().expect("turn_id").to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"agent.wait","params":{"session_id":sid2,"turn_id":turn2}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 7).await;

            // Deleting sid1 must not affect sid2's cached request_id receipt.
            host.kernel.emit_json("session/deleted", json!({ "session_id": sid1 })).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":8,"method":"agent.prompt","params":{"session_id":sid2,"input":"hello-other-diff","request_id":"r-other"}}),
            )
            .await;
            let keep_conflict = rpc_read_reply(&mut cr, 8).await;
            assert_eq!(
                keep_conflict["error"]["data"]["code"], "request_id_conflict",
                "session/deleted for sid1 must not clear sid2 receipts: {keep_conflict}"
            );

            // Deleting sid2 must clear sid2's cached request_id receipt so reuse with new input succeeds.
            host.kernel.emit_json("session/deleted", json!({ "session_id": sid2 })).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"agent.prompt","params":{"session_id":sid2,"input":"hello-after-del","request_id":"r-other"}}),
            )
            .await;
            let cleared_reply = rpc_read_reply(&mut cr, 9).await;
            assert!(
                cleared_reply["result"]["turn_id"].is_string(),
                "session/deleted for sid2 must clear sid2 receipts: {cleared_reply}"
            );
            let turn_after = cleared_reply["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"agent.wait","params":{"session_id":sid2,"turn_id":turn_after}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 10).await;

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":"shutdown","params":{}})).await;
            let _ = rpc_read_reply(&mut cr, 11).await;
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

/// contract event declarations drive `hooks.subscribe` (`expose = "both"` allowed
/// cleanly via contracts, `expose = "process"` refused) and dynamically loaded `mode = "policy"` points.
#[tokio::test]
async fn contract_events_drive_hooks_subscribe_and_policy_points() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let plugin_path = dir.path().join("hook_demo.lua");
            std::fs::write(
                &plugin_path,
                r#"return {
    name = "hook-demo",
    contracts = {
        ["tp-hooks"] = {
            methods = {},
            events = {
                ["public-emit"] = {
                    doc = "Public contract emit event.",
                    mode = "emit",
                    expose = "both",
                    delivery = "required",
                    ignorable = true,
                    data = { type = "object", properties = { msg = { type = "string" } }, required = { "msg" } },
                    example = { msg = "hi" },
                },
                ["process-secret"] = {
                    doc = "Internal process-only event.",
                    mode = "emit",
                    expose = "process",
                },
                ["custom-policy"] = {
                    doc = "Declared third-party policy point.",
                    mode = "policy",
                    expose = "lua",
                    affirmative = true,
                },
                ["process-policy"] = {
                    doc = "Internal process-only policy point.",
                    mode = "policy",
                    expose = "process",
                },
            },
        },
    },
}"#,
            )
            .unwrap();
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
            let def = host
                .kernel
                .load_def(&mare_kernel::PluginSource {
                    name_hint: "hook-demo".into(),
                    path: Some(plugin_path),
                    code: None,
                    origin: mare_kernel::Origin::User,
                })
                .unwrap();
            let (_, refused) = host.kernel.merge_defs(vec![def]);
            assert!(refused.is_empty(), "{refused:?}");

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
            let _ = rpc_read_reply(&mut cr, 1).await;

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"hooks.subscribe","params":{"scope":"all","event":"tp-hooks/public-emit","callbacks":["handle"]}}),
            )
            .await;
            let pub_sub = rpc_read_reply(&mut cr, 2).await;
            assert!(pub_sub["result"]["registration_id"].is_string(), "expected declared expose=both hook to subscribe: {pub_sub}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"hooks.subscribe","params":{"scope":"all","event":"tp-hooks/custom-policy","callbacks":["handle"]}}),
            )
            .await;
            let pol_sub = rpc_read_reply(&mut cr, 3).await;
            assert!(pol_sub["result"]["registration_id"].is_string(), "expected declared policy point hook to subscribe: {pol_sub}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"hooks.subscribe","params":{"scope":"all","event":"tp-hooks/process-secret","callbacks":["handle"]}}),
            )
            .await;
            let process_err = rpc_read_reply(&mut cr, 4).await;
            assert_eq!(process_err["error"]["code"], codes::INVALID_PARAMS, "expected invalid_arguments for process emit: {process_err}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"hooks.subscribe","params":{"scope":"all","event":"tp-hooks/process-policy","callbacks":["handle"]}}),
            )
            .await;
            let process_pol_err = rpc_read_reply(&mut cr, 5).await;
            assert_eq!(
                process_pol_err["error"]["code"],
                codes::INVALID_PARAMS,
                "expected invalid_arguments for process policy: {process_pol_err}"
            );

            drop(cw);
            drop(cr);
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_session_epoch_parameter_addressing_scope_and_workspace_trust() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let root = tempfile::tempdir().unwrap();
            let ws_real = root.path().join("ws-real");
            let ws_other = root.path().join("ws-other");
            std::fs::create_dir_all(&ws_real).unwrap();
            std::fs::create_dir_all(&ws_other).unwrap();
            #[cfg(unix)]
            let ws_alias = {
                let link = root.path().join("ws-link");
                std::os::unix::fs::symlink(&ws_real, &link).unwrap();
                link
            };
            #[cfg(not(unix))]
            let ws_alias = ws_real.clone();

            let mare_home = root.path().join("mare-home");
            std::fs::create_dir_all(&mare_home).unwrap();
            struct EnvRestore(Option<std::ffi::OsString>);
            impl Drop for EnvRestore {
                fn drop(&mut self) {
                    match &self.0 {
                        Some(v) => unsafe { std::env::set_var("MARE_HOME", v) },
                        None => unsafe { std::env::remove_var("MARE_HOME") },
                    }
                }
            }
            let _env_restore = EnvRestore(std::env::var_os("MARE_HOME"));
            unsafe { std::env::set_var("MARE_HOME", &mare_home) };

            let host = Host::new(mare_host::HostOptions {
                cwd: ws_real.clone(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": {
                        "allow": [
                            "tools.register",
                            "hooks.subscribe",
                            "commands.register",
                            "events.subscribe"
                        ]
                    }
                }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: false,
            })
            .await
            .unwrap();

            // Primary connection
            let (p_client, p_server) = tokio::io::duplex(65536);
            let (p_sr, p_sw) = tokio::io::split(p_server);
            let _p_task = tokio::task::spawn_local(serve(host.clone(), p_sr, p_sw, true, std::future::pending()));
            let (p_cr, mut p_cw) = tokio::io::split(p_client);
            let mut p_cr = BufReader::new(p_cr);

            // Secondary / peer connection
            let (s_client, s_server) = tokio::io::duplex(65536);
            let (s_sr, s_sw) = tokio::io::split(s_server);
            let _s_task = tokio::task::spawn_local(serve(host.clone(), s_sr, s_sw, false, std::future::pending()));
            let (s_cr, mut s_cw) = tokio::io::split(s_client);
            let mut s_cr = BufReader::new(s_cr);

            async fn call(
                cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
                cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                id: i64,
                method: &str,
                params: J,
            ) -> J {
                rpc_send(cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
                rpc_read_reply(cr, id).await
            }

            let p_init = call(
                &mut p_cr,
                &mut p_cw,
                1,
                "initialize",
                json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }),
            )
            .await;
            let sid = p_init["result"]["session"]["session_id"]
                .as_str()
                .expect("initialize must carry session.session_id")
                .to_string();
            let p_epoch = p_init["result"]["session"]["session_epoch"]
                .as_str()
                .expect("initialize must carry session.session_epoch");
            assert!(p_init["result"].get("session_id").is_none(), "top-level session_id removed: {p_init}");
            assert!(p_init["result"].get("session_epoch").is_none(), "top-level session_epoch removed: {p_init}");
            assert!(p_init["result"].get("generation").is_none(), "wire generation must be removed: {p_init}");
            assert!(!p_epoch.is_empty(), "{p_init}");
            assert!(p_init["result"]["session"].get("generation").is_none(), "{p_init}");

            let s_init = call(
                &mut s_cr,
                &mut s_cw,
                1,
                "initialize",
                json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }),
            )
            .await;
            assert!(s_init["result"].get("generation").is_none(), "{s_init}");
            assert!(s_init["result"].get("session_id").is_none(), "{s_init}");
            assert!(s_init["result"].get("session_epoch").is_none(), "{s_init}");
            assert!(
                s_init["result"]["session"]["session_epoch"].as_str().is_some_and(|e| !e.is_empty()),
                "{s_init}"
            );

            // events.subscribe carries session_epoch (no generation); session-targeted methods
            // honor session_id / expected_epoch and reject missing or stale sessions.
            let sub_epoch = call(
                &mut p_cr,
                &mut p_cw,
                2,
                "events.subscribe",
                json!({ "session_id": sid, "include": ["turn/*"] }),
            )
            .await;
            let epoch = sub_epoch["result"]["session_epoch"].as_str().unwrap().to_string();
            assert!(!epoch.is_empty(), "{sub_epoch}");

            let addressed = call(
                &mut p_cr,
                &mut p_cw,
                3,
                "session.list_records",
                json!({ "kind": "probe", "session_id": sid }),
            )
            .await;
            assert!(addressed.get("result").is_some(), "{addressed}");

            let stale = call(
                &mut p_cr,
                &mut p_cw,
                4,
                "agent.prompt",
                json!({
                    "input": "stale probe",
                    "session_id": sid,
                    "expected_epoch": "wrong-epoch"
                }),
            )
            .await;
            assert!(stale.get("error").is_some(), "stale expected_epoch must be rejected: {stale}");

            for (idx, (method, params)) in [
                ("session.list_records", json!({ "kind": "probe", "session_id": "missing-session" })),
                ("session.update", json!({ "name": "nope", "session_id": "missing-session" })),
                (
                    "events.subscribe",
                    json!({ "include": ["turn/*"], "scope": "session", "session_id": "missing-session" }),
                ),
                (
                    "tools.register",
                    json!({ "name": "p4t", "description": "d", "parameters": { "type": "object", "properties": {} }, "callbacks": ["execute"], "scope": "session", "session_id": "missing-session" }),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let res = call(&mut p_cr, &mut p_cw, 10 + idx as i64, method, params).await;
                assert_eq!(res["error"]["data"]["code"], "unknown_session", "{method}: {res}");
            }

            // Scope::All vs Scope::Session validation on primary and secondary connections.
            let p_all = call(
                &mut p_cr,
                &mut p_cw,
                20,
                "hooks.subscribe",
                json!({ "event": "net/approval", "callbacks": ["handle"], "scope": "all" }),
            )
            .await;
            assert!(p_all.get("result").is_some(), "primary scope=all must succeed: {p_all}");

            let p_all_with_sid = call(
                &mut p_cr,
                &mut p_cw,
                21,
                "hooks.subscribe",
                json!({ "event": "net/approval", "callbacks": ["handle"], "scope": "all", "session_id": sid }),
            )
            .await;
            assert!(p_all_with_sid.get("error").is_some(), "scope=all with session_id must fail: {p_all_with_sid}");

            let p_sess_missing_sid = call(
                &mut p_cr,
                &mut p_cw,
                22,
                "tools.register",
                json!({ "name": "p4_t", "description": "d", "parameters": { "type": "object", "properties": {} }, "callbacks": ["execute"], "scope": "session" }),
            )
            .await;
            assert!(p_sess_missing_sid.get("error").is_some(), "scope=session without session_id must fail: {p_sess_missing_sid}");

            let s_all_denied = call(
                &mut s_cr,
                &mut s_cw,
                23,
                "events.subscribe",
                json!({ "include": ["turn/*"], "scope": "all" }),
            )
            .await;
            assert_eq!(s_all_denied["error"]["data"]["code"], "denied", "secondary scope=all must deny: {s_all_denied}");

            let s_sess_ok = call(
                &mut s_cr,
                &mut s_cw,
                24,
                "events.subscribe",
                json!({ "include": ["turn/*"], "scope": "session", "session_id": sid }),
            )
            .await;
            assert!(s_sess_ok.get("result").is_some(), "secondary scope=session with valid sid must succeed: {s_sess_ok}");

            // workspace.get & workspace.set_trust across canonical path alias and secondary rejection.
            let ws_get = call(&mut p_cr, &mut p_cw, 30, "workspace.get", json!({ "cwd": ws_alias })).await;
            assert_eq!(ws_get["result"]["root"].as_str(), dunce::canonicalize(&ws_real).unwrap().to_str(), "{ws_get}");
            assert_eq!(ws_get["result"]["trusted"], false, "{ws_get}");
            assert!(ws_get["result"]["dirs"].is_array(), "{ws_get}");

            let ws_wrong = call(&mut p_cr, &mut p_cw, 31, "workspace.get", json!({ "cwd": ws_other })).await;
            assert_eq!(ws_wrong["error"]["data"]["code"], "unknown_session", "{ws_wrong}");

            let s_trust_denied = call(
                &mut s_cr,
                &mut s_cw,
                32,
                "workspace.set_trust",
                json!({ "cwd": ws_real, "trusted": true, "save": false }),
            )
            .await;
            assert!(s_trust_denied.get("error").is_some(), "secondary cannot set_trust: {s_trust_denied}");

            let set_true = call(
                &mut p_cr,
                &mut p_cw,
                33,
                "workspace.set_trust",
                json!({ "cwd": ws_alias, "trusted": true, "save": false }),
            )
            .await;
            assert!(set_true.get("result").is_some(), "{set_true}");
            assert!(host.is_project_trusted());

            let ws_after_true = call(&mut p_cr, &mut p_cw, 34, "workspace.get", json!({ "cwd": ws_real })).await;
            assert_eq!(ws_after_true["result"]["trusted"], true, "{ws_after_true}");

            let set_false = call(
                &mut p_cr,
                &mut p_cw,
                35,
                "workspace.set_trust",
                json!({ "cwd": ws_real, "trusted": false, "save": false }),
            )
            .await;
            assert!(set_false.get("result").is_some(), "{set_false}");
            assert!(!host.is_project_trusted());

            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn hooks_callback_uses_live_session_id_after_session_close_and_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0", "capabilities": { "callbacks": true } } })).await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 { break; }
            }
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 10, "method": "session.create", "params": {} })).await;
            let s1_res = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 10 { break msg; }
            };
            let s1 = s1_res["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "hooks.subscribe", "params": { "event": "request/build", "scope": "all", "callbacks": ["handle"] } })).await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }

            // Close S1 over RPC (leaving any cached conn.session_id stale if not invalidated),
            // then switch to a fresh session S2 from another RPC connection.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 3, "method": "session.close", "params": { "session_id": s1 } })).await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 3 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }

            let (c2_client, c2_server) = tokio::io::duplex(65536);
            let (c2_sr, c2_sw) = tokio::io::split(c2_server);
            let c2_task = tokio::task::spawn_local(serve(host.clone(), c2_sr, c2_sw, true, std::future::pending()));
            let (c2_cr, mut c2_cw) = tokio::io::split(c2_client);
            let mut c2_cr = BufReader::new(c2_cr);
            rpc_send(&mut c2_cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } })).await;
            loop {
                if rpc_read(&mut c2_cr).await["id"] == 1 { break; }
            }
            rpc_send(&mut c2_cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} })).await;
            let s2_val = loop {
                let msg = rpc_read(&mut c2_cr).await;
                if msg["id"] == 2 { break msg; }
            };
            let s2 = s2_val["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(s1, s2);
            drop(c2_cw);
            c2_task.abort();
            let _ = c2_task.await;

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 4, "method": "agent.prompt", "params": { "input": "hello", "session_id": s2 } })).await;
            let started = rpc_read_reply(&mut cr, 4).await;
            let turn_id = started["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 5, "method": "agent.wait", "params": { "session_id": s2, "turn_id": turn_id } })).await;
            let observed_sid = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    let cb_sid = msg["params"]["session_id"].as_str().map(str::to_string);
                    rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": msg["id"], "result": {} })).await;
                    break cb_sid;
                }
                assert_ne!(msg["id"], 5, "turn completed without firing request/build callback");
            };
            let _ = rpc_read_reply(&mut cr, 5).await;

            assert_eq!(observed_sid.as_deref(), Some(s2.as_str()), "callback must carry live session_id S2, not stale S1");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn agent_prompt_and_events_subscribe_accept_snake_case_and_reject_legacy_aliases() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 20, "method": "session.create", "params": {} })).await;
            let created = rpc_read_reply(&mut cr, 20).await;
            let sid = created["result"]["session_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 21, "method": "events.subscribe", "params": { "session_id": sid } }),
            )
            .await;
            let sub = rpc_read_reply(&mut cr, 21).await;
            let epoch = sub["result"]["session_epoch"].as_str().unwrap().to_string();

            // Canonical snake_case session_id + expected_epoch must succeed.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "agent.prompt",
                    "params": {
                        "input": "ok",
                        "session_id": sid,
                        "expected_epoch": epoch
                    }
                }),
            )
            .await;
            let ok_res = rpc_read_reply(&mut cr, 2).await;
            assert!(ok_res.get("result").is_some(), "canonical agent.prompt must succeed: {ok_res}");
            let wait_turn = ok_res["result"]["turn_id"].clone();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "agent.wait",
                    "params": { "session_id": sid, "turn_id": wait_turn }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 3).await;

            // Legacy camelCase aliases must be rejected with -32602 (invalid_arguments).
            for (idx, (method, bad_params)) in [
                ("events.subscribe", json!({ "sessionId": sid })),
                ("events.subscribe", json!({ "session_id": sid, "sessionEpoch": epoch })),
                ("agent.prompt", json!({ "input": "bad1", "sessionId": sid })),
                ("agent.prompt", json!({ "input": "bad2", "session_id": sid, "expectedEpoch": epoch })),
            ]
            .into_iter()
            .enumerate()
            {
                let id = 10 + idx as i64;
                rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": bad_params })).await;
                let err_res = rpc_read_reply(&mut cr, id).await;
                assert_eq!(err_res["error"]["code"], -32602, "legacy camelCase {method} alias must fail: {err_res}");
            }

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn hooks_subscribe_rejects_deprecated_priority_and_uses_prepend() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0", "capabilities": { "callbacks": true } } })).await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 { break; }
            }

            rpc_send(&mut cw, json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "hooks.subscribe",
                "params": { "scope": "all", "event": "request/build", "priority": 10, "callbacks": ["handle"] }
            })).await;
            let err_res = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 { break msg; }
            };
            assert_eq!(err_res["error"]["data"]["code"], "invalid_arguments", "deprecated priority must fail: {err_res}");

            rpc_send(&mut cw, json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "hooks.subscribe",
                "params": { "scope": "all", "event": "request/build", "prepend": true, "callbacks": ["handle"] }
            })).await;
            let ok_res = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 3 { break msg; }
            };
            assert!(ok_res.get("result").is_some(), "prepend=true must succeed: {ok_res}");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn events_subscribe_scope_all_receives_events_across_session_switch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } })).await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 { break; }
            }
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 10, "method": "session.create", "params": {} })).await;
            let s1_res = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 10 { break msg; }
            };
            let s1 = s1_res["result"]["session_id"].as_str().unwrap().to_string();

            rpc_send(&mut cw, json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "events.subscribe",
                "params": { "scope": "all", "include": ["turn/*"] }
            })).await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }

            // Create/switch to S2 from another RPC connection (so this connection's finish_session_switch
            // does NOT open an Exact(S2) cursor on our behalf), then run a turn on S2.
            let (c2_client, c2_server) = tokio::io::duplex(65536);
            let (c2_sr, c2_sw) = tokio::io::split(c2_server);
            let c2_task = tokio::task::spawn_local(serve(host.clone(), c2_sr, c2_sw, true, std::future::pending()));
            let (c2_cr, mut c2_cw) = tokio::io::split(c2_client);
            let mut c2_cr = BufReader::new(c2_cr);
            rpc_send(&mut c2_cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } })).await;
            loop {
                if rpc_read(&mut c2_cr).await["id"] == 1 { break; }
            }
            rpc_send(&mut c2_cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} })).await;
            let s2_val = loop {
                let msg = rpc_read(&mut c2_cr).await;
                if msg["id"] == 2 { break msg; }
            };
            let s2 = s2_val["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(s1, s2);
            drop(c2_cw);
            c2_task.abort();
            let _ = c2_task.await;

            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 3, "method": "agent.prompt", "params": { "input": "across", "session_id": s2 } })).await;
            let mut turn_started_sids = Vec::new();
            let prompt_rep = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 3 {
                    break msg;
                } else if msg["method"] == "event" && msg["params"]["type"] == "turn/started" {
                    turn_started_sids.push(msg["params"]["session_id"].as_str().unwrap_or_default().to_string());
                }
            };
            assert!(prompt_rep.get("error").is_none(), "{prompt_rep}");
            let turn_id = prompt_rep["result"]["turn_id"].as_str().unwrap().to_string();
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 4, "method": "agent.wait", "params": { "session_id": s2, "turn_id": turn_id } })).await;
            let mut seen_reply = false;
            while !seen_reply || turn_started_sids.is_empty() {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 4 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    seen_reply = true;
                } else if msg["method"] == "event" && msg["params"]["type"] == "turn/started" {
                    turn_started_sids.push(msg["params"]["session_id"].as_str().unwrap_or_default().to_string());
                }
            }
            assert_eq!(turn_started_sids, vec![s2], "scope=all must deliver S2 turn/started once without losing events or duplicating");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_initialize_requires_version_and_exactly_once() {
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
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            // Missing, null, numeric, malformed, and incompatible protocol_version fail cleanly without initializing.
            for (id, params, expected_code, expected_data_code) in [
                (1, json!({}), -32602, Some("invalid_arguments")),
                (2, json!({"protocol_version": null}), -32602, Some("invalid_arguments")),
                (3, json!({"protocol_version": 3}), -32602, Some("invalid_arguments")),
                (4, json!({"protocol_version": "bad"}), -32000, Some("unsupported_version")),
                (5, json!({"protocol_version": "2.0"}), -32000, Some("unsupported_version")),
            ] {
                rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":params})).await;
                let msg = loop {
                    let m = rpc_read(&mut cr).await;
                    if m["id"] == id { break m; }
                };
                assert_eq!(msg["error"]["code"], expected_code, "case {id}: {msg}");
                if let Some(dc) = expected_data_code {
                    assert_eq!(msg["error"]["data"]["code"], dc, "case {id}: {msg}");
                }
            }

            // Still Uninitialized: other methods are still gated.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"session.list","params":{}})).await;
            let uninit_msg = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 6 { break m; }
            };
            assert_eq!(uninit_msg["error"]["code"], -32600, "{uninit_msg}");

            // Exactly one successful initialize transitions to Ready and binds session_epoch.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocol_version":"3.0"}})).await;
            let ok_init = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 7 { break m; }
            };
            assert!(ok_init.get("error").is_none(), "{ok_init}");
            let init_epoch = ok_init["result"]["session"]["session_epoch"].as_str().unwrap_or("");
            assert!(!init_epoch.is_empty(), "initialize must return session.session_epoch: {ok_init}");
            assert!(ok_init["result"].get("session_id").is_none(), "{ok_init}");
            assert!(ok_init["result"].get("session_epoch").is_none(), "{ok_init}");

            // Duplicate initialize fails with InvalidRequest (-32600) and cannot rebind capabilities or timeouts.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":8,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true},"callback_timeout_ms":1234}})).await;
            let dup = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 8 { break m; }
            };
            assert_eq!(dup["error"]["code"], -32600, "duplicate initialize must fail with InvalidRequest: {dup}");

            // Rejected duplicate initialize must have zero side effects on capabilities.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":81,"method":"hooks.subscribe","params":{"event":"tool/before","callbacks":["handle"]}})).await;
            let hook_res = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 81 { break m; }
            };
            assert!(hook_res.get("error").is_some(), "rejected duplicate initialize must not enable callbacks: {hook_res}");

            // session.create also returns a non-empty session_epoch.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":9,"method":"session.create","params":{}})).await;
            let created = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 9 { break m; }
            };
            assert!(!created["result"]["session_epoch"].as_str().unwrap_or("").is_empty(), "{created}");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;

            // Concurrent initialize requests racing on one connection: exactly one succeeds, the other gets -32600.
            let (client2, server2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(server2);
            let server_task2 = tokio::task::spawn_local(serve(host.clone(), sr2, sw2, true, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":101,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":false}}})).await;
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":102,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}})).await;
            let mut r101 = None;
            let mut r102 = None;
            while r101.is_none() || r102.is_none() {
                let m = rpc_read(&mut cr2).await;
                if m["id"] == 101 { r101 = Some(m); }
                else if m["id"] == 102 { r102 = Some(m); }
            }
            let (r101, r102) = (r101.unwrap(), r102.unwrap());
            let ok_count = [r101.get("error").is_none(), r102.get("error").is_none()].into_iter().filter(|&b| b).count();
            assert_eq!(ok_count, 1, "exactly one concurrent initialize must succeed: r101={r101}, r102={r102}");
            let loser = if r101.get("error").is_some() { &r101 } else { &r102 };
            assert_eq!(loser["error"]["code"], -32600, "losing concurrent initialize must fail with -32600: {loser}");

            drop(cw2);
            server_task2.abort();
            let _ = server_task2.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn epoch_check_survives_await_and_idempotent_retry() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let png: &[u8] = &[
                0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00,
                0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00,
                0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01,
                0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
            ];
            std::fs::write(dir.path().join("img.png"), png).unwrap();

            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "permissions": { "mode": "allow-all" },
                    "plugins": { "subagent": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let s0 = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, s0.host.clone(), Some(s0.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            async fn call(
                cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
                cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                id: i64,
                method: &str,
                params: J,
            ) -> J {
                rpc_send(cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
                rpc_read_reply(cr, id).await
            }

            let init = call(
                &mut cr,
                &mut cw,
                1,
                "initialize",
                json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }),
            )
            .await;
            assert!(init.get("error").is_none(), "{init}");

            // Idempotent session.create retry preserves the original session_id + session_epoch.
            let c1 =
                call(&mut cr, &mut cw, 2, "session.create", json!({ "name": "idem", "request_id": "idem-c1" })).await;
            assert!(c1.get("error").is_none(), "{c1}");
            let s1 = c1["result"]["session_id"].as_str().unwrap().to_string();
            let e1 = c1["result"]["session_epoch"].as_str().unwrap().to_string();
            assert!(!e1.is_empty(), "{c1}");

            let c1_retry =
                call(&mut cr, &mut cw, 3, "session.create", json!({ "name": "idem", "request_id": "idem-c1" })).await;
            assert_eq!(c1_retry["result"]["session_id"], s1, "{c1_retry}");
            assert_eq!(c1_retry["result"]["session_epoch"], e1, "{c1_retry}");

            // Persist at least one turn on s1 so session.fork has a source file.
            let p_init = call(&mut cr, &mut cw, 30, "agent.prompt", json!({ "session_id": s1, "input": "seed" })).await;
            assert!(p_init.get("error").is_none(), "{p_init}");
            let p_wait = call(
                &mut cr,
                &mut cw,
                31,
                "agent.wait",
                json!({ "session_id": s1, "turn_id": p_init["result"]["turn_id"] }),
            )
            .await;
            assert!(p_wait.get("error").is_none(), "{p_wait}");

            // Idempotent session.fork retry returns the live fork snapshot while open, and
            // creates a fresh live fork after the previous fork is closed (receipt cleared).
            let f1 = call(
                &mut cr,
                &mut cw,
                4,
                "session.fork",
                json!({ "session_id": s1, "name": "fork1", "expected_epoch": e1, "request_id": "idem-f1" }),
            )
            .await;
            assert!(f1.get("error").is_none(), "{f1}");
            let f1_id = f1["result"]["session_id"].as_str().unwrap().to_string();
            let f1_epoch = f1["result"]["session_epoch"].as_str().unwrap().to_string();
            assert!(!f1_epoch.is_empty(), "{f1}");
            let f1_live_retry = call(
                &mut cr,
                &mut cw,
                40,
                "session.fork",
                json!({ "session_id": s1, "name": "fork1", "expected_epoch": e1, "request_id": "idem-f1" }),
            )
            .await;
            assert_eq!(f1_live_retry["result"]["session_id"], f1_id, "{f1_live_retry}");
            assert_eq!(f1_live_retry["result"]["session_epoch"], f1_epoch, "{f1_live_retry}");
            let f1_close =
                call(&mut cr, &mut cw, 5, "session.close", json!({ "session_id": f1_id, "expected_epoch": f1_epoch }))
                    .await;
            assert!(f1_close.get("error").is_none(), "{f1_close}");
            let f1_after_close = call(
                &mut cr,
                &mut cw,
                6,
                "session.fork",
                json!({ "session_id": s1, "name": "fork1", "expected_epoch": e1, "request_id": "idem-f1" }),
            )
            .await;
            assert!(f1_after_close.get("error").is_none(), "{f1_after_close}");
            assert_ne!(
                f1_after_close["result"]["session_id"], f1_id,
                "closed fork receipt must not replay dead session_id: {f1_after_close}"
            );

            // Subscribe a `tool/before` voter on s1 so image attachment in `agent.prompt` suspends at `.await`.
            let sub = call(
                &mut cr,
                &mut cw,
                7,
                "hooks.subscribe",
                json!({ "event": "tool/before", "scope": "session", "session_id": s1, "callbacks": ["handle"] }),
            )
            .await;
            assert!(sub.get("error").is_none(), "{sub}");

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 10,
                    "method": "agent.prompt",
                    "params": {
                        "session_id": s1,
                        "expected_epoch": e1,
                        "input": [
                            { "type": "text", "text": "hello" },
                            { "type": "image", "path": "img.png" }
                        ]
                    }
                }),
            )
            .await;
            let cb_req = loop {
                let m = rpc_read(&mut cr).await;
                if m["method"] == "callback.call" {
                    break m;
                }
            };

            // While prompt is suspended at the callback `.await`, close and reload s1 (rotating epoch e1 -> e2).
            let closed =
                call(&mut cr, &mut cw, 11, "session.close", json!({ "session_id": s1, "expected_epoch": e1 })).await;
            assert!(closed.get("error").is_none(), "{closed}");
            let loaded = call(&mut cr, &mut cw, 12, "session.load", json!({ "session_id": s1 })).await;
            assert!(loaded.get("error").is_none(), "{loaded}");
            let e2 = loaded["result"]["session_epoch"].as_str().unwrap();
            assert_ne!(e2, e1, "reload must rotate session_epoch");

            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": cb_req["id"], "result": { "value": { "decision": "allow" } } }),
            )
            .await;
            let prompt_reply = rpc_read_reply(&mut cr, 10).await;
            let code = prompt_reply["error"]["data"]["code"].as_str();
            assert!(
                matches!(code, Some("stale_session" | "cancelled")),
                "prompt resuming across close+reload await must fail cleanly: {prompt_reply}"
            );

            // Stale prompt resuming across close+reload must have zero write side effects on s1.
            let msgs_after = call(&mut cr, &mut cw, 13, "agent.list_messages", json!({ "session_id": s1 })).await;
            assert!(msgs_after.get("error").is_none(), "{msgs_after}");
            assert_eq!(
                msgs_after["result"]["data"].as_array().map(|a| a.len()),
                Some(2),
                "stale prompt must not write messages to the reloaded runtime: {msgs_after}"
            );

            // After close + reload rotated session_epoch (and cleared s1's receipts), retrying
            // session.create with the old request_id must not replay the stale e1 receipt.
            let c1_after_reload =
                call(&mut cr, &mut cw, 14, "session.create", json!({ "name": "idem", "request_id": "idem-c1" })).await;
            assert!(c1_after_reload.get("error").is_none(), "{c1_after_reload}");
            assert_ne!(c1_after_reload["result"]["session_id"], s1, "{c1_after_reload}");
            assert_ne!(c1_after_reload["result"]["session_epoch"], e1, "{c1_after_reload}");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Peer RPC `RpcRuntime::from_host` + `session.create` must derive sibling options from the
/// original startup configuration and destination persistent trust decision — NEVER copying
/// `TrustSession` (`host.is_project_trusted()`) or project-merged `host.config()` markers,
/// and honoring `--no-trust` and corrupt `trust.json` fail-closed.
#[tokio::test]
async fn from_host_and_peer_sibling_do_not_inherit_session_only_trust_or_project_config() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            struct EnvRestore(Option<std::ffi::OsString>);
            impl Drop for EnvRestore {
                fn drop(&mut self) {
                    match self.0.take() {
                        Some(v) => unsafe { std::env::set_var("MARE_HOME", v) },
                        None => unsafe { std::env::remove_var("MARE_HOME") },
                    }
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let mare_home = dir.path().join("mare-home");
            std::fs::create_dir_all(&mare_home).unwrap();
            let _env_restore = EnvRestore(std::env::var_os("MARE_HOME"));
            unsafe { std::env::set_var("MARE_HOME", &mare_home) };
            let mare_dir = dir.path().join(".mare");
            std::fs::create_dir_all(mare_dir.join("plugins")).unwrap();
            std::fs::write(mare_dir.join("init.lua"), "mare.config.peer_leak_marker = 'leaked-from-session-only'\n")
                .unwrap();
            std::fs::write(
                mare_dir.join("plugins/projplug.lua"),
                "return { name = 'projplug', apply = function() _G.PROJPLUG_APPLIED = true end }\n",
            )
            .unwrap();

            let anchor_opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["session.create", "plugins.list"] },
                    "plugins": {
                        "preset": "minimal",
                        "permissions": { "enabled": false }
                    }
                }),
                load_init: true,
                project_trusted: false,
                args: json!({ "trust": "undecided" }),
                no_user_plugins: false,
            };
            let mgr1 = mare_host::HostManager::new();
            let anchor_rt = mgr1.create_session(anchor_opts.clone()).await.unwrap();
            let anchor = anchor_rt.host.clone();

            anchor.answer_trust(mare_host::TrustChoice::TrustSession).await.unwrap();
            assert!(anchor.is_project_trusted(), "anchor must be trusted for session only");
            assert_eq!(
                anchor.config().get("peer_leak_marker").and_then(|v| v.as_str()),
                Some("leaked-from-session-only")
            );

            // 1a. `RpcRuntime::from_host` derives clean `base_opts` without leaking `TrustSession` or project config.
            let from_host_rpc = RpcRuntime::from_host(&anchor).await;
            let fh_opts = from_host_rpc.clone_base_opts(None).unwrap();
            assert!(!fh_opts.project_trusted, "from_host must NOT inherit TrustSession");
            assert!(fh_opts.config.get("peer_leak_marker").is_none_or(|v| v.is_null()));
            drop(from_host_rpc);

            // 1b. True multi-session Peer principal (`RpcRuntime::new` + `serve_peer_manager`) creating a sibling after `TrustSession`.
            let rpc = RpcRuntime::new(mgr1, anchor_opts, anchor.clone(), Some(anchor_rt.session_id.clone()), false);
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr, sw, None));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": "3.0" }
                }),
            )
            .await;
            let init = rpc_read_reply(&mut cr, 1).await;
            assert!(init.get("error").is_none(), "{init}");

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "name": "peer-sibling" }
                }),
            )
            .await;
            let created = rpc_read_reply(&mut cr, 2).await;
            assert!(created.get("error").is_none(), "peer session.create must succeed: {created}");
            let sibling_sid = created["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(sibling_sid, anchor_rt.session_id, "must spawn a true sibling Host");
            let sibling = rpc.manager.session(&sibling_sid).expect("sibling session registered");
            assert!(!sibling.host.is_project_trusted(), "peer sibling must NOT inherit TrustSession");
            assert!(anchor.is_project_trusted(), "anchor session remains trusted under TrustSession");
            assert!(
                sibling.host.config().get("peer_leak_marker").is_none_or(|v| v.is_null()),
                "peer sibling must NOT inherit TrustSession .mare/init.lua marker: {:?}",
                sibling.host.config()
            );
            assert!(
                sibling.host.kernel.def("projplug").is_none()
                    && !sibling.host.lua().load("return _G.PROJPLUG_APPLIED == true").eval::<bool>().unwrap(),
                "peer sibling must NOT load project plugins"
            );

            drop(cw);
            drop(cr);
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;

            // 2. CLI `--no-trust` (even with `load_init = true`) strictly overrides saved persistent trust
            //    through a true `Principal::Peer` `session.create` connection.
            mare_config::trust::set_dir(dir.path(), true).unwrap();
            assert_eq!(mare_config::trust::decision(dir.path()), Some(true));
            let no_trust_opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["session.create"] },
                    "plugins": {
                        "preset": "minimal",
                        "permissions": { "enabled": false }
                    }
                }),
                load_init: true,
                project_trusted: false,
                args: json!({ "trust": "untrusted", "trust_source": "cli" }),
                no_user_plugins: false,
            };
            let mgr_nt = mare_host::HostManager::new();
            let nt_rt = mgr_nt.create_session(no_trust_opts.clone()).await.unwrap();
            let rpc_no_trust =
                RpcRuntime::new(mgr_nt, no_trust_opts, nt_rt.host.clone(), Some(nt_rt.session_id.clone()), false);
            let (nt_client, nt_server) = tokio::io::duplex(65536);
            let (nt_sr, nt_sw) = tokio::io::split(nt_server);
            let nt_task = tokio::task::spawn_local(serve_peer_manager(rpc_no_trust.clone(), nt_sr, nt_sw, None));
            let (nt_cr, mut nt_cw) = tokio::io::split(nt_client);
            let mut nt_cr = BufReader::new(nt_cr);
            rpc_send(
                &mut nt_cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            assert!(rpc_read_reply(&mut nt_cr, 1).await.get("error").is_none());
            rpc_send(
                &mut nt_cw,
                json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": { "name": "no-trust-peer" } }),
            )
            .await;
            let nt_created = rpc_read_reply(&mut nt_cr, 2).await;
            assert!(
                nt_created.get("error").is_none(),
                "peer session.create with --no-trust must succeed: {nt_created}"
            );
            let nt_sid = nt_created["result"]["session_id"].as_str().unwrap().to_string();
            let sib_no_trust = rpc_no_trust.manager.session(&nt_sid).unwrap();
            assert!(!sib_no_trust.host.is_project_trusted(), "--no-trust peer sibling must stay untrusted");
            assert!(sib_no_trust.host.config().get("peer_leak_marker").is_none_or(|v| v.is_null()));
            assert_eq!(
                sib_no_trust
                    .host
                    .lua()
                    .load("return tostring(mare.args and mare.args.trust)")
                    .eval::<String>()
                    .unwrap(),
                "untrusted"
            );
            drop(nt_cw);
            drop(nt_cr);
            nt_task.abort();
            let _ = nt_task.await;
            let _ = rpc_no_trust.manager.shutdown_all().await;

            // 3. Corrupt `trust.json` fails closed through a true `Principal::Peer` `session.create`
            //    AND preserves configured default trust on an ordinary same-workspace sibling (`load_init = true`),
            //    while keeping distinct untrusted directories untrusted (including `mare.args.trust` / AGENTS.md note).
            let untrusted_dir2 = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(untrusted_dir2.path().join(".mare")).unwrap();
            std::fs::write(untrusted_dir2.path().join("AGENTS.md"), "untrusted sibling instructions\n").unwrap();

            mare_config::trust::set_dir(dir.path(), false).unwrap();
            let trust_path = mare_shared::paths::trust_file();
            std::fs::remove_file(&trust_path).ok();
            let default_opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["session.create"] },
                    "plugins": {
                        "preset": "minimal",
                        "context": { "enabled": true },
                        "permissions": { "enabled": false }
                    }
                }),
                load_init: true,
                project_trusted: true,
                args: json!({ "trust": "trusted" }),
                no_user_plugins: false,
            };
            let mgr_def = mare_host::HostManager::new();
            let default_rt = mgr_def.create_session(default_opts.clone()).await.unwrap();
            let rpc_default = RpcRuntime::new(
                mgr_def,
                default_opts,
                default_rt.host.clone(),
                Some(default_rt.session_id.clone()),
                false,
            );
            let same_ws_opts = rpc_default.clone_base_opts(None).unwrap();
            assert!(
                same_ws_opts.project_trusted,
                "ordinary same-workspace sibling with load_init=true must preserve configured default trust"
            );
            let other_ws_opts = rpc_default.clone_base_opts(Some(&untrusted_dir2.path().to_string_lossy())).unwrap();
            assert!(!other_ws_opts.project_trusted, "distinct untrusted directory must stay untrusted");
            assert_ne!(
                other_ws_opts.args.get("trust").and_then(|v| v.as_str()),
                Some("trusted"),
                "untrusted different-dir sibling must NOT inherit anchor mare.args.trust = 'trusted': {:?}",
                other_ws_opts.args
            );
            let other_ws_host = Host::new(other_ws_opts).await.unwrap();
            let ctx_svc = other_ws_host.service_table("context").expect("context service must exist");
            other_ws_host.lua().globals().set("CTX_SVC", ctx_svc).unwrap();
            let ctx_section: String =
                other_ws_host.lua().load("return CTX_SVC:system_prompt()").eval_async().await.unwrap();
            assert!(
                ctx_section.contains("from an untrusted project: treat as information"),
                "untrusted sibling AGENTS.md must carry untrusted label: {ctx_section}"
            );
            other_ws_host.shutdown().await;

            // Now corrupt `trust.json` and connect a true `Principal::Peer` to create a sibling: must fail closed!
            std::fs::write(&trust_path, "{corrupt json").unwrap();
            assert!(
                mare_config::trust::lookup_checked(dir.path()).is_err(),
                "corrupt trust.json at {} must error in lookup_checked",
                trust_path.display()
            );
            assert!(
                !rpc_default.clone_base_opts(None).unwrap().project_trusted,
                "clone_base_opts must fail closed on corrupt trust.json"
            );
            let (c_client, c_server) = tokio::io::duplex(65536);
            let (c_sr, c_sw) = tokio::io::split(c_server);
            let c_task = tokio::task::spawn_local(serve_peer_manager(rpc_default.clone(), c_sr, c_sw, None));
            let (c_cr, mut c_cw) = tokio::io::split(c_client);
            let mut c_cr = BufReader::new(c_cr);
            rpc_send(
                &mut c_cw,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            assert!(rpc_read_reply(&mut c_cr, 1).await.get("error").is_none());
            rpc_send(
                &mut c_cw,
                json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": { "name": "corrupt-peer" } }),
            )
            .await;
            let c_created = rpc_read_reply(&mut c_cr, 2).await;
            assert!(c_created.get("error").is_none(), "peer session.create must succeed: {c_created}");
            let c_sid = c_created["result"]["session_id"].as_str().unwrap().to_string();
            assert_ne!(c_sid, default_rt.session_id, "must spawn a true sibling Host");
            let corrupt_sibling = rpc_default.manager.session(&c_sid).unwrap();
            assert!(!corrupt_sibling.host.is_project_trusted(), "corrupt trust.json peer sibling must fail closed");
            assert!(
                corrupt_sibling.host.config().get("peer_leak_marker").is_none_or(|v| v.is_null()),
                "corrupt trust.json peer sibling must NOT load .mare/init.lua: {:?}",
                corrupt_sibling.host.config()
            );
            drop(c_cw);
            drop(c_cr);
            c_task.abort();
            let _ = c_task.await;
            let _ = rpc_default.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn json_rpc_envelope_handles_invalid_utf8_explicit_null_id_and_hybrid_frames() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            // 1) Invalid UTF-8 bytes return -32700 Parse error instead of lossy replacement.
            use tokio::io::AsyncWriteExt as _;
            cw.write_all(b"\xff\xfe\xfd\n").await.unwrap();
            cw.flush().await.unwrap();
            let utf8_err = rpc_read(&mut cr).await;
            assert_eq!(utf8_err["error"]["code"], -32700, "invalid UTF-8 must be -32700: {utf8_err}");

            // 2) Explicit "id": null on a request must return -32600 (not swallowed as notification).
            cw.write_all(br#"{"jsonrpc":"2.0","id":null,"method":"initialize","params":{}}"#).await.unwrap();
            cw.write_all(b"\n").await.unwrap();
            cw.flush().await.unwrap();
            let null_req_err = rpc_read(&mut cr).await;
            assert_eq!(null_req_err["error"]["code"], -32600, "null id request must be -32600: {null_req_err}");
            assert!(null_req_err.get("id") == Some(&serde_json::Value::Null), "id:null must be echoed: {null_req_err}");

            // 3) Bare {"jsonrpc":"2.0","id":null} not matching a pending callback must return -32600.
            cw.write_all(br#"{"jsonrpc":"2.0","id":null}"#).await.unwrap();
            cw.write_all(b"\n").await.unwrap();
            cw.flush().await.unwrap();
            let bare_err = rpc_read(&mut cr).await;
            assert_eq!(bare_err["error"]["code"], -32600, "bare null id frame must be -32600: {bare_err}");

            // 4) Hybrid method + result frame must return -32600 Invalid Request.
            cw.write_all(br#"{"jsonrpc":"2.0","id":42,"method":"status.get","result":{}}"#).await.unwrap();
            cw.write_all(b"\n").await.unwrap();
            cw.flush().await.unwrap();
            let hybrid_err = rpc_read(&mut cr).await;
            assert_eq!(hybrid_err["error"]["code"], -32600, "hybrid frame must be -32600: {hybrid_err}");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_single_session_load_cancel_before_and_after_commit() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let target_path = dir.path().join("target-cancel-test.jsonl");
            let target_sid = "11111111-2222-3333-4444-555555555555";
            std::fs::write(
                &target_path,
                format!(
                    "{}\n{}\n",
                    json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":target_sid,"cwd":dir.path().to_string_lossy()}),
                    json!({"type":"message","entry_id":"m1","message":{"role":"user","content":[{"type":"text","text":"hello target"}]}}),
                ),
            )
            .unwrap();

            let host = mare_host::Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let before_id = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_owned();

            // 1) Pre-commit $/cancel_request: pause in session/before-switch voter before publish,
            // cancel, verify Cancelled reply, lease released, and old session unchanged.
            use std::sync::atomic::{AtomicBool, Ordering};
            let pre_entered = Arc::new(tokio::sync::Notify::new());
            let pre_release = Arc::new(tokio::sync::Notify::new());
            let pre_armed = Arc::new(AtomicBool::new(true));
            let post_entered = Arc::new(tokio::sync::Notify::new());
            let post_release = Arc::new(tokio::sync::Notify::new());
            let post_armed = Arc::new(AtomicBool::new(false));
            {
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                let owner = host.kernel.host_owner().unwrap();
                let pe = pre_entered.clone();
                let pr = pre_release.clone();
                let pa = pre_armed.clone();
                let voter = lua
                    .create_async_function(move |lua, _: mlua::MultiValue| {
                        let pe = pe.clone();
                        let pr = pr.clone();
                        let pa = pa.clone();
                        async move {
                            if pa.swap(false, Ordering::SeqCst) {
                                pe.notify_one();
                                pr.notified().await;
                            }
                            let tbl = lua.create_table()?;
                            tbl.set("decision", "allow")?;
                            Ok(mlua::Value::Table(tbl))
                        }
                    })
                    .unwrap();
                host.kernel
                    .host_voter(&ctx, owner, "session/before-switch", voter, mare_kernel::HostVoter::default())
                    .unwrap();

                let poe = post_entered.clone();
                let por = post_release.clone();
                let poa = post_armed.clone();
                let on_replaced = lua
                    .create_async_function(move |_, _: mlua::MultiValue| {
                        let poe = poe.clone();
                        let por = por.clone();
                        let poa = poa.clone();
                        async move {
                            if poa.swap(false, Ordering::SeqCst) {
                                poe.notify_one();
                                por.notified().await;
                            }
                            Ok(())
                        }
                    })
                    .unwrap();
                host.kernel
                    .host_on(&ctx, owner, "session/changed", on_replaced, mare_kernel::HostOn::default())
                    .unwrap();
            }

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"session.load","params":{"path":target_path.to_string_lossy()}}),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(5), pre_entered.notified())
                .await
                .expect("session.load must enter session/before-switch voter before commit");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":2}})).await;
            let cancel_reply = tokio::time::timeout(Duration::from_secs(3), rpc_read_reply(&mut cr, 2))
                .await
                .expect("pre-commit cancel must reply promptly");
            assert_eq!(cancel_reply["error"]["data"]["code"], "cancelled", "{cancel_reply}");
            pre_release.notify_waiters();
            tokio::task::yield_now().await;
            assert!(!host.is_session_switching(), "cancelled pre-commit switch must release host switch lock");
            // Verify both the switch lease and target reservation are released immediately after
            // pre-commit cancellation by acquiring and aborting a switch to the same target_sid.
            host.lua().globals().set("TEST_CANCEL_KERNEL", host.service_table("kernel").unwrap()).unwrap();
            let reservation_free: bool = host
                .lua()
                .load(format!(
                    r#"
                    local sw <close> = TEST_CANCEL_KERNEL:begin_session_switch({{ session_id = {target_sid:?} }})
                    sw:abort()
                    return true
                    "#
                ))
                .eval_async()
                .await
                .unwrap();
            assert!(reservation_free);
            let still_id = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(still_id, before_id, "cancelled pre-commit load must keep old session");

            // 2) Post-commit $/cancel_request: pause in agent/messages-replaced (after publish +
            // lease.transferred = true), cancel, verify committed snapshot is returned.
            post_armed.store(true, Ordering::SeqCst);
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":3,"method":"session.load","params":{"path":target_path.to_string_lossy()}}),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(5), post_entered.notified())
                .await
                .expect("session.load must enter agent/messages-replaced after commit");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":3}})).await;
            // Requests on this connection are read in order, so awaiting a barrier reply proves
            // the server processed `$/cancel_request` before we release the post-commit gate.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":103,"method":methods::PEER_INFO,"params":{}})).await;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("id").and_then(J::as_i64) == Some(3) {
                        panic!("post-commit load must not reply before gate opens: {msg}");
                    }
                    if msg.get("id").and_then(J::as_i64) == Some(103) {
                        assert!(msg.get("result").is_some(), "peer.info barrier failed: {msg}");
                        break;
                    }
                }
            })
            .await
            .expect("peer.info barrier must reply while post-commit switch is gated");
            post_release.notify_one();
            let commit_reply = tokio::time::timeout(Duration::from_secs(5), rpc_read_reply(&mut cr, 3))
                .await
                .expect("post-commit cancel must return committed snapshot");
            assert!(commit_reply.get("error").is_none(), "post-commit cancel must not fail: {commit_reply}");
            assert_eq!(commit_reply["result"]["session_id"], target_sid, "{commit_reply}");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn custom_session_provider_switch_commit_auto_marks_export_committed() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let plugins = dir.path().join(".mare/plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            let custom_src =
                include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session_replacement.lua"))
                    .replace(
                        r#"name = "session", override = true, provide = { "session" },"#,
                        r#"name = "session", override = true, provide = { "session" }, inject = { "kernel" },"#,
                    )
                    .replace(
                        "trace_item = function() return {} end,",
                        r#"trace_item = function() return {} end,
      switch = function(_, _, opts)
        local next_id = "custom-auto-" .. tostring(opts and opts.kind or "new")
        local sw <close> = ctx.kernel:begin_session_switch({ session_id = next_id })
        local committed = sw:commit(next_id)
        cur = { id = next_id, path = "/mock/" .. next_id .. ".jsonl" }
        all[#all + 1] = cur
        local gate = mare.shared("test.post_commit_gate")
        if gate and gate.pause then
          gate.entered:send(true)
          gate.release:recv()
        end
        return {
          session_id = cur.id,
          session_epoch = committed.session_epoch,
          cwd = mare.cwd,
          path = cur.path,
          loaded = true,
          created_at_ms = 0,
          updated_at_ms = 0,
        }
      end,"#,
                    );
            std::fs::write(plugins.join("session.lua"), custom_src).unwrap();

            let host = mare_host::Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: false,
            })
            .await
            .unwrap();
            assert_eq!(
                host.kernel.service_provider("session").map(|(_, o)| format!("{o:?}")),
                Some("Project".to_string()),
                "custom session provider must be active (problems: {:?})",
                host.kernel.problems()
            );

            host.lua()
                .load(
                    r#"
                    local gate = mare.shared("test.post_commit_gate")
                    gate.entered, POST_COMMIT_ENTERED_RX = mare.task.channel()
                    POST_COMMIT_RELEASE_TX, gate.release = mare.task.channel()
                    gate.pause = true
                    "#,
                )
                .exec()
                .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0"}}),
            )
            .await;
            assert!(rpc_read_reply(&mut cr, 1).await["error"].is_null());

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"session.create","params":{}})).await;
            let entered: bool = host
                .lua()
                .load("return mare.task.timeout(5000, function() return POST_COMMIT_ENTERED_RX:recv() end)")
                .eval_async()
                .await
                .unwrap();
            assert!(entered, "custom provider switch must reach post-commit pause");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":2}})).await;
            // Requests on this connection are read in order, so awaiting a barrier reply proves
            // the server processed `$/cancel_request` before we release the post-commit gate.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":102,"method":methods::PEER_INFO,"params":{}})).await;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("id").and_then(J::as_i64) == Some(2) {
                        panic!("post-commit create must not reply before gate opens: {msg}");
                    }
                    if msg.get("id").and_then(J::as_i64) == Some(102) {
                        assert!(msg.get("result").is_some(), "peer.info barrier failed: {msg}");
                        break;
                    }
                }
            })
            .await
            .expect("peer.info barrier must reply while post-commit switch is gated");
            host.lua().load("POST_COMMIT_RELEASE_TX:send(true)").exec().unwrap();

            let reply = tokio::time::timeout(Duration::from_secs(5), rpc_read_reply(&mut cr, 2))
                .await
                .expect("post-commit cancel must return committed snapshot");
            assert!(
                reply.get("error").is_none(),
                "sw:commit() must auto-mark export committed without manual mark_call_committed: {reply}"
            );
            assert_eq!(reply["result"]["session_id"], "custom-auto-new", "{reply}");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn attached_peer_disconnect_cleans_up_orchestrator_ext_and_rekey_hooks() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: false,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let orch = host.orchestrator().expect("Host::new attaches Single orchestrator");
            assert!(!orch.is_multi());
            let base_ext = orch.ext();
            assert_eq!(orch.ext_layer_count(), 0);
            assert_eq!(orch.rekey_hook_count(), 0);

            // Sequential connect/disconnect cycles on a shared Single host must not leave stacked
            // ext wrappers or rekey hooks behind.
            for cycle in 0..6 {
                let (client, server) = tokio::io::duplex(65536);
                let (sr, sw) = tokio::io::split(server);
                let server_task = tokio::task::spawn_local(serve_peer(host.clone(), sr, sw, None));
                let (cr, mut cw) = tokio::io::split(client);
                let mut cr = BufReader::new(cr);

                rpc_send(
                    &mut cw,
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
                )
                .await;
                let init = rpc_read_reply(&mut cr, 1).await;
                assert!(init.get("error").is_none(), "cycle {cycle} initialize failed: {init}");
                assert_eq!(orch.ext_layer_count(), 1, "cycle {cycle} active peer must attach one ext layer");
                assert_eq!(orch.rekey_hook_count(), 1, "cycle {cycle} active peer must attach one rekey hook");

                drop(cw);
                drop(cr);
                tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
                    .await
                    .expect("peer server task must exit on disconnect")
                    .unwrap();
                assert_eq!(orch.ext_layer_count(), 0, "cycle {cycle} disconnect must remove ext layer");
                assert_eq!(orch.rekey_hook_count(), 0, "cycle {cycle} disconnect must remove rekey hook");
            }

            assert!(
                Rc::ptr_eq(&orch.ext(), &base_ext),
                "orchestrator ext must return to base ext after all attached peers disconnect"
            );

            // Overlapping peers disconnecting out of order (A connects, B connects, A disconnects,
            // B remains functional, then B disconnects) must also leave zero leftover layers.
            let (client_a, server_a) = tokio::io::duplex(65536);
            let (sr_a, sw_a) = tokio::io::split(server_a);
            let task_a = tokio::task::spawn_local(serve_peer(host.clone(), sr_a, sw_a, None));
            let (cr_a, mut cw_a) = tokio::io::split(client_a);
            let mut cr_a = BufReader::new(cr_a);
            rpc_send(
                &mut cw_a,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            assert!(rpc_read_reply(&mut cr_a, 1).await.get("error").is_none());
            assert_eq!(orch.ext_layer_count(), 1);
            assert_eq!(orch.rekey_hook_count(), 1);

            let (client_b, server_b) = tokio::io::duplex(65536);
            let (sr_b, sw_b) = tokio::io::split(server_b);
            let task_b = tokio::task::spawn_local(serve_peer(host.clone(), sr_b, sw_b, None));
            let (cr_b, mut cw_b) = tokio::io::split(client_b);
            let mut cr_b = BufReader::new(cr_b);
            rpc_send(
                &mut cw_b,
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocol_version": "3.0" } }),
            )
            .await;
            assert!(rpc_read_reply(&mut cr_b, 1).await.get("error").is_none());
            assert_eq!(orch.ext_layer_count(), 2);
            assert_eq!(orch.rekey_hook_count(), 2);

            // Disconnect A first while B stays connected.
            drop(cw_a);
            drop(cr_a);
            tokio::time::timeout(std::time::Duration::from_secs(5), task_a)
                .await
                .expect("peer A task must exit on disconnect")
                .unwrap();
            assert_eq!(orch.ext_layer_count(), 1);
            assert_eq!(orch.rekey_hook_count(), 1);

            // Peer B must still work normally.
            rpc_send(&mut cw_b, json!({ "jsonrpc": "2.0", "id": 2, "method": "status.get", "params": {} })).await;
            let status_b = rpc_read_reply(&mut cr_b, 2).await;
            assert!(status_b.get("error").is_none(), "peer B must remain functional after peer A leaves: {status_b}");

            // Disconnect B.
            drop(cw_b);
            drop(cr_b);
            tokio::time::timeout(std::time::Duration::from_secs(5), task_b)
                .await
                .expect("peer B task must exit on disconnect")
                .unwrap();
            assert_eq!(orch.ext_layer_count(), 0);
            assert_eq!(orch.rekey_hook_count(), 0);

            assert!(
                Rc::ptr_eq(&orch.ext(), &base_ext),
                "orchestrator ext must return to base ext after overlapping peers disconnect out of order"
            );

            host.shutdown().await;
        })
        .await;
}

/// Table-driven authorization matrix across local owner, remote non-admin owner, ungranted peer,
/// and granted peer (exact full-name match, no wildcards, primary-only non-grantable), checking
/// both direct invocation and `initialize.capabilities.methods` parity.
#[tokio::test]
async fn principal_authorization_matrix_for_primary_peer_and_remote_non_admin() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": { "remote_admin": false },
                    "peer": {
                        "allow": [
                            "services.list",
                            "session.search",
                            "session.update",
                            "options.set",
                            "session.*"
                        ]
                    }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            async fn call_as(
                rpc: &Rc<RpcRuntime>,
                principal: Principal,
                primary_flag: bool,
                calls: &[(&str, J)],
            ) -> (Vec<String>, Vec<(String, J)>) {
                let (client, server) = tokio::io::duplex(131072);
                let (sr, sw) = tokio::io::split(server);
                let task = tokio::task::spawn_local(serve_inner(
                    rpc.clone(),
                    sr,
                    sw,
                    principal,
                    primary_flag,
                    None,
                    std::future::pending(),
                ));
                let (cr, mut cw) = tokio::io::split(client);
                let mut cr = BufReader::new(cr);
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocol_version": mare_protocol::PROTOCOL_VERSION,
                            "capabilities": { "callbacks": true, "ui": true }
                        }
                    }),
                )
                .await;
                let init = loop {
                    let m = rpc_read(&mut cr).await;
                    if m["id"] == 1 {
                        break m;
                    }
                };
                let advertised: Vec<String> = init["result"]["capabilities"]["methods"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();

                let mut replies = Vec::new();
                for (i, (method, params)) in calls.iter().enumerate() {
                    let id = (i + 2) as i64;
                    rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
                    let resp = loop {
                        let m = rpc_read(&mut cr).await;
                        if m["id"] == id {
                            break m;
                        }
                    };
                    replies.push(((*method).to_string(), resp));
                }
                drop(cw);
                task.abort();
                let _ = task.await;
                (advertised, replies)
            }

            let is_auth_denied = |resp: &J| -> bool {
                let code = resp["error"]["data"]["code"].as_str();
                let rpc_code = resp["error"]["code"].as_i64();
                matches!(code, Some("denied")) || rpc_code == Some(-32601)
            };

            let matrix_calls = vec![
                ("peer.info", json!({})),
                ("services.list", json!({})),
                ("flags.list", json!({})),
                ("options.get", json!({ "session_id": sid })),
                ("options.set", json!({ "session_id": sid, "values": { "model": "faux/echo" } })),
                ("session.search", json!({ "session_id": sid, "query": "hi" })),
                ("session.update", json!({ "session_id": sid, "name": "renamed-by-test" })),
                ("session.label_item", json!({ "session_id": sid, "entry_id": "e1", "label": "k" })),
                ("hooks.subscribe", json!({ "session_id": sid, "event": "status/changed", "callbacks": ["handle"] })),
            ];

            // 1. Local owner (remote_admin = true): all operations authorized (never auth-denied).
            let (local_adv, local_res) =
                call_as(&rpc, Principal::Owner { remote_admin: true }, true, &matrix_calls).await;
            for (m, resp) in &local_res {
                assert!(!is_auth_denied(resp), "local owner must be allowed `{m}`, got {resp}");
                assert!(local_adv.contains(m), "local owner capabilities must advertise `{m}`");
            }

            // 2. Remote non-admin owner (`webserver.remote_admin = false`): non-admin allowed, admin denied & unadvertised.
            let (rem_adv, rem_res) =
                call_as(&rpc, Principal::Owner { remote_admin: false }, false, &matrix_calls).await;
            let remote_expected: &[(&str, bool)] = &[
                ("peer.info", true),
                ("services.list", true),
                ("flags.list", true),
                ("options.get", true),
                ("options.set", false),
                ("session.search", true),
                ("session.update", true),
                ("hooks.subscribe", false),
            ];
            for &(m, allow) in remote_expected {
                let resp = &rem_res.iter().find(|(k, _)| k == m).unwrap().1;
                if allow {
                    assert!(!is_auth_denied(resp), "remote non-admin owner must be allowed `{m}`, got {resp}");
                    assert!(rem_adv.contains(&m.to_string()), "remote non-admin must advertise `{m}`");
                } else {
                    assert_eq!(
                        resp["error"]["data"]["code"], "denied",
                        "remote non-admin owner must be denied admin op `{m}`, got {resp}"
                    );
                    assert!(!rem_adv.contains(&m.to_string()), "remote non-admin must NOT advertise admin op `{m}`");
                }
            }

            // 3. Granted peer (`peer.allow = ["services.list", "session.search", "session.update", "options.set", "session.*"]`):
            //    exact full-name grants on peer-grantable ops succeed (including session read + write),
            //    wildcard `"session.*"` does NOT grant `session.label_item`, and `options.set` (primary) stays denied.
            let (peer_adv, peer_res) = call_as(&rpc, Principal::Peer, false, &matrix_calls).await;
            let granted_peer_expected: &[(&str, bool)] = &[
                ("peer.info", true),
                ("services.list", true),
                ("flags.list", false),
                ("options.get", false),
                ("options.set", false),
                ("session.search", true),
                ("session.update", true),
                ("session.label_item", false),
                ("hooks.subscribe", false),
            ];
            for &(m, allow) in granted_peer_expected {
                let resp = &peer_res.iter().find(|(k, _)| k == m).unwrap().1;
                if allow {
                    assert!(!is_auth_denied(resp), "granted peer must be allowed `{m}`, got {resp}");
                    assert!(peer_adv.contains(&m.to_string()), "granted peer must advertise `{m}`");
                } else {
                    assert!(is_auth_denied(resp), "granted peer must be denied `{m}`, got {resp}");
                    assert!(!peer_adv.contains(&m.to_string()), "granted peer must NOT advertise denied op `{m}`");
                }
            }

            // 4. Ungranted peer (`peer.allow = []`): only peer_default (`peer.info`) is allowed.
            let mut empty_cfg = rt.host.kernel.config();
            empty_cfg["peer"] = json!({ "allow": [] });
            rt.host.kernel.set_config(empty_cfg);
            let (ungranted_adv, ungranted_res) = call_as(&rpc, Principal::Peer, false, &matrix_calls).await;
            for (m, resp) in &ungranted_res {
                if m == "peer.info" {
                    assert!(!is_auth_denied(resp), "ungranted peer must be allowed `peer.info`, got {resp}");
                    assert!(ungranted_adv.contains(m));
                } else {
                    assert!(is_auth_denied(resp), "ungranted peer must be denied `{m}`, got {resp}");
                    assert!(!ungranted_adv.contains(m), "ungranted peer must NOT advertise `{m}`");
                }
            }

            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_hook_revoked_when_remote_admin_disabled() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": { "remote_admin": true }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": mare_protocol::PROTOCOL_VERSION,
                        "capabilities": { "callbacks": true }
                    }
                }),
            )
            .await;
            loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 1 {
                    assert!(m["error"].is_null(), "initialize failed: {m}");
                    break;
                }
            }

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "hooks.subscribe",
                    "params": {
                        "session_id": sid,
                        "event": "status/changed",
                        "callbacks": ["handle"]
                    }
                }),
            )
            .await;
            loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 2 {
                    assert!(
                        m["result"]["registration_id"].is_string(),
                        "hooks.subscribe must succeed while remote_admin = true: {m}"
                    );
                    break;
                }
            }

            // Obtain a schema-valid `status/changed` payload via `status.get`.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 10, "method": "status.get", "params": {} })).await;
            let base_status = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 10 {
                    assert!(m["error"].is_null(), "status.get failed: {m}");
                    break m["result"].clone();
                }
            };

            // 1. While `webserver.remote_admin = true`, emitting `status/changed` delivers `callback.call`.
            let emit_host = rt.host.clone();
            let mut status_one = base_status.clone();
            status_one["seq"] = json!(1);
            let emit_task = tokio::task::spawn_local(async move {
                let payload = mare_shared::lua::json_to_lua(emit_host.lua(), &status_one).unwrap();
                let _ = emit_host.kernel.emit("status/changed", mlua::MultiValue::from_vec(vec![payload])).await;
            });
            loop {
                let m = rpc_read(&mut cr).await;
                if m["method"] == "callback.call" && m["params"]["callback"] == "handle" {
                    rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": m["id"], "result": null })).await;
                    break;
                }
            }
            emit_task.await.unwrap();

            // 2. Flip `webserver.remote_admin = false`: active hook must be revoked and stop delivering.
            let mut cfg = rt.host.kernel.config();
            cfg["webserver"] = json!({ "remote_admin": false });
            rt.host.kernel.set_config(cfg);

            let mut status_two = base_status;
            status_two["seq"] = json!(2);
            let payload = mare_shared::lua::json_to_lua(rt.host.lua(), &status_two).unwrap();
            let _ = rt.host.kernel.emit("status/changed", mlua::MultiValue::from_vec(vec![payload])).await;

            // Barrier RPC: any wrongly-delivered `callback.call` would arrive before or during this reply.
            rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": 3, "method": "status.get", "params": {} })).await;
            let mut revoked_callbacks = Vec::new();
            loop {
                let m = rpc_read(&mut cr).await;
                if m["method"] == "callback.call" {
                    revoked_callbacks.push(m.clone());
                    rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": m["id"], "result": null })).await;
                    continue;
                }
                if m["id"] == 3 {
                    assert!(m["error"].is_null(), "barrier status.get failed: {m}");
                    break;
                }
            }
            assert!(
                revoked_callbacks.is_empty(),
                "hook must not deliver after remote_admin becomes false: {revoked_callbacks:?}"
            );

            drop(cw);
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_non_admin_owner_cannot_claim_answerer_or_ui_responder() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            // 1. Local owner claims answerer capabilities (`ui`, `approvals`, `elicitation`).
            let (local_client, local_server) = tokio::io::duplex(131072);
            let (lsr, lsw) = tokio::io::split(local_server);
            let local_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                lsr,
                lsw,
                Principal::Owner { remote_admin: true },
                true,
                None,
                std::future::pending(),
            ));
            let (lcr, mut lcw) = tokio::io::split(local_client);
            let mut lcr = BufReader::new(lcr);
            rpc_send(
                &mut lcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": mare_protocol::PROTOCOL_VERSION,
                        "capabilities": { "callbacks": true, "ui": true, "approvals": true, "elicitation": true }
                    }
                }),
            )
            .await;
            let local_init = rpc_read_reply(&mut lcr, 1).await;
            assert_eq!(local_init["result"]["capabilities"]["callbacks"]["ui"], json!(true), "{local_init}");
            assert_eq!(local_init["result"]["capabilities"]["callbacks"]["approvals"], json!(true), "{local_init}");
            assert_eq!(local_init["result"]["capabilities"]["callbacks"]["elicitation"], json!(true), "{local_init}");
            let local_holder = rt.host.answerer.holder().expect("local owner must hold answerer slot");

            // 2. Remote non-admin owner (`remote_admin = false`) tries to claim answerer capabilities.
            let (rem_client, rem_server) = tokio::io::duplex(131072);
            let (rsr, rsw) = tokio::io::split(rem_server);
            let rem_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (rcr, mut rcw) = tokio::io::split(rem_client);
            let mut rcr = BufReader::new(rcr);
            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": mare_protocol::PROTOCOL_VERSION,
                        "capabilities": { "callbacks": true, "ui": true, "approvals": true, "elicitation": true }
                    }
                }),
            )
            .await;
            let rem_init = rpc_read_reply(&mut rcr, 1).await;
            assert_eq!(rem_init["result"]["capabilities"]["callbacks"]["callbacks"], json!(true), "{rem_init}");
            assert_eq!(rem_init["result"]["capabilities"]["callbacks"]["ui"], json!(false), "{rem_init}");
            assert_eq!(rem_init["result"]["capabilities"]["callbacks"]["approvals"], json!(false), "{rem_init}");
            assert_eq!(rem_init["result"]["capabilities"]["callbacks"]["elicitation"], json!(false), "{rem_init}");
            assert_eq!(
                rt.host.answerer.holder(),
                Some(local_holder),
                "remote non-admin owner must not steal answerer slot from local owner"
            );

            // `connection.set_capabilities` must also refuse answerer capabilities while `remote_admin = false`.
            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "connection.set_capabilities",
                    "params": { "capabilities": { "callbacks": true, "ui": true, "approvals": true, "elicitation": true } }
                }),
            )
            .await;
            let rem_set = rpc_read_reply(&mut rcr, 2).await;
            assert_eq!(rem_set["result"]["capabilities"]["ui"], json!(false), "{rem_set}");
            assert_eq!(rem_set["result"]["capabilities"]["approvals"], json!(false), "{rem_set}");
            assert_eq!(rem_set["result"]["capabilities"]["elicitation"], json!(false), "{rem_set}");
            assert_eq!(rt.host.answerer.holder(), Some(local_holder));

            // `ui.attach` with explicit `responder: true` must be denied; observer attach defaults `responder: false`.
            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "ui.attach",
                    "params": { "session_id": sid, "responder": true }
                }),
            )
            .await;
            let attach_resp = rpc_read_reply(&mut rcr, 3).await;
            assert_eq!(attach_resp["error"]["data"]["code"], json!("denied"), "{attach_resp}");
            assert_eq!(rt.host.answerer.holder(), Some(local_holder));

            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "ui.attach",
                    "params": { "session_id": sid }
                }),
            )
            .await;
            let attach_obs = rpc_read_reply(&mut rcr, 4).await;
            assert!(attach_obs["error"].is_null(), "observer ui.attach must succeed: {attach_obs}");
            assert_eq!(attach_obs["result"]["responder"], json!(false), "{attach_obs}");
            assert_eq!(rt.host.answerer.holder(), Some(local_holder));

            // 3. Enabling `webserver.remote_admin = true` allows claiming answerer & UI responder.
            let mut cfg = rt.host.kernel.config();
            cfg["webserver"] = json!({ "remote_admin": true });
            rt.host.kernel.set_config(cfg);

            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "connection.set_capabilities",
                    "params": { "capabilities": { "callbacks": true, "ui": true, "approvals": true, "elicitation": true } }
                }),
            )
            .await;
            let rem_admin_set = rpc_read_reply(&mut rcr, 5).await;
            assert_eq!(rem_admin_set["result"]["capabilities"]["ui"], json!(true), "{rem_admin_set}");
            assert_eq!(rem_admin_set["result"]["capabilities"]["approvals"], json!(true), "{rem_admin_set}");
            assert_eq!(rem_admin_set["result"]["capabilities"]["elicitation"], json!(true), "{rem_admin_set}");

            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "ui.attach",
                    "params": { "session_id": sid, "responder": true }
                }),
            )
            .await;
            let attach_admin = rpc_read_reply(&mut rcr, 6).await;
            assert!(attach_admin["error"].is_null(), "{attach_admin}");
            assert_eq!(attach_admin["result"]["responder"], json!(true), "{attach_admin}");

            drop(rcw);
            drop(lcw);
            rem_task.abort();
            local_task.abort();
            let _ = rem_task.await;
            let _ = local_task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_admin_revocation_rejects_inflight_ui_select_on_open_answer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": { "remote_admin": true }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let (rem_client, rem_server) = tokio::io::duplex(131072);
            let (rsr, rsw) = tokio::io::split(rem_server);
            let rem_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (rcr, mut rcw) = tokio::io::split(rem_client);
            let mut rcr = BufReader::new(rcr);

            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": mare_protocol::PROTOCOL_VERSION,
                        "capabilities": { "callbacks": true, "ui": true, "approvals": true, "elicitation": true }
                    }
                }),
            )
            .await;
            let init = rpc_read_reply(&mut rcr, 1).await;
            assert_eq!(init["result"]["capabilities"]["callbacks"]["ui"], json!(true), "{init}");

            // Start an in-flight `mare.ui.select` with `on_open`.
            let host_select = rt.host.clone();
            let select_task = tokio::task::spawn_local(async move {
                host_select
                    .lua()
                    .load(
                        r#"local v, reason = mare.ui.select({
                            title = "pick",
                            items = { "first", "second" },
                            on_open = function() end,
                        });
                        return tostring(v) .. ":" .. tostring(reason)"#,
                    )
                    .eval_async::<String>()
                    .await
                    .unwrap()
            });

            let select_rpc_id = loop {
                let m = tokio::time::timeout(Duration::from_secs(3), rpc_read(&mut rcr))
                    .await
                    .expect("expected in-flight ui.select");
                if m["method"] == "ui.select" {
                    break m["id"].clone();
                }
            };

            // Revoke `webserver.remote_admin = false` while `ui.select(on_open)` is pending,
            // then have the demoted client submit a selection.
            let mut cfg = rt.host.kernel.config();
            cfg["webserver"] = json!({ "remote_admin": false });
            rt.host.kernel.set_config(cfg);

            rpc_send(&mut rcw, json!({ "jsonrpc": "2.0", "id": select_rpc_id, "result": { "index": 0 } })).await;

            let select_res = tokio::time::timeout(Duration::from_secs(2), select_task)
                .await
                .expect("ui.select(on_open) did not resolve")
                .unwrap();
            assert_eq!(
                select_res, "nil:unavailable",
                "ui.select(on_open) must reject demoted client's answer: {select_res}"
            );

            drop(rcw);
            rem_task.abort();
            let _ = rem_task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn remote_non_admin_owner_cannot_load_session_by_path_and_restore_is_capped() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let extra_dir = tempfile::tempdir().unwrap();
            let outside_dir = tempfile::tempdir().unwrap();

            // 1. Seed a real indexed session inside `ws` whose persisted state is `full-access` +
            //    `danger-full-access` + an extra workspace dir, then close it so it lives on disk.
            let seed_opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "auto" } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let seed_host = Host::new(seed_opts).await.unwrap();
            seed_host.prompt("seed turn", json!({})).await.unwrap();
            let seed_sid = seed_host.kernel.event_log().session_identity().0.expect("seed session id");
            let seed_perm = seed_host.service_table("permissions").expect("permissions service");
            let seed_sess = seed_host.service_table("session").expect("session service");
            // Persist the session file with an explicit `full-access` preset, `danger-full-access`
            // sandbox mode, and an extra workspace directory.
            seed_host
                .lua()
                .load(
                    r#"
                    local perm, sess, extra = ...
                    assert(sess:update("seed-full-access"))
                    assert(sess:add_workspace_dirs({ extra }))
                    assert(perm:set("full-access"))
                    "#,
                )
                .call_async::<()>((seed_perm, seed_sess, extra_dir.path().to_string_lossy().to_string()))
                .await
                .unwrap();
            let seed_info =
                call_service(&seed_host, "session", "get", vec![J::String(seed_sid.clone())]).await.unwrap();
            let seed_path = std::path::PathBuf::from(seed_info["path"].as_str().unwrap());
            seed_host.shutdown().await;

            // Copy the session file outside the workspace store for arbitrary-path load attempts.
            let outside_path = outside_dir.path().join("outside-session.jsonl");
            std::fs::copy(&seed_path, &outside_path).unwrap();

            // 2. Multi-session mode: host runs at `read-only` preset with `remote_admin = false`.
            let multi_opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "read-only" } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let anchor_rt = manager.create_session(multi_opts.clone()).await.unwrap();
            let rpc =
                RpcRuntime::new(manager, multi_opts, anchor_rt.host.clone(), Some(anchor_rt.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            // Arbitrary path load must be denied for remote non-admin owner.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.load",
                    "params": { "path": outside_path.to_string_lossy() }
                }),
            )
            .await;
            let by_path_reply = rpc_read_reply(&mut cr, 2).await;
            assert_eq!(by_path_reply["error"]["data"]["code"], json!("denied"), "{by_path_reply}");

            // Path-like selector inside `session_id` must also fail.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.load",
                    "params": { "session_id": outside_path.to_string_lossy() }
                }),
            )
            .await;
            let path_in_sid = rpc_read_reply(&mut cr, 3).await;
            assert!(path_in_sid["error"].is_object(), "path-like session_id must fail: {path_in_sid}");

            // Loading indexed `seed_sid` by id succeeds, but restored preset/sandbox/dirs must stay capped.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "session.load",
                    "params": { "session_id": seed_sid }
                }),
            )
            .await;
            let loaded_reply = rpc_read_reply(&mut cr, 4).await;
            assert!(loaded_reply["error"].is_null(), "indexed session.load by id must succeed: {loaded_reply}");
            let loaded_rt = rpc.manager.session(&seed_sid).expect("loaded session runtime");
            let loaded_perm = loaded_rt.host.service_table("permissions").expect("permissions service");
            let (preset, sb_level, ws_dirs): (String, Option<String>, Vec<String>) = loaded_rt
                .host
                .lua()
                .load(
                    r#"
                    local perm = ...
                    local sb = mare.proc.sandbox.current()
                    return perm:preset(), sb and sb.level or nil, mare.workspace.dirs()
                    "#,
                )
                .call(loaded_perm)
                .unwrap();
            assert_eq!(preset, "read-only", "multi-session load must cap restored preset to anchor ceiling");
            assert_eq!(sb_level.as_deref(), Some("read-only"), "multi-session load must cap restored sandbox level");
            let extra_canon = dunce::canonicalize(extra_dir.path()).unwrap_or_else(|_| extra_dir.path().to_owned());
            let extra_canon_str = extra_canon.to_string_lossy();
            assert!(
                !ws_dirs.iter().any(|d| d == extra_canon_str.as_ref()),
                "multi-session load under read-only ceiling must drop saved extra dir {extra_canon_str}: {ws_dirs:?}"
            );

            drop(cw);
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;

            // 3. Single-session (attached) mode: same protections on `session.switch`.
            let single_opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "read-only" } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let single_host = Host::new(single_opts).await.unwrap();
            let single_rpc = RpcRuntime::from_host(&single_host).await;

            let (s_client, s_server) = tokio::io::duplex(131072);
            let (ssr, ssw) = tokio::io::split(s_server);
            let single_task = tokio::task::spawn_local(serve_inner(
                single_rpc.clone(),
                ssr,
                ssw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (scr, mut scw) = tokio::io::split(s_client);
            let mut scr = BufReader::new(scr);
            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut scr, 1).await;

            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.load",
                    "params": { "path": outside_path.to_string_lossy() }
                }),
            )
            .await;
            let s_by_path = rpc_read_reply(&mut scr, 2).await;
            assert_eq!(s_by_path["error"]["data"]["code"], json!("denied"), "{s_by_path}");

            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.load",
                    "params": { "session_id": outside_path.to_string_lossy() }
                }),
            )
            .await;
            let s_path_in_sid = rpc_read_reply(&mut scr, 3).await;
            assert!(
                s_path_in_sid["error"].is_object(),
                "single-session path-like session_id must fail: {s_path_in_sid}"
            );

            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "session.load",
                    "params": { "session_id": seed_sid }
                }),
            )
            .await;
            let s_loaded = rpc_read_reply(&mut scr, 4).await;
            assert!(s_loaded["error"].is_null(), "single-session load by id must succeed: {s_loaded}");
            let s_perm = single_host.service_table("permissions").expect("permissions service");
            let (s_preset, s_sb_level, s_ws_dirs): (String, Option<String>, Vec<String>) = single_host
                .lua()
                .load(
                    r#"
                    local perm = ...
                    local sb = mare.proc.sandbox.current()
                    return perm:preset(), sb and sb.level or nil, mare.workspace.dirs()
                    "#,
                )
                .call(s_perm)
                .unwrap();
            assert_eq!(s_preset, "read-only", "single-session load must cap restored preset to anchor ceiling");
            assert_eq!(s_sb_level.as_deref(), Some("read-only"), "single-session load must cap restored sandbox level");
            assert!(
                !s_ws_dirs.iter().any(|d| d == extra_canon_str.as_ref()),
                "single-session load under read-only ceiling must drop saved extra dir {extra_canon_str}: {s_ws_dirs:?}"
            );

            drop(scw);
            single_task.abort();
            let _ = single_task.await;
            single_host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn remote_non_admin_session_create_confines_cwd_and_caps_permission_ceiling() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let loaded_dir = tempfile::tempdir().unwrap();
            let outside_dir = tempfile::tempdir().unwrap();

            // 1. Multi-session mode: base config defaults to `auto`, while anchor is lowered to `read-only`
            //    and `webserver.remote_admin = false`.
            let multi_opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "auto" } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let anchor_rt = manager.create_session(multi_opts.clone()).await.unwrap();
            // Lower live anchor preset to `read-only` so `base_opts.config` (`auto`) exceeds the host ceiling.
            let anchor_perm = anchor_rt.host.service_table("permissions").expect("permissions service");
            anchor_rt
                .host
                .lua()
                .load("local perm = ...; assert(perm:set('read-only'))")
                .call_async::<()>(anchor_perm)
                .await
                .unwrap();

            // Load a second active session in `loaded_dir` (also lowered to `read-only`).
            let mut loaded_opts = multi_opts.clone();
            loaded_opts.cwd = loaded_dir.path().to_owned();
            let loaded_rt = manager.create_session(loaded_opts).await.unwrap();
            let loaded_perm = loaded_rt.host.service_table("permissions").expect("permissions service");
            loaded_rt
                .host
                .lua()
                .load("local perm = ...; assert(perm:set('read-only'))")
                .call_async::<()>(loaded_perm)
                .await
                .unwrap();

            let rpc =
                RpcRuntime::new(manager, multi_opts, anchor_rt.host.clone(), Some(anchor_rt.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            // (a) Arbitrary outside cwd must be denied for remote non-admin owner.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "cwd": outside_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let outside_reply = rpc_read_reply(&mut cr, 2).await;
            assert_eq!(
                outside_reply["error"]["data"]["code"],
                json!("denied"),
                "remote non-admin session.create outside project/loaded workspaces must be denied: {outside_reply}"
            );

            // (b) Default project cwd succeeds, but preset & sandbox MUST be capped to anchor ceiling (`read-only`).
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.create",
                    "params": {}
                }),
            )
            .await;
            let created_reply = rpc_read_reply(&mut cr, 3).await;
            assert!(created_reply["error"].is_null(), "project cwd create must succeed: {created_reply}");
            let created_sid = created_reply["result"]["session_id"].as_str().unwrap();
            let created_rt = rpc.manager.session(created_sid).expect("created session runtime");
            let created_perm = created_rt.host.service_table("permissions").expect("permissions service");
            let (preset, sb_level): (String, Option<String>) = created_rt
                .host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(created_perm)
                .unwrap();
            assert_eq!(preset, "read-only", "remote non-admin session.create must cap preset to host ceiling");
            assert_eq!(
                sb_level.as_deref(),
                Some("read-only"),
                "remote non-admin session.create must cap sandbox level to host ceiling"
            );

            // (c) Already-loaded session cwd is permitted and also capped to that workspace's ceiling.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "session.create",
                    "params": { "cwd": loaded_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let loaded_create_reply = rpc_read_reply(&mut cr, 4).await;
            assert!(
                loaded_create_reply["error"].is_null(),
                "creating in an already-loaded session cwd must succeed: {loaded_create_reply}"
            );
            let loaded_created_sid = loaded_create_reply["result"]["session_id"].as_str().unwrap();
            let loaded_created_rt = rpc.manager.session(loaded_created_sid).expect("loaded-dir created runtime");
            let loaded_created_perm = loaded_created_rt.host.service_table("permissions").expect("permissions service");
            let (l_preset, l_sb): (String, Option<String>) = loaded_created_rt
                .host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(loaded_created_perm)
                .unwrap();
            assert_eq!(l_preset, "read-only", "loaded-dir create must respect ceiling");
            assert_eq!(l_sb.as_deref(), Some("read-only"), "loaded-dir create sandbox must respect ceiling");

            // (d) Local admin owner can still create sessions in arbitrary outside cwd.
            let (a_client, a_server) = tokio::io::duplex(131072);
            let (asr, asw) = tokio::io::split(a_server);
            let admin_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                asr,
                asw,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (acr, mut acw) = tokio::io::split(a_client);
            let mut acr = BufReader::new(acr);
            rpc_send(
                &mut acw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut acr, 1).await;
            rpc_send(
                &mut acw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "cwd": outside_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let admin_outside_reply = rpc_read_reply(&mut acr, 2).await;
            assert!(
                admin_outside_reply["error"].is_null(),
                "local admin owner must still be able to create in outside_dir: {admin_outside_reply}"
            );

            drop(acw);
            admin_task.abort();
            let _ = admin_task.await;
            drop(cw);
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;

            // 2. Single-session mode: remote non-admin `session.create` must not escalate above lowered live preset.
            let single_opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "auto" } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let single_host = Host::new(single_opts).await.unwrap();
            let s_perm = single_host.service_table("permissions").expect("permissions service");
            single_host
                .lua()
                .load("local perm = ...; assert(perm:set('read-only'))")
                .call_async::<()>(s_perm)
                .await
                .unwrap();
            let single_rpc = RpcRuntime::from_host(&single_host).await;

            let (s_client, s_server) = tokio::io::duplex(131072);
            let (ssr, ssw) = tokio::io::split(s_server);
            let single_task = tokio::task::spawn_local(serve_inner(
                single_rpc.clone(),
                ssr,
                ssw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (scr, mut scw) = tokio::io::split(s_client);
            let mut scr = BufReader::new(scr);
            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut scr, 1).await;

            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.create",
                    "params": { "cwd": outside_dir.path().to_string_lossy() }
                }),
            )
            .await;
            let s_outside = rpc_read_reply(&mut scr, 2).await;
            assert_eq!(s_outside["error"]["data"]["code"], json!("denied"), "{s_outside}");

            rpc_send(
                &mut scw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.create",
                    "params": {}
                }),
            )
            .await;
            let s_new = rpc_read_reply(&mut scr, 3).await;
            assert!(s_new["error"].is_null(), "single-session create in project cwd must succeed: {s_new}");
            let s_perm_after = single_host.service_table("permissions").expect("permissions service");
            let (s_preset, s_sb): (String, Option<String>) = single_host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(s_perm_after)
                .unwrap();
            assert_eq!(s_preset, "read-only", "single-session remote non-admin create must stay capped at read-only");
            assert_eq!(s_sb.as_deref(), Some("read-only"), "single-session sandbox must stay capped at read-only");

            drop(scw);
            single_task.abort();
            let _ = single_task.await;
            single_host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn remote_non_admin_single_session_close_preserves_permission_ceiling() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let single_opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "auto" } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let single_host = Host::new(single_opts).await.unwrap();
            let active_sid = single_host
                .kernel
                .event_log()
                .session_identity()
                .0
                .expect("active session id");
            let s_perm = single_host.service_table("permissions").expect("permissions service");
            single_host
                .lua()
                .load("local perm = ...; assert(perm:set('read-only'))")
                .call_async::<()>(s_perm)
                .await
                .unwrap();
            let single_rpc = RpcRuntime::from_host(&single_host).await;

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_inner(
                single_rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.close",
                    "params": { "session_id": active_sid }
                }),
            )
            .await;
            let close_reply = rpc_read_reply(&mut cr, 2).await;
            assert!(close_reply["error"].is_null(), "single-session close must succeed: {close_reply}");

            let new_sid = single_host.kernel.event_log().session_identity().0.expect("replacement session id");
            assert_ne!(new_sid, active_sid, "closing active session in single-host mode starts a fresh session");

            let perm_after = single_host.service_table("permissions").expect("permissions service");
            let (preset, sb_level): (String, Option<String>) = single_host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(perm_after)
                .unwrap();
            assert_eq!(
                preset, "read-only",
                "single-session remote non-admin session.close must keep replacement session capped at read-only"
            );
            assert_eq!(
                sb_level.as_deref(),
                Some("read-only"),
                "single-session remote non-admin session.close must keep replacement sandbox capped at read-only"
            );

            drop(cw);
            task.abort();
            let _ = task.await;
            single_host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn remote_non_admin_session_create_omitted_cwd_matches_explicit_project_cwd_ceiling() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let ws_a = tempfile::tempdir().unwrap();
            let ws_b = tempfile::tempdir().unwrap();

            let base_opts = mare_host::HostOptions {
                cwd: ws_a.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "preset": "auto" } },
                    "webserver": { "remote_admin": false }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            // Project A (server default cwd) is narrowed to `read-only`.
            let rt_a = manager.create_session(base_opts.clone()).await.unwrap();
            let perm_a = rt_a.host.service_table("permissions").expect("permissions service");
            rt_a.host
                .lua()
                .load("local perm = ...; assert(perm:set('read-only'))")
                .call_async::<()>(perm_a)
                .await
                .unwrap();

            // Project B stays at `auto` and is the current anchor host.
            let mut opts_b = base_opts.clone();
            opts_b.cwd = ws_b.path().to_owned();
            let rt_b = manager.create_session(opts_b).await.unwrap();

            let rpc = RpcRuntime::new(manager, base_opts, rt_b.host.clone(), Some(rt_b.session_id.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                sr,
                sw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut cr, 1).await;

            // (1) Omitted `cwd` creates in default project A and must inherit A's `read-only` ceiling
            //     even though the current anchor is B (`auto`).
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 2, "method": "session.create", "params": {} }),
            )
            .await;
            let omitted_reply = rpc_read_reply(&mut cr, 2).await;
            assert!(omitted_reply["error"].is_null(), "omitted cwd create must succeed: {omitted_reply}");
            let omitted_sid = omitted_reply["result"]["session_id"].as_str().unwrap();
            let omitted_rt = rpc.manager.session(omitted_sid).unwrap();
            assert_eq!(
                mare_host::session_orchestrator::canonical_dir(&omitted_rt.host.cwd),
                mare_host::session_orchestrator::canonical_dir(ws_a.path())
            );
            let omitted_perm = omitted_rt.host.service_table("permissions").unwrap();
            let (omitted_preset, omitted_sb): (String, Option<String>) = omitted_rt
                .host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(omitted_perm)
                .unwrap();
            assert_eq!(omitted_preset, "read-only", "omitted cwd must take ceiling from project A, not anchor B");
            assert_eq!(omitted_sb.as_deref(), Some("read-only"));

            // Reset anchor back to B (`auto`) and verify explicit `cwd = A` matches omitted `cwd`.
            *rpc.anchor_session_id.borrow_mut() = Some(rt_b.session_id.clone());
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.create",
                    "params": { "cwd": ws_a.path().to_string_lossy() }
                }),
            )
            .await;
            let explicit_a_reply = rpc_read_reply(&mut cr, 3).await;
            assert!(explicit_a_reply["error"].is_null(), "explicit A create must succeed: {explicit_a_reply}");
            let explicit_a_rt = rpc.manager.session(explicit_a_reply["result"]["session_id"].as_str().unwrap()).unwrap();
            let explicit_a_perm = explicit_a_rt.host.service_table("permissions").unwrap();
            let (explicit_a_preset, explicit_a_sb): (String, Option<String>) = explicit_a_rt
                .host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(explicit_a_perm)
                .unwrap();
            assert_eq!(explicit_a_preset, omitted_preset);
            assert_eq!(explicit_a_sb, omitted_sb);

            // (2) Explicit `cwd = B` still gets B's `auto` ceiling (confirming the two workspaces differ).
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "session.create",
                    "params": { "cwd": ws_b.path().to_string_lossy() }
                }),
            )
            .await;
            let explicit_b_reply = rpc_read_reply(&mut cr, 4).await;
            assert!(explicit_b_reply["error"].is_null(), "explicit B create must succeed: {explicit_b_reply}");
            let explicit_b_rt = rpc.manager.session(explicit_b_reply["result"]["session_id"].as_str().unwrap()).unwrap();
            let explicit_b_perm = explicit_b_rt.host.service_table("permissions").unwrap();
            let (explicit_b_preset, explicit_b_sb): (String, Option<String>) = explicit_b_rt
                .host
                .lua()
                .load("local perm = ...; local sb = mare.proc.sandbox.current(); return perm:preset(), sb and sb.level or nil")
                .call(explicit_b_perm)
                .unwrap();
            assert_eq!(explicit_b_preset, "auto");
            assert_eq!(explicit_b_sb.as_deref(), Some("workspace-write"));

            drop(cw);
            task.abort();
            let _ = task.await;
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn owner_disk_load_roots_runtime_at_header_cwd_and_blocks_other_project_peer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let dir_a = tempfile::tempdir().unwrap();
            let dir_b = tempfile::tempdir().unwrap();

            let sid_b = "bbbb7777-0000-0000-0000-000000000007";
            let path_b = dir_b.path().join("project-b.jsonl");
            std::fs::write(
                &path_b,
                format!(
                    "{}\n{}\n",
                    json!({
                        "type": "session",
                        "version": 3,
                        "session_id": sid_b,
                        "cwd": mare_host::session_orchestrator::canonical_dir(dir_b.path())
                    }),
                    json!({
                        "type": "message",
                        "entry_id": "m1",
                        "parent_entry_id": null,
                        "message": { "role": "user", "content": "secret from project B" }
                    }),
                ),
            )
            .unwrap();

            let base_opts = mare_host::HostOptions {
                cwd: dir_a.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["session.get", "session.load", "session.list"] }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt_a = manager.create_session(base_opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, base_opts, rt_a.host.clone(), Some(rt_a.session_id.clone()), false);

            // Owner loads project B's closed session from disk.
            let (o_client, o_server) = tokio::io::duplex(131072);
            let (osr, osw) = tokio::io::split(o_server);
            let owner_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                osr,
                osw,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (ocr, mut ocw) = tokio::io::split(o_client);
            let mut ocr = BufReader::new(ocr);
            rpc_send(
                &mut ocw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut ocr, 1).await;

            rpc_send(
                &mut ocw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.load",
                    "params": { "path": path_b.to_string_lossy() }
                }),
            )
            .await;
            let load_reply = rpc_read_reply(&mut ocr, 2).await;
            assert!(load_reply["error"].is_null(), "owner disk load of project B must succeed: {load_reply}");
            let loaded_rt = rpc.manager.session(sid_b).expect("loaded runtime for project B");
            assert_eq!(
                mare_host::session_orchestrator::canonical_dir(&loaded_rt.host.cwd),
                mare_host::session_orchestrator::canonical_dir(dir_b.path()),
                "disk-loaded runtime must be rooted at its validated header cwd (project B), not server default (project A)"
            );

            // Peer scoped to project A must NOT be able to query or load the reopened project B session.
            let (p_client, p_server) = tokio::io::duplex(131072);
            let (psr, psw) = tokio::io::split(p_server);
            let peer_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                psr,
                psw,
                Principal::Peer,
                false,
                None,
                std::future::pending(),
            ));
            let (pcr, mut pcw) = tokio::io::split(p_client);
            let mut pcr = BufReader::new(pcr);
            rpc_send(
                &mut pcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            let _ = rpc_read_reply(&mut pcr, 1).await;

            rpc_send(
                &mut pcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.get",
                    "params": { "session_id": sid_b }
                }),
            )
            .await;
            let peer_get = rpc_read_reply(&mut pcr, 2).await;
            assert!(
                peer_get["error"].is_object(),
                "peer of project A must be denied session.get on reopened project B session: {peer_get}"
            );

            rpc_send(
                &mut pcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.load",
                    "params": { "session_id": sid_b }
                }),
            )
            .await;
            let peer_load = rpc_read_reply(&mut pcr, 3).await;
            assert!(
                peer_load["error"].is_object(),
                "peer of project A must be denied session.load on reopened project B session: {peer_load}"
            );

            drop(pcw);
            peer_task.abort();
            let _ = peer_task.await;
            drop(ocw);
            owner_task.abort();
            let _ = owner_task.await;
            let _ = rpc.manager.shutdown_all().await;
            let _ = std::fs::remove_file(mare_store::lock_path(&path_b));
        })
        .await;
}

/// Multi-session `session.load` and `session.fork` must reject a persisted session whose header
/// `cwd` directory no longer exists instead of silently building its Host in the server default dir.
#[tokio::test]
async fn multi_session_load_and_fork_reject_deleted_header_cwd() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir_a = tempfile::tempdir().unwrap();
            let dir_b = tempfile::tempdir().unwrap();
            let dir_b_canon = mare_host::session_orchestrator::canonical_dir(dir_b.path());

            let sid_b = "bbbb8888-0000-0000-0000-000000000008";
            let path_b = dir_a.path().join("deleted-project-b.jsonl");
            std::fs::write(
                &path_b,
                format!(
                    "{}\n{}\n",
                    json!({
                        "type": "session",
                        "version": 3,
                        "session_id": sid_b,
                        "cwd": dir_b_canon
                    }),
                    json!({
                        "type": "message",
                        "entry_id": "m1",
                        "parent_entry_id": null,
                        "message": { "role": "user", "content": "secret from deleted project B" }
                    }),
                ),
            )
            .unwrap();
            drop(dir_b);
            assert!(!dir_b_canon.exists(), "project B directory must be deleted");

            let base_opts = mare_host::HostOptions {
                cwd: dir_a.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "shutdown_grace_ms": 50,
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt_a = manager.create_session(base_opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, base_opts, rt_a.host.clone(), Some(rt_a.session_id.clone()), false);

            let (o_client, o_server) = tokio::io::duplex(131072);
            let (osr, osw) = tokio::io::split(o_server);
            let owner_task = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                osr,
                osw,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (ocr, mut ocw) = tokio::io::split(o_client);
            let mut ocr = BufReader::new(ocr);
            rpc_send(
                &mut ocw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocol_version": mare_protocol::PROTOCOL_VERSION }
                }),
            )
            .await;
            assert!(rpc_read_reply(&mut ocr, 1).await.get("error").is_none());

            rpc_send(
                &mut ocw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session.load",
                    "params": { "path": path_b.to_string_lossy() }
                }),
            )
            .await;
            let load_rep = rpc_read_reply(&mut ocr, 2).await;
            assert_eq!(
                load_rep["error"]["data"]["code"].as_str(),
                Some("invalid_arguments"),
                "load of session with deleted dir must fail with invalid_arguments: {load_rep}"
            );
            assert!(rpc.manager.session(sid_b).is_err(), "failed load must not spawn runtime in project A");

            rpc_send(
                &mut ocw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session.fork",
                    "params": { "session_id": path_b.to_string_lossy() }
                }),
            )
            .await;
            let fork_rep = rpc_read_reply(&mut ocr, 3).await;
            assert_eq!(
                fork_rep["error"]["data"]["code"].as_str(),
                Some("invalid_arguments"),
                "fork of session with deleted dir must fail with invalid_arguments: {fork_rep}"
            );
            assert_eq!(rpc.manager.active_ids().len(), 1, "failed fork must not leave an extra runtime open");

            drop(ocw);
            owner_task.abort();
            let _ = owner_task.await;
            let _ = rpc.manager.shutdown_all().await;
            let _ = std::fs::remove_file(mare_store::lock_path(&path_b));
        })
        .await;
}
