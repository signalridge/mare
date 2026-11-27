use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;
use mare_binding::fs::{MAX_WATCHES_PER_CONN, active_watch_count};
use mare_binding::proc::{MAX_PROCS_PER_OWNER, active_proc_count};
use tokio::io::AsyncReadExt;

async fn call_rpc(
    cw: &mut (impl AsyncWrite + Unpin),
    cr: &mut (impl AsyncBufReadExt + Unpin),
    id: u64,
    method: &str,
    params: J,
) -> J {
    rpc_send(cw, json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
    loop {
        let msg = rpc_read(cr).await;
        if msg.get("id").and_then(J::as_u64) == Some(id) {
            return msg;
        }
    }
}

#[tokio::test]
async fn proc_service_spawn_enforces_per_owner_limit_and_cleans_up_on_exit_and_disconnect() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
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

            host.lua().globals().set("RPC", host.kernel.service("rpc").unwrap()).unwrap();
            host.lua().globals().set("MAX_PROCS", MAX_PROCS_PER_OWNER).unwrap();

            // Spawn up to MAX_PROCS_PER_OWNER processes that block on stdin; 33rd fails with busy;
            // killing one frees its slot so replacement succeeds.
            let (): () = host
                .lua()
                .load(
                    r#"
                    RPC:connect("limit-owner")
                    local P = assert(RPC:context("limit-owner"):get("proc"))
                    local p0_exited = false
                    for i = 0, MAX_PROCS - 1 do
                        local r = P:spawn({
                            process_id = "p-" .. i,
                            command = { "cat" },
                            on_exit = function(ev)
                                if ev.process_id == "p-0" then p0_exited = true end
                            end,
                        })
                        assert(r.process_id == ("p-" .. i))
                    end
                    local ok, err = pcall(function()
                        P:spawn({ process_id = "p-over", command = { "cat" } })
                    end)
                    assert(not ok and tostring(err):find("proc.process_limit_reached", 1, true), "expected proc.process_limit_reached error, got: " .. tostring(err))
                    P:kill({ process_id = "p-0" })
                    for _ = 1, 200 do
                        if p0_exited then break end
                        mare.task.sleep(10)
                    end
                    assert(p0_exited, "expected p-0 on_exit callback")
                    local rep = P:spawn({ process_id = "p-replacement", command = { "cat" } })
                    assert(rep.process_id == "p-replacement")
                    "#,
                )
                .call_async(())
                .await
                .unwrap();
            assert_eq!(active_proc_count(), MAX_PROCS_PER_OWNER);

            // Disconnect must kill and clean up all remaining processes.
            super::fs_proc_tests::rpc_disconnect(&host, "limit-owner").await;
            assert_eq!(active_proc_count(), 0);
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_fs_watch_enforces_per_conn_limit_and_cleans_up_on_unwatch_and_disconnect() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
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
            let server_task = tokio::task::spawn_local(serve(host, sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let init =
                call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                    .await;
            assert_eq!(init["result"]["protocol_version"], proto::PROTOCOL_VERSION);

            let mut req_id = 2u64;
            let mut watch_ids = Vec::with_capacity(MAX_WATCHES_PER_CONN);
            for _ in 0..MAX_WATCHES_PER_CONN {
                let res = call_rpc(&mut cw, &mut cr, req_id, "fs.watch", json!({"path": "."})).await;
                req_id += 1;
                let id = res["result"]["registration_id"].as_str().expect("registration_id").to_string();
                watch_ids.push(id);
            }
            assert_eq!(active_watch_count(), MAX_WATCHES_PER_CONN);

            // Exceeding the per-connection limit must be rejected with BUSY.
            let over = call_rpc(&mut cw, &mut cr, req_id, "fs.watch", json!({"path": "."})).await;
            req_id += 1;
            assert_eq!(over["error"]["data"]["code"], "busy", "{over}");
            assert_eq!(active_watch_count(), MAX_WATCHES_PER_CONN);

            // Releasing one frees its slot so a new watch succeeds.
            let unwatch =
                call_rpc(&mut cw, &mut cr, req_id, "registrations.release", json!({"registration_id": watch_ids[0]}))
                    .await;
            req_id += 1;
            assert_eq!(unwatch["result"], json!({}), "{unwatch}");
            assert_eq!(active_watch_count(), MAX_WATCHES_PER_CONN - 1);

            let replacement = call_rpc(&mut cw, &mut cr, req_id, "fs.watch", json!({"path": "."})).await;
            assert!(replacement["result"]["registration_id"].as_str().is_some(), "{replacement}");
            assert_eq!(active_watch_count(), MAX_WATCHES_PER_CONN);

            // Disconnect must abort and clean up all remaining watches.
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            assert_eq!(active_watch_count(), 0);
        })
        .await;
}

#[tokio::test]
async fn stalled_non_ws_peer_overflow_disconnects_without_ws_drain_wait() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);

            // Small duplex buffer so the peer's unread responses saturate the stream quickly
            // without reading a single byte back.
            let (client, server) = tokio::io::duplex(4096);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve_peer_manager(rpc.clone(), sr, sw, None));
            let (mut cr, mut cw) = tokio::io::split(client);

            let t0 = std::time::Instant::now();
            let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status.get\",\"params\":{}}\n";
            // Write enough requests to overflow OUT_QUEUE (4096) without consuming responses;
            // server must process them fast enough and close within WRITER_DRAIN_WAIT (500ms),
            // never stalling for WRITER_OVERLOAD_DRAIN_WAIT (8s).
            let write_fut = async {
                for _ in 0..(OUT_QUEUE * 2) {
                    if cw.write_all(request).await.is_err() {
                        break;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(4), write_fut)
                .await
                .expect("stalled peer write loop timed out before server disconnected");

            tokio::time::timeout(Duration::from_secs(3), server_task)
                .await
                .expect("stalled non-WS peer server task must finish within WRITER_DRAIN_WAIT, not 8s WS wait")
                .unwrap();
            assert!(
                t0.elapsed() < WRITER_OVERLOAD_DRAIN_WAIT,
                "stalled non-WS peer took {:?}, exceeding WS overload drain wait",
                t0.elapsed()
            );

            // Drain any buffered prefix and confirm EOF on the client read half.
            let mut buf = [0u8; 4096];
            loop {
                match cr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            drop(cw);
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}
