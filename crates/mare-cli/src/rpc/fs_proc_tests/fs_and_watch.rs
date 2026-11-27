use super::*;

#[tokio::test]
async fn rpc_initialize_advertises_watch_and_process_capabilities() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let host = make_rpc_host(
                ws.path(),
                json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                false,
                true,
            )
            .await;

            let (mut cw, mut cr, server_task, init) = open_rpc_session(&host, true, json!({})).await;
            assert_eq!(init["result"]["protocol_version"], proto::PROTOCOL_VERSION);
            let methods: Vec<&str> =
                init["result"]["capabilities"]["methods"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            for m in ["fs.read", "fs.write", "fs.list", "fs.stat", "fs.watch"] {
                assert!(methods.contains(&m), "missing {m} in {methods:?}");
            }
            for removed in [
                "fs.unwatch",
                "services.call",
                "services.unregister",
                "providers.unregister",
                "tools.unregister",
                "web.unregister",
                "peer.idle_subscribe",
                "peer.idle_unsubscribe",
            ] {
                assert!(!methods.contains(&removed), "{removed} must not be advertised in RPC capabilities");
                let rej = call_rpc(&mut cw, &mut cr, 91, removed, json!({})).await;
                assert_eq!(rej["error"]["code"], codes::METHOD_NOT_FOUND, "{removed}: {rej}");
            }
            // proc.* and legacy process.* must never be exposed over remote RPC (expose = "lua" only).
            for denied in ["proc.spawn", "proc.write", "proc.resize", "proc.kill", "process.spawn"] {
                assert!(!methods.contains(&denied), "{denied} must not be advertised in RPC capabilities");
                let rej = call_rpc(&mut cw, &mut cr, 90, denied, json!({})).await;
                assert_eq!(rej["error"]["code"], codes::METHOD_NOT_FOUND, "{denied}: {rej}");
            }

            close_rpc_session(cw, cr, server_task).await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_write_preserves_snapshot_journal() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let journal = tempfile::tempdir().unwrap();
            let target = ws.path().join("tracked.txt");
            let tool_target = ws.path().join("tool-tracked.txt");
            std::fs::write(&target, "before-v1").unwrap();
            std::fs::write(&tool_target, "tool-before").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            set_lua_sandbox(
                &host,
                r#"{ level = "workspace-write", exclude_slash_tmp = true, exclude_tmpdir_env_var = true }"#,
            );

            let j_path = journal.path().to_string_lossy().into_owned();
            host.lua()
                .load(
                    r#"local dir = ...
                    mare.snapshot.configure({ dir = dir })
                    assert(mare.snapshot.begin_turn("turn-rpc-1"))"#,
                )
                .call::<()>(j_path)
                .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let _ = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                .await;
            let wr = call_rpc(
                &mut cw,
                &mut cr,
                2,
                "fs.write",
                json!({"path": "tracked.txt", "data_base64": STANDARD.encode(b"after-v2"), "atomic": true}),
            )
            .await;
            assert!(wr.get("error").is_none(), "{wr}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "after-v2");

            let written: bool = host
                .lua()
                .load(
                    r#"return mare.exec.run({ tool = "snapshot-probe", tool_call_id = "tool-call-1" }, function()
                        return mare.fs.write("tool-tracked.txt", "tool-after", { capture_snapshot = true })
                    end)"#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(written);
            let index = std::fs::read_to_string(journal.path().join("index.jsonl")).unwrap();
            let records: Vec<J> = index.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            for (path, call) in [(&target, "rpc.fs.write"), (&tool_target, "tool-call-1")] {
                let entry = records
                    .iter()
                    .find(|entry| entry["path"] == path.to_string_lossy().as_ref() && entry.get("before").is_some())
                    .unwrap_or_else(|| panic!("missing snapshot for {}: {records:?}", path.display()));
                assert_eq!(entry["call"], call, "snapshot attribution: {entry}");
            }

            let restored: Vec<String> = host
                .lua()
                .load(
                    r#"local res = mare.snapshot.restore("turn-rpc-1")
                    mare.snapshot.configure(nil)
                    return res.restored"#,
                )
                .call_async(())
                .await
                .unwrap();
            assert!(!restored.is_empty(), "snapshot restore did not restore tracked file");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "before-v1");
            assert_eq!(std::fs::read_to_string(&tool_target).unwrap(), "tool-before");

            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_watch_emits_filtered_changes_and_unwatch_stops_them() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("visible.txt"), "init").unwrap();
            std::fs::write(ws.path().join("secret.txt"), "hidden").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            set_lua_sandbox(
                &host,
                r#"{ level = "workspace-write", deny_read = { "secret.txt" }, accept_partial = { "fs_deny_read" } }"#,
            );

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let _ = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                .await;
            let watch = call_rpc(&mut cw, &mut cr, 2, "fs.watch", json!({"path": "."})).await;
            let watch_id = watch["result"]["registration_id"].as_str().unwrap().to_string();

            // Write a denied file on disk + a visible file via RPC; only visible.txt may be reported.
            std::fs::write(ws.path().join("secret.txt"), "changed-secret").unwrap();
            let mut buf = Vec::new();
            let wr = call_rpc_with_buf(
                &mut cw,
                &mut cr,
                3,
                "fs.write",
                json!({"path": "visible.txt", "data_base64": STANDARD.encode(b"updated")}),
                &mut buf,
            )
            .await;
            assert!(wr.get("error").is_none(), "{wr}");

            let changed = next_notification_with_buf(&mut cr, "fs/changed", &mut buf).await;
            assert_eq!(changed["watch_id"], watch_id);
            let paths: Vec<&str> = changed["changed_paths"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(paths.iter().any(|p| p.ends_with("visible.txt")), "{paths:?}");
            assert!(!paths.iter().any(|p| p.ends_with("secret.txt")), "deny_read leaked in fs/changed: {paths:?}");

            let unwatch =
                call_rpc(&mut cw, &mut cr, 4, "registrations.release", json!({"registration_id": watch_id})).await;
            assert_eq!(unwatch["result"], json!({}), "{unwatch}");

            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_watch_tracks_git_and_dynamic_policy_changes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(ws.path().join(".git")).unwrap();
            std::fs::write(ws.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(ws.path().join(".git/secret"), "token").unwrap();
            std::fs::write(ws.path().join("guarded.txt"), "initial").unwrap();
            std::fs::write(ws.path().join("trigger.txt"), "initial").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            set_lua_sandbox(&host, r#"{ level = "workspace-write", deny_read = { ".git/secret", "guarded.txt" }, accept_partial = { "fs_deny_read" } }"#);

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let _ = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let watch = call_rpc(&mut cw, &mut cr, 2, "fs.watch", json!({"path": "."})).await;
            let watch_id = watch["result"]["registration_id"].as_str().unwrap().to_string();

            // 1. Readable .git/HEAD changes are reported; denied .git/secret is filtered.
            std::fs::write(ws.path().join(".git/secret"), "changed-token").unwrap();
            std::fs::write(ws.path().join(".git/HEAD"), "ref: refs/heads/dev\n").unwrap();
            let changed = next_notification(&mut cr, "fs/changed").await;
            assert_eq!(changed["watch_id"], watch_id);
            let paths: Vec<&str> = changed["changed_paths"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(paths.iter().any(|p| std::path::Path::new(p).ends_with(".git/HEAD")), "{paths:?}");
            assert!(!paths.iter().any(|p| std::path::Path::new(p).ends_with(".git/secret")), "{paths:?}");

            // 2. Relaxing policy reveals guarded.txt on next poll.
            set_lua_sandbox(&host, r#"{ level = "workspace-write", deny_read = { ".git/secret" }, accept_partial = { "fs_deny_read" } }"#);
            let revealed = next_notification(&mut cr, "fs/changed").await;
            let paths: Vec<&str> = revealed["changed_paths"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(paths.iter().any(|p| p.ends_with("guarded.txt")), "{paths:?}");

            // 3. Tightening policy suppresses subsequent edits to trigger.txt.
            set_lua_sandbox(&host, r#"{ level = "workspace-write", deny_read = { ".git/secret", "trigger.txt" }, accept_partial = { "fs_deny_read" } }"#);
            let removed = next_notification(&mut cr, "fs/changed").await;
            let paths: Vec<&str> = removed["changed_paths"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(paths.iter().any(|p| p.ends_with("trigger.txt")), "{paths:?}");

            std::fs::write(ws.path().join("trigger.txt"), "edited-while-denied").unwrap();
            let mut buf = Vec::new();
            let wr = call_rpc_with_buf(
                &mut cw,
                &mut cr,
                3,
                "fs.write",
                json!({"path": "guarded.txt", "data_base64": STANDARD.encode(b"after-deny")}),
                &mut buf,
            )
            .await;
            assert!(wr.get("error").is_none(), "{wr}");
            let after_deny = next_notification_with_buf(&mut cr, "fs/changed", &mut buf).await;
            let paths: Vec<&str> =
                after_deny["changed_paths"].as_array().unwrap().iter().filter_map(J::as_str).collect();
            assert!(paths.iter().any(|p| p.ends_with("guarded.txt")), "{paths:?}");
            assert!(!paths.iter().any(|p| p.ends_with("trigger.txt")), "denied edit leaked: {paths:?}");

            let _ = call_rpc(&mut cw, &mut cr, 4, "registrations.release", json!({"registration_id": watch_id})).await;
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_watch_signals_truncation_over_max_entries() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let big = ws.path().join("big");
            let growing = ws.path().join("growing");
            std::fs::create_dir_all(&big).unwrap();
            std::fs::create_dir_all(&growing).unwrap();
            for i in 0..8200 {
                std::fs::write(big.join(format!("f_{i:04}.txt")), "").unwrap();
            }
            for i in 0..8190 {
                std::fs::write(growing.join(format!("g_{i:04}.txt")), "").unwrap();
            }

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let _ = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                .await;

            // 1. Initial snapshot >8192 entries reports truncated: true via fs/changed.
            let watch_big = call_rpc(&mut cw, &mut cr, 2, "fs.watch", json!({"path": "big"})).await;
            let big_id = watch_big["result"]["registration_id"].as_str().unwrap().to_string();
            loop {
                let note = next_notification(&mut cr, "fs/changed").await;
                if note["watch_id"] == big_id && note.get("truncated").and_then(J::as_bool) == Some(true) {
                    break;
                }
            }
            let _ = call_rpc(&mut cw, &mut cr, 3, "registrations.release", json!({"registration_id": big_id})).await;

            // 2. Growing past 8192 entries during watch signals truncated: true on fs/changed.
            let watch_grow = call_rpc(&mut cw, &mut cr, 4, "fs.watch", json!({"path": "growing"})).await;
            let grow_id = watch_grow["result"]["registration_id"].as_str().unwrap().to_string();
            for i in 8190..8200 {
                std::fs::write(growing.join(format!("g_{i:04}.txt")), "").unwrap();
            }
            loop {
                let note = next_notification(&mut cr, "fs/changed").await;
                if note["watch_id"] == grow_id && note.get("truncated").and_then(J::as_bool) == Some(true) {
                    break;
                }
            }
            let _ = call_rpc(&mut cw, &mut cr, 5, "registrations.release", json!({"registration_id": grow_id})).await;

            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_watch_cleans_up_on_disconnect() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("a.txt"), "1").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let _ = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                .await;
            let watch = call_rpc(&mut cw, &mut cr, 2, "fs.watch", json!({"path": "."})).await;
            assert!(watch.get("error").is_none(), "{watch}");
            assert_eq!(mare_binding::fs::active_watch_count(), 1);

            std::fs::write(ws.path().join("a.txt"), "2").unwrap();
            let changed = next_notification(&mut cr, "fs/changed").await;
            assert_eq!(changed["watch_id"], watch["result"]["registration_id"]);

            // Disconnect without calling registrations.release; watch task must be aborted and reaped.
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            assert_eq!(mare_binding::fs::active_watch_count(), 0, "watch task must be gone after disconnect");
            host.shutdown().await;
        })
        .await;
}

#[cfg(unix)]
#[tokio::test]
async fn rpc_fs_watch_reports_unreadable_dir_and_does_not_claim_untruncated() {
    use std::os::unix::fs::PermissionsExt as _;

    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let unreadable = ws.path().join("unreadable");
            std::fs::create_dir_all(&unreadable).unwrap();
            std::fs::write(unreadable.join("hidden.txt"), "secret").unwrap();
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o111)).unwrap();

            let parent = ws.path().join("parent");
            let sub = parent.join("sub");
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("ok.txt"), "ok").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let _ = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                .await;

            // 1. Direct fs.watch on an unreadable 0111 directory must fail with an error.
            let direct = call_rpc(&mut cw, &mut cr, 2, "fs.watch", json!({"path": "unreadable"})).await;
            assert!(direct.get("error").is_some(), "unreadable root must not succeed: {direct}");

            // 2. Watching "." containing a 0111 subdirectory must emit fs/changed with truncated: true.
            let root_watch = call_rpc(&mut cw, &mut cr, 3, "fs.watch", json!({"path": "."})).await;
            assert!(root_watch.get("error").is_none(), "{root_watch}");
            let root_id = root_watch["result"]["registration_id"].as_str().unwrap().to_string();
            loop {
                let note = next_notification(&mut cr, "fs/changed").await;
                if note["watch_id"] == root_id && note.get("truncated").and_then(J::as_bool) == Some(true) {
                    break;
                }
            }
            let _ = call_rpc(&mut cw, &mut cr, 4, "registrations.release", json!({"registration_id": root_id})).await;

            // 3. A subdirectory becoming 0111 during watch emits fs/changed with truncated: true.
            let parent_watch = call_rpc(&mut cw, &mut cr, 5, "fs.watch", json!({"path": "parent"})).await;
            assert!(parent_watch["result"].get("truncated").is_none(), "{parent_watch}");
            let parent_id = parent_watch["result"]["registration_id"].as_str().unwrap().to_string();
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o111)).unwrap();
            loop {
                let note = next_notification(&mut cr, "fs/changed").await;
                if note["watch_id"] == parent_id && note.get("truncated").and_then(J::as_bool) == Some(true) {
                    break;
                }
            }

            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            assert_eq!(mare_binding::fs::active_watch_count(), 0);
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_watch_on_second_session_works_after_prior_connection_shifts_fiber_uid() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws1 = tempfile::tempdir().unwrap();
            let ws2 = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: ws1.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt1.host.clone(), Some(rt1.session_id.clone()), false);

            // 1. Connection C1 connects to S1 (consuming S1's initial fiber uid) and disconnects.
            let (c1, s1_srv) = tokio::io::duplex(65536);
            let (sr1, sw1) = tokio::io::split(s1_srv);
            let t1 = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                sr1,
                sw1,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr1, mut cw1) = tokio::io::split(c1);
            let mut cr1 = BufReader::new(cr1);
            let init1 =
                call_rpc(&mut cw1, &mut cr1, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                    .await;
            assert!(init1.get("error").is_none(), "{init1}");
            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), t1).await.unwrap().unwrap();

            // 2. Connection C2 connects to S1 (getting shifted fiber uid U+1 on S1), then creates
            //    a fresh session S2 (new Lua VM where C2 gets fiber uid U) and watches S2's cwd.
            let (c2, s2_srv) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(s2_srv);
            let t2 = tokio::task::spawn_local(serve_inner(
                rpc.clone(),
                sr2,
                sw2,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            let init2 =
                call_rpc(&mut cw2, &mut cr2, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                    .await;
            assert!(init2.get("error").is_none(), "{init2}");

            let created =
                call_rpc(&mut cw2, &mut cr2, 2, "session.create", json!({"cwd": ws2.path().to_string_lossy()})).await;
            assert!(created.get("error").is_none(), "{created}");
            let _s2 = created["result"]["session_id"].as_str().unwrap().to_string();

            let watch = call_rpc(&mut cw2, &mut cr2, 3, "fs.watch", json!({"path": "."})).await;
            assert!(watch.get("error").is_none(), "{watch}");
            let watch_id = watch["result"]["registration_id"].as_str().unwrap().to_string();

            std::fs::write(ws2.path().join("hello.txt"), "changed").unwrap();
            let note = tokio::time::timeout(Duration::from_secs(3), next_notification(&mut cr2, "fs/changed"))
                .await
                .expect("fs/changed on second session must not be dropped due to stale Conn.fiber_uid");
            assert_eq!(note["watch_id"], watch_id, "{note}");

            drop((cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), t2).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}
