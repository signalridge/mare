use super::*;
use mare_host::session_state::{SessionPromptReceipts, prompt_content_fingerprint};

fn fp(s: &str) -> [u8; 32] {
    prompt_content_fingerprint(s)
}

#[test]
fn prompt_request_cache_session_removal_cleans_deleted_session_only() {
    let receipts = SessionPromptReceipts::new();
    receipts.insert("lz225-deleted".into(), "r1".into(), fp("c1"), json!({"turn": 1}));
    receipts.insert("lz225-active".into(), "r1".into(), fp("c2"), json!({"turn": 2}));

    receipts.remove_session("lz225-deleted");

    assert!(receipts.get("lz225-deleted", "r1").is_none(), "deleted session entry must be cleared");
    assert_eq!(receipts.get("lz225-active", "r1").unwrap().1, json!({"turn": 2}));
}

#[test]
fn rpc_session_resume_index_work_does_not_block_options_get_on_real_rpc_path() {
    if std::env::var_os("MARE_ISOLATED_TEST").is_none() {
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rpc::dispatch::tests::rpc_session_resume_index_work_does_not_block_options_get_on_real_rpc_path",
            ])
            .env("MARE_HOME", home.path())
            .env("MARE_ISOLATED_TEST", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "isolated child test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        return;
    }

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mare_home = std::path::PathBuf::from(std::env::var_os("MARE_HOME").expect("MARE_HOME must be set"));
    let isolated_sessions = mare_shared::paths::sessions_dir();
    assert!(isolated_sessions.starts_with(&mare_home), "{}", isolated_sessions.display());

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    let ws = mare_home.join("ws");
    let proj_dir = isolated_sessions.join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&proj_dir).unwrap();
    let iso_path = proj_dir.join("2026-01-01T00-00-00_iso.jsonl");
    let iso_id = "isolated-sess-0001";
    std::fs::write(
        &iso_path,
        format!(
            "{}\n{}\n",
            json!({"type": "session", "version": mare_store::CURRENT_VERSION, "session_id": iso_id, "cwd": &ws}),
            json!({"type": "name", "name": "isolated-target"}),
        ),
    )
    .unwrap();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));

    local.block_on(&rt, async move {
        let host = Host::new(mare_host::HostOptions {
            cwd: ws.clone(),
            mode: mare_plugin::Mode::Print,
            config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
            load_init: false,
            project_trusted: false,
            args: json!({}),
            no_user_plugins: true,
        })
        .await
        .unwrap();

        {
            let lua = host.lua();
            let store: mlua::Table = lua.globals().get::<mlua::Table>("mare").unwrap().get("store").unwrap();
            let orig_read: mlua::Function = store.get("index_read").unwrap();
            let gate_entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let wrapper = lua
                .create_async_function(move |lua, args: mlua::MultiValue| {
                    let orig = orig_read.clone();
                    let entered_tx = entered_tx.clone();
                    let release_rx = release_rx.clone();
                    let first = !gate_entered.swap(true, std::sync::atomic::Ordering::SeqCst);
                    async move {
                        let _ = &lua;
                        if first {
                            tokio::task::spawn_blocking(move || {
                                let _ = entered_tx.send(());
                                let _ = release_rx.lock().unwrap().recv();
                            })
                            .await
                            .unwrap();
                        }
                        orig.call_async::<mlua::MultiValue>(args).await
                    }
                })
                .unwrap();
            store.set("index_read", wrapper).unwrap();
        }

        let (crate::rpc::rpc_harness::TestRpc { server: server_task, mut cr, mut cw, .. }, shutdown_tx) =
            crate::rpc::rpc_harness::TestRpc::connect_with_shutdown(host.clone(), true);

        // Drive the client from a separate OS thread with bounded timeouts so a blocked
        // single-threaded server LocalSet fails promptly instead of hanging.
        let (client_done_tx, client_done_rx) = tokio::sync::oneshot::channel::<()>();
        let client_thread = std::thread::spawn(move || {
            let crt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            crt.block_on(async move {
                let mut line = String::new();

                cw.write_all(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{}}}"#)
                    .await
                    .unwrap();
                cw.write_all(b"\n").await.unwrap();
                cw.flush().await.unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), cr.read_line(&mut line))
                    .await
                    .expect("initialize timeout")
                    .unwrap();
                let init_resp: J = serde_json::from_str(&line).unwrap();
                let init_sid = init_resp["result"]["session"]["session_id"].as_str().unwrap().to_owned();

                // Start session.load; it must enter spawn_blocking(load_index_for_query)
                // against the isolated session root.
                cw.write_all(
                    br#"{"jsonrpc":"2.0","id":2,"method":"session.load","params":{"session_id":"isolated-target"}}"#,
                )
                .await
                .unwrap();
                cw.write_all(b"\n").await.unwrap();
                cw.flush().await.unwrap();

                entered_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("session.load did not enter index work");

                // While session.load's index work is held, options.get must still respond.
                let opt_req = json!({"jsonrpc":"2.0","id":3,"method":"options.get","params":{"session_id":init_sid}});
                cw.write_all(format!("{opt_req}\n").as_bytes()).await.unwrap();
                cw.flush().await.unwrap();

                line.clear();
                tokio::time::timeout(std::time::Duration::from_secs(5), cr.read_line(&mut line))
                    .await
                    .expect("options.get blocked while session.load index work was held")
                    .unwrap();
                let opt_resp: J = serde_json::from_str(&line).unwrap();
                assert_eq!(opt_resp["id"], 3, "{opt_resp}");
                assert!(opt_resp["result"].is_object(), "{opt_resp}");

                // Release session.load and verify it actually indexed and resolved the
                // isolated session (skipping any session/switch-completed event notifications).
                release_tx.send(()).unwrap();
                let res_resp: J = loop {
                    line.clear();
                    tokio::time::timeout(std::time::Duration::from_secs(5), cr.read_line(&mut line))
                        .await
                        .expect("session.load did not finish after release")
                        .unwrap();
                    let v: J = serde_json::from_str(&line).unwrap();
                    if v.get("id").is_some() {
                        break v;
                    }
                };
                assert_eq!(res_resp["id"], 2, "{res_resp}");
                assert_eq!(res_resp["result"]["session_id"], iso_id, "{res_resp}");
            });
            let _ = client_done_tx.send(());
        });

        let _ = client_done_rx.await;
        client_thread.join().unwrap();
        assert!(isolated_sessions.join(mare_store::index::DB_NAME).is_file());
        let _ = shutdown_tx.send(());
        server_task.await.unwrap();
        host.shutdown().await;
    });
}

/// Session-scoped RPC methods must require an explicit `session_id` and return `invalid_arguments`
/// when it is omitted rather than silently falling back to a default session.
#[tokio::test]
async fn session_scoped_methods_require_session_id() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::HOST_TEST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": false } } }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (crate::rpc::rpc_harness::TestRpc { server: server_task, mut cr, mut cw, .. }, shutdown_tx) =
                crate::rpc::rpc_harness::TestRpc::connect_with_shutdown(host.clone(), true);

            async fn send_and_read(
                cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                cr: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
                id: i64,
                method: &str,
                params: J,
            ) -> J {
                let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
                cw.write_all(format!("{req}\n").as_bytes()).await.unwrap();
                cw.flush().await.unwrap();
                loop {
                    let mut line = String::new();
                    tokio::time::timeout(std::time::Duration::from_secs(5), cr.read_line(&mut line))
                        .await
                        .expect("rpc reply timeout")
                        .unwrap();
                    let v: J = serde_json::from_str(&line).unwrap();
                    if v["id"] == id {
                        return v;
                    }
                }
            }

            let init = send_and_read(
                &mut cw,
                &mut cr,
                1,
                "initialize",
                json!({ "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": { "callbacks": true, "ui": true } }),
            )
            .await;
            assert!(init["error"].is_null(), "{init}");
            let active_sid = init["result"]["session"]["session_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .expect("host must have an active default session to prove no fallback occurs")
                .to_owned();
            let ok_opts = send_and_read(&mut cw, &mut cr, 90, "options.get", json!({ "session_id": active_sid })).await;
            assert!(ok_opts["error"].is_null(), "explicit session_id must succeed on active host: {ok_opts}");

            // Protocol and contract methods that must reject missing `session_id` with `invalid_arguments`.
            let cases: &[(&str, J)] = &[
                ("options.get", json!({})),
                ("options.set", json!({ "values": { "model": "faux/echo" } })),
                ("ui.attach", json!({})),
                ("ui.detach", json!({})),
                ("session.rewind", json!({})),
                ("session.rewind", json!({ "before_item_id": "nonexistent" })),
                ("hooks.subscribe", json!({ "event": "status/changed", "callbacks": ["handle"] })),
                ("events.subscribe", json!({})),
            ];

            let mut failures = Vec::new();
            for (idx, (method, params)) in cases.iter().enumerate() {
                let resp = send_and_read(&mut cw, &mut cr, (idx + 2) as i64, method, params.clone()).await;
                let code = resp["error"]["data"]["code"].as_str();
                if code != Some("invalid_arguments") {
                    failures.push(format!("{method}: expected error.data.code == \"invalid_arguments\", got {resp}"));
                }
            }

            let _ = shutdown_tx.send(());
            let _ = server_task.await;
            host.shutdown().await;

            assert!(
                failures.is_empty(),
                "session-scoped methods accepted missing session_id:\n{}",
                failures.join("\n")
            );
        })
        .await;
}

#[tokio::test]
async fn workspace_host_methods_work_from_lua_and_wire_bridge() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = crate::rpc::rpc_harness::HOST_LOCK.lock().await;
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
                no_user_plugins: false,
            })
            .await
            .unwrap();

            // 1. Lua caller invokes `workspace:get()` and `workspace:set_trust(true, false)` via the host service.
            let ws_svc = host.kernel.service("workspace").expect("workspace service must be registered");
            let get_lua: mlua::Function = host
                .lua()
                .load(
                    r#"
                    local ws = ...
                    local before = ws:get()
                    assert(type(before.root) == "string" and #before.root > 0, "root missing")
                    assert(before.trusted == false, "expected initially untrusted")
                    assert(type(before.dirs) == "table", "dirs must be a table")
                    ws:set_trust(true, false)
                    local after = ws:get()
                    assert(after.trusted == true, "expected trusted after set_trust")
                    ws:set_trust(false, false)
                    return after.root
                    "#,
                )
                .into_function()
                .unwrap();
            let lua_root: String = get_lua.call_async(ws_svc).await.expect("Lua workspace host methods failed");

            // 2. Wire RPC caller invokes `workspace.get` and `workspace.set_trust` via the generic Scope::Workspace bridge.
            let mut rpc = crate::rpc::rpc_harness::TestRpc::connect(host.clone());
            let init = rpc.call_id(1, "initialize", json!({ "protocol_version": "3.0" })).await;
            assert!(init.get("error").is_none(), "{init}");
            let got = rpc.call_id(2, "workspace.get", json!({ "cwd": dir.path().to_string_lossy() })).await;
            assert!(got.get("error").is_none(), "{got}");
            assert_eq!(got["result"]["root"], lua_root, "{got}");
            assert_eq!(got["result"]["trusted"], false, "{got}");
            assert!(got["result"]["dirs"].is_array(), "{got}");

            let set = rpc
                .call_id(
                    3,
                    "workspace.set_trust",
                    json!({ "cwd": dir.path().to_string_lossy(), "trusted": true, "save": false }),
                )
                .await;
            assert!(set.get("error").is_none(), "{set}");
            let got2 = rpc.call_id(4, "workspace.get", json!({ "cwd": dir.path().to_string_lossy() })).await;
            assert_eq!(got2["result"]["trusted"], true, "{got2}");

            let server = rpc.disconnect();
            server.await.unwrap();
            host.shutdown().await;
        })
        .await;
}
