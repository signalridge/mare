use super::*;

#[tokio::test]
async fn proc_service_spawn_non_pty_exits_and_frees_slot_when_descendant_inherits_pipes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let marker = ws.path().join("child.pid");
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

            let cmd = format!("sleep 30 & echo $! > '{}'; echo complete", marker.display());
            host.lua().globals().set("REAP_CMD", cmd).unwrap();
            let (out, exit_code, drain_timed_out): (String, Option<i32>, Option<bool>) = proc_eval(
                &host,
                r#"
                RPC:connect("conn-bg")
                local P = assert(RPC:context("conn-bg"):get("proc"))
                local chunks, exited = {}, nil
                P:spawn({
                    process_id = "p-bg",
                    command = { "sh", "-c", REAP_CMD },
                    tty = false,
                    on_output = function(ev) chunks[#chunks + 1] = mare.crypto.base64_decode(ev.data_base64) end,
                    on_exit = function(ev) exited = ev end,
                })
                for _ = 1, 200 do
                    if exited ~= nil then break end
                    mare.task.sleep(20)
                end
                assert(exited ~= nil, "on_exit must arrive promptly even when a descendant keeps pipes open")
                return table.concat(chunks), exited.exit_code, exited.output_drain_timed_out
                "#,
            )
            .await;

            assert_eq!(exit_code, Some(0));
            assert_eq!(drain_timed_out, Some(true));
            assert!(out.contains("complete"), "stdout: {out:?}");
            assert_eq!(mare_binding::proc::active_proc_count(), 0);

            let reused_code: Option<i32> = proc_eval(
                &host,
                r#"
                local P = assert(RPC:context("conn-bg"):get("proc"))
                local exited = nil
                P:spawn({
                    process_id = "p-bg",
                    command = { "sh", "-c", "echo reused" },
                    tty = false,
                    on_exit = function(ev) exited = ev end,
                })
                for _ = 1, 200 do
                    if exited ~= nil then break end
                    mare.task.sleep(15)
                end
                assert(exited ~= nil, "expected p-bg reuse exit")
                return exited.exit_code
                "#,
            )
            .await;
            assert_eq!(reused_code, Some(0));
            assert_eq!(mare_binding::proc::active_proc_count(), 0);

            let _child_pid: u32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
            rpc_disconnect(&host, "conn-bg").await;

            #[cfg(unix)]
            assert_pid_terminated(_child_pid, "lingering child must be terminated on disconnect").await;

            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn proc_service_spawn_non_pty_single_open_pipe_times_out_within_single_deadline() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let marker = ws.path().join("single_pipe.pid");
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

            let cmd = format!("sleep 0.16 & sleep 30 >/dev/null & echo $! > '{}'; echo complete", marker.display());
            host.lua().globals().set("PIPE_CMD", cmd).unwrap();
            let (out, exit_code, drain_timed_out): (String, Option<i32>, Option<bool>) = proc_eval(
                &host,
                r#"
                RPC:connect("conn-pipe")
                local P = assert(RPC:context("conn-pipe"):get("proc"))
                local chunks, exited = {}, nil
                P:spawn({
                    process_id = "p-single-pipe",
                    command = { "sh", "-c", PIPE_CMD },
                    tty = false,
                    on_output = function(ev) chunks[#chunks + 1] = mare.crypto.base64_decode(ev.data_base64) end,
                    on_exit = function(ev) exited = ev end,
                })
                for _ = 1, 250 do
                    if exited ~= nil then break end
                    mare.task.sleep(10)
                end
                assert(exited ~= nil, "on_exit must arrive promptly")
                return table.concat(chunks), exited.exit_code, exited.output_drain_timed_out
                "#,
            )
            .await;
            let exited_at = std::time::SystemTime::now();

            assert_eq!(exit_code, Some(0));
            assert_eq!(drain_timed_out, Some(true));
            assert!(out.contains("complete"), "stdout: {out:?}");
            let shell_exit = std::fs::metadata(&marker).unwrap().modified().unwrap();
            let drained = exited_at.duration_since(shell_exit).unwrap_or_else(|e| {
                panic!("on_exit ({exited_at:?}) is earlier than shell exit marker ({shell_exit:?}): {e}")
            });
            assert!(
                drained < std::time::Duration::from_millis(450),
                "on_exit must share a single ~250ms deadline across stdout/stderr, took {drained:?}"
            );

            let _child_pid: u32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
            rpc_disconnect(&host, "conn-pipe").await;
            assert_eq!(mare_binding::proc::active_proc_count(), 0);

            #[cfg(unix)]
            assert_pid_terminated(_child_pid, "lingering child must be terminated on disconnect").await;

            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn proc_service_disconnect_during_pipe_drain_terminates_lingering_descendant() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let marker = ws.path().join("in_flight_drain.pid");
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

            let cmd = format!("sleep 30 & echo $! > '{}'; echo complete", marker.display());
            host.lua().globals().set("DRAIN_CMD", cmd).unwrap();
            let _: () = proc_eval(
                &host,
                r#"
                RPC:connect("conn-drain-dc")
                local P = assert(RPC:context("conn-drain-dc"):get("proc"))
                local seen = ""
                P:spawn({
                    process_id = "p-drain-dc",
                    command = { "sh", "-c", DRAIN_CMD },
                    tty = false,
                    on_output = function(ev) seen = seen .. mare.crypto.base64_decode(ev.data_base64) end,
                })
                for _ = 1, 200 do
                    if seen:find("complete", 1, true) then break end
                    mare.task.sleep(5)
                end
                assert(seen:find("complete", 1, true), "expected stdout delta from spawned process")
                "#,
            )
            .await;

            let _child_pid: u32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
            rpc_disconnect(&host, "conn-drain-dc").await;
            assert_eq!(mare_binding::proc::active_proc_count(), 0);

            #[cfg(unix)]
            assert_pid_terminated(
                _child_pid,
                "lingering child must be terminated when owner disconnects during pipe drain",
            )
            .await;

            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn proc_service_lingering_tree_reaped_naturally_without_further_calls() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let marker = ws.path().join("natural_exit.pid");
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

            let cmd = format!("sleep 0.8 >/dev/null 2>&1 & echo $! > '{}'; echo complete", marker.display());
            host.lua().globals().set("NAT_CMD", cmd).unwrap();
            let exit_code: Option<i32> = proc_eval(
                &host,
                r#"
                RPC:connect("conn-natural")
                local P = assert(RPC:context("conn-natural"):get("proc"))
                local exited = nil
                P:spawn({
                    process_id = "p-natural-reap",
                    command = { "sh", "-c", NAT_CMD },
                    tty = false,
                    on_exit = function(ev) exited = ev end,
                })
                for _ = 1, 200 do
                    if exited ~= nil then break end
                    mare.task.sleep(15)
                end
                assert(exited ~= nil, "on_exit must arrive promptly")
                return exited.exit_code
                "#,
            )
            .await;
            assert_eq!(exit_code, Some(0));
            assert_eq!(mare_binding::proc::active_proc_count(), 0);
            assert!(
                mare_binding::proc::active_lingering_tree_count() >= 1,
                "lingering tree + sandbox guard must be tracked while descendant is alive"
            );

            let reaped = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while mare_binding::proc::active_lingering_tree_count() > 0 {
                    // sleep-ok: poll until background lingering reaper drops exited tree
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            })
            .await;
            assert!(reaped.is_ok(), "background reaper must drop lingering tree without further proc calls");

            let _child_pid: u32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
            #[cfg(unix)]
            assert_pid_terminated(_child_pid, "natural-exit lingering child must be terminated").await;

            rpc_disconnect(&host, "conn-natural").await;
            host.shutdown().await;
        })
        .await;
}
