use mlua::LuaSerdeExt;

use super::*;

fn test_python() -> String {
    if !cfg!(windows) {
        return "python3".to_string();
    }
    if let Some(p) = std::env::var_os("PYTHON").filter(|v| !v.is_empty()) {
        return std::path::PathBuf::from(p).to_string_lossy().into_owned();
    }
    if let Some(path) = std::env::var_os("PATH") {
        for name in ["python.exe", "python3.exe"] {
            for dir in std::env::split_paths(&path) {
                let cand = dir.join(name);
                // Windows App Execution Aliases (`...\WindowsApps\python3.exe`) are Store stubs,
                // not real Python, and fail inside sandboxed / headless child processes.
                let is_store_alias =
                    cand.components().any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case("WindowsApps"));
                if !is_store_alias && cand.is_file() {
                    return cand.to_string_lossy().into_owned();
                }
            }
        }
    }
    "python".to_string()
}

#[tokio::test]
async fn proc_service_piped_pty_sandbox_and_cleanup() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let outside_target = outside.path().join("escape.txt");

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
                if cfg!(windows) {
                    r#"{ level = "workspace-write", accept_partial = { "fs_protect", "net_block" }, exclude_slash_tmp = true, exclude_tmpdir_env_var = true }"#
                } else {
                    r#"{ level = "workspace-write", exclude_slash_tmp = true, exclude_tmpdir_env_var = true }"#
                },
            );

            host.lua().globals().set("OUTSIDE_TARGET", outside_target.to_string_lossy().to_string()).unwrap();

            // 1. Piped process: write stdin + close_stdin -> stdout/stderr deltas + exit 0.
            // 2. Sandbox enforcement: spawned child cannot write outside writable roots.
            // 3. PTY process: resize + write + output + exit.
            // 4. proc.kill terminates a running process.
            let (piped_cmd, sandbox_cmd, pty_cmd, kill_cmd, linger_cmd) = if cfg!(windows) {
                let py = test_python();
                (
                    serde_json::json!([&py, "-c", "import sys; sys.stdout.write(sys.stdin.read()); sys.stdout.flush(); sys.stderr.write('errline\\n'); sys.stderr.flush()"]),
                    serde_json::json!(["cmd", "/d", "/c", format!("echo x > \"{}\"", outside_target.display())]),
                    serde_json::json!([&py, "-c", "import os, sys; s1 = os.get_terminal_size(); print(f'{s1.lines} {s1.columns}', flush=True); line = sys.stdin.readline().rstrip('\\r\\n'); s2 = os.get_terminal_size(); print(f'after:{s2.lines} {s2.columns}:{line}', flush=True)"]),
                    serde_json::json!([&py, "-c", "import time; time.sleep(60)"]),
                    serde_json::json!([&py, "-c", "import time; print('ready', flush=True); time.sleep(60)"]),
                )
            } else {
                (
                    serde_json::json!(["sh", "-c", "cat; echo errline >&2"]),
                    serde_json::json!(["sh", "-c", format!("echo x > '{}'", outside_target.display())]),
                    serde_json::json!(["sh", "-c", "stty size; read line; echo after:$(stty size):$line"]),
                    serde_json::json!(["cat"]),
                    serde_json::json!(["sh", "-c", "echo ready; exec cat"]),
                )
            };
            host.lua().globals().set("CMD_PIPED", host.lua().to_value(&piped_cmd).unwrap()).unwrap();
            host.lua().globals().set("CMD_SANDBOX", host.lua().to_value(&sandbox_cmd).unwrap()).unwrap();
            host.lua().globals().set("CMD_PTY", host.lua().to_value(&pty_cmd).unwrap()).unwrap();
            host.lua().globals().set("CMD_KILL", host.lua().to_value(&kill_cmd).unwrap()).unwrap();
            host.lua().globals().set("CMD_LINGER", host.lua().to_value(&linger_cmd).unwrap()).unwrap();
            let (out, err, pty_out): (String, String, String) = proc_eval(
                &host,
                r#"
                RPC:connect("conn-pty")
                local cctx = assert(RPC:context("conn-pty"))
                local P = assert(cctx:get("proc"))
                local events = {}
                local function collect(id, stream_name)
                    local parts = {}
                    for _, e in ipairs(events) do
                        if e.id == id and (not stream_name or e.stream == stream_name) then
                            parts[#parts + 1] = e.chunk
                        end
                    end
                    return table.concat(parts)
                end
                local function wait_exit(id)
                    for _ = 1, 600 do
                        for _, e in ipairs(events) do
                            if e.exit and e.id == id then
                                return e.code
                            end
                        end
                        mare.task.sleep(25)
                    end
                    error("timed out waiting for exit of " .. id)
                end

                local r1 = P:spawn({
                    process_id = "p1",
                    command = CMD_PIPED,
                    tty = false,
                    on_output = function(ev)
                        events[#events + 1] = { id = ev.process_id, stream = ev.stream, chunk = mare.crypto.base64_decode(ev.data_base64) }
                    end,
                    on_exit = function(ev)
                        events[#events + 1] = { id = ev.process_id, exit = true, code = ev.exit_code }
                    end,
                })
                assert(r1.process_id == "p1")
                P:write({ process_id = "p1", data_base64 = mare.crypto.base64_encode("piped-in\n"), close_stdin = true })
                assert(wait_exit("p1") == 0)

                P:spawn({
                    process_id = "p-sb",
                    command = CMD_SANDBOX,
                    tty = false,
                    on_exit = function(ev)
                        events[#events + 1] = { id = ev.process_id, exit = true, code = ev.exit_code }
                    end,
                })
                local sb_code = wait_exit("p-sb")
                assert(sb_code ~= 0, "sandbox must refuse outside write, got " .. tostring(sb_code))

                P:spawn({
                    process_id = "p-pty",
                    command = CMD_PTY,
                    tty = true,
                    size = { rows = 24, cols = 80 },
                    on_output = function(ev)
                        events[#events + 1] = { id = ev.process_id, stream = ev.stream, chunk = mare.crypto.base64_decode(ev.data_base64) }
                    end,
                    on_exit = function(ev)
                        events[#events + 1] = { id = ev.process_id, exit = true, code = ev.exit_code }
                    end,
                })
                local pty_ready = false
                for _ = 1, 1000 do
                    if collect("p-pty", "pty"):find("24 80", 1, true) then
                        pty_ready = true
                        break
                    end
                    mare.task.sleep(10)
                end
                assert(pty_ready, "initial 24 80 missing: " .. collect("p-pty", "pty"))
                P:resize({ process_id = "p-pty", size = { rows = 40, cols = 120 } })
                local pty_nl = mare.os == "windows" and "\r" or "\n"
                P:write({ process_id = "p-pty", data_base64 = mare.crypto.base64_encode("hi-pty" .. pty_nl) })
                assert(wait_exit("p-pty") == 0)

                P:spawn({
                    process_id = "p-kill",
                    command = CMD_KILL,
                    tty = false,
                    on_exit = function(ev)
                        events[#events + 1] = { id = ev.process_id, exit = true, code = ev.exit_code }
                    end,
                })
                P:kill({ process_id = "p-kill" })
                wait_exit("p-kill")
                return collect("p1", "stdout"), collect("p1", "stderr"), collect("p-pty", "pty")
                "#,
            )
            .await;

            assert!(out.contains("piped-in"), "stdout: {out:?}");
            assert!(err.contains("errline"), "stderr: {err:?}");
            assert!(!outside_target.exists(), "outside file must not be written by sandboxed process");
            assert!(pty_out.contains("24 80"), "initial stty size missing in pty output: {pty_out:?}");
            assert!(pty_out.contains("after:40 120:hi-pty"), "resized stty size missing in pty output: {pty_out:?}");
            assert_eq!(mare_binding::proc::active_proc_count(), 0, "p-kill must drop active count to 0");

            // 5. Start a lingering process before disconnecting.
            let (): () = proc_eval(
                &host,
                r#"
                local P = assert(RPC:context("conn-pty"):get("proc"))
                local ready = false
                P:spawn({
                    process_id = "p-linger",
                    command = CMD_LINGER,
                    tty = false,
                    on_output = function(ev)
                        local txt = mare.crypto.base64_decode(ev.data_base64)
                        if txt:find("ready", 1, true) then ready = true end
                    end,
                })
                for _ = 1, 200 do
                    if ready then break end
                    mare.task.sleep(10)
                end
                assert(ready, "lingering process failed to report ready")
                "#,
            )
            .await;

            let linger_pid = mare_binding::proc::last_spawned_pid();
            assert_ne!(linger_pid, 0);
            assert_eq!(mare_binding::proc::active_proc_count(), 1);
            #[cfg(unix)]
            assert!(
                std::process::Command::new("kill").args(["-0", &linger_pid.to_string()]).status().unwrap().success(),
                "lingering process {linger_pid} should be alive before disconnect"
            );

            rpc_disconnect(&host, "conn-pty").await;
            assert_eq!(mare_binding::proc::active_proc_count(), 0, "process task must be gone after disconnect");
            #[cfg(unix)]
            assert!(
                !std::process::Command::new("kill").args(["-0", &linger_pid.to_string()]).status().unwrap().success(),
                "lingering process {linger_pid} must be dead after disconnect"
            );
            #[cfg(target_os = "linux")]
            assert!(
                !std::path::Path::new(&format!("/proc/{linger_pid}")).exists(),
                "lingering process {linger_pid} must be reaped (not zombie) after disconnect"
            );

            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn proc_service_write_pty_close_stdin_writes_zero_bytes_on_error() {
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

            let pty_out: String = proc_eval(
                &host,
                r#"
                RPC:connect("conn-pty-err")
                local P = assert(RPC:context("conn-pty-err"):get("proc"))
                local chunks, exited_code = {}, nil
                P:spawn({
                    process_id = "p-pty-err",
                    command = { "sh", "-c", "read line; echo got:$line" },
                    tty = true,
                    size = { rows = 24, cols = 80 },
                    on_output = function(ev)
                        chunks[#chunks + 1] = mare.crypto.base64_decode(ev.data_base64)
                    end,
                    on_exit = function(ev)
                        exited_code = ev.exit_code
                    end,
                })
                local ok = pcall(function()
                    P:write({ process_id = "p-pty-err", data_base64 = mare.crypto.base64_encode("forbidden\n"), close_stdin = true })
                end)
                assert(not ok, "close_stdin=true on PTY must fail")
                P:write({ process_id = "p-pty-err", data_base64 = mare.crypto.base64_encode("allowed\n") })
                for _ = 1, 200 do
                    if exited_code ~= nil then break end
                    mare.task.sleep(15)
                end
                assert(exited_code == 0, "expected exit 0, got " .. tostring(exited_code))
                return table.concat(chunks)
                "#,
            )
            .await;

            rpc_disconnect(&host, "conn-pty-err").await;

            assert!(!pty_out.contains("forbidden"), "failed write must send 0 bytes: {pty_out:?}");
            assert!(pty_out.contains("got:allowed"), "child must receive only the valid write: {pty_out:?}");
            assert_eq!(mare_binding::proc::active_proc_count(), 0);
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn proc_service_spawn_pipe_and_pty_apply_net_fake_ip_off() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
                "fake.test".to_string(),
                vec!["198.18.0.5".parse().unwrap()],
            )])));
            let ws = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"enabled": false}},
                    "net": {"fake_ip": "auto"}
                }),
                load_init: false,
                project_trusted: false,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            set_lua_sandbox(
                &host,
                if cfg!(windows) {
                    r#"{ level = "workspace-write", network = { allow = { "fake.test" } }, accept_partial = { "fs_protect", "net_proxy" }, exclude_slash_tmp = true, exclude_tmpdir_env_var = true }"#
                } else {
                    r#"{ level = "workspace-write", network = { allow = { "fake.test" } }, accept_partial = { "net_proxy" }, exclude_slash_tmp = true, exclude_tmpdir_env_var = true }"#
                },
            );
            host.lua()
                .load(
                    "if mare.o and mare.o.net then mare.o.net.fake_ip = 'off' end if mare.config and mare.config.net then mare.config.net.fake_ip = 'off' end",
                )
                .exec()
                .unwrap();

            host.lua().globals().set("PYTHON_CMD", test_python()).unwrap();
            for tty in [false, true] {
                let handle = if tty { "p-pty-off" } else { "p-pipe-off" };
                host.lua().globals().set("TRY_TTY", tty).unwrap();
                host.lua().globals().set("TRY_ID", handle).unwrap();
                let out: String = proc_eval(
                    &host,
                    r#"
                    local P = assert(PROC)
                    local py = "import os, socket, urllib.parse; u = urllib.parse.urlparse(os.environ['HTTP_PROXY']); s = socket.create_connection((u.hostname, u.port), 5); s.sendall(b'CONNECT fake.test:80 HTTP/1.1\\r\\nHost: fake.test:80\\r\\n\\r\\n'); print(s.recv(4096).decode(errors='replace'))"
                    local chunks, exited = {}, false
                    P:spawn({
                        process_id = TRY_ID,
                        command = { PYTHON_CMD, "-c", py },
                        tty = TRY_TTY,
                        size = { rows = 24, cols = 80 },
                        on_output = function(ev) chunks[#chunks + 1] = mare.crypto.base64_decode(ev.data_base64) end,
                        on_exit = function() exited = true end,
                    })
                    for _ = 1, 200 do
                        if exited then break end
                        mare.task.sleep(20)
                    end
                    assert(exited, "expected process exit for " .. TRY_ID)
                    return table.concat(chunks)
                    "#,
                )
                .await;
                assert!(
                    out.contains("403 Forbidden") && out.contains("local/private address"),
                    "proc.spawn (tty={tty}) must enforce net.fake_ip=\"off\" over existing auto policy and reject fake-IP as local/private, got: {out:?}"
                );
            }

            mare_binding::netauth::set_resolver_override(None);
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn proc_service_rpc_isolation_exec_ctx_and_validation_errors() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let outside_file = outside.path().join("exec_ctx_escape.txt");
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

            // 1. Active tool execution context (`mare.exec.run`) confines `proc.spawn`.
            // 2. Error paths report `proc.unknown_process`.
            host.lua().globals().set("EXEC_OUTSIDE", outside_file.to_string_lossy().to_string()).unwrap();
            let exec_cmd = if cfg!(windows) {
                serde_json::json!(["cmd", "/d", "/c", format!("echo blocked > \"{}\"", outside_file.display())])
            } else {
                serde_json::json!(["sh", "-c", format!("echo blocked > '{}'", outside_file.display())])
            };
            host.lua().globals().set("EXEC_CMD", host.lua().to_value(&exec_cmd).unwrap()).unwrap();
            host.lua()
                .globals()
                .set(
                    "EXEC_ACCEPT_PARTIAL",
                    host.lua()
                        .to_value(&if cfg!(windows) { vec!["fs_protect", "net_block"] } else { Vec::new() })
                        .unwrap(),
                )
                .unwrap();
            host.lua().globals().set("PYTHON_CMD", test_python()).unwrap();
            let (): () = proc_eval(
                &host,
                r#"
                RPC:connect("conn-err")
                local P = assert(RPC:context("conn-err"):get("proc"))

                local function expect_code(code, label, fn)
                    local ok, err = pcall(fn)
                    assert(not ok, label .. " must fail")
                    assert(tostring(err):find(code, 1, true), label .. " expected " .. code .. ", got: " .. tostring(err))
                end

                -- Unknown process_id on write/resize/kill before any spawn.
                expect_code("proc.unknown_process", "write-no-mgr", function() P:write({ process_id = "missing" }) end)
                expect_code("proc.unknown_process", "resize-no-mgr", function() P:resize({ process_id = "missing", size = { rows = 24, cols = 80 } }) end)
                expect_code("proc.unknown_process", "kill-no-mgr", function() P:kill({ process_id = "missing" }) end)

                -- Active tool execution context (`mare.exec.run`) confines `proc.spawn`.
                local exec_code = nil
                local sb = { level = "workspace-write", exclude_slash_tmp = true, exclude_tmpdir_env_var = true }
                if #EXEC_ACCEPT_PARTIAL > 0 then sb.accept_partial = EXEC_ACCEPT_PARTIAL end
                local run_ok = mare.exec.run(
                    { sandbox = sb },
                    function()
                        P:spawn({
                            process_id = "p-exec-ctx",
                            command = EXEC_CMD,
                            tty = false,
                            on_exit = function(ev) exec_code = ev.exit_code end,
                        })
                        for _ = 1, 200 do
                            if exec_code ~= nil then break end
                            mare.task.sleep(10)
                        end
                        return true
                    end
                )
                assert(run_ok == true)
                assert(exec_code ~= nil and exec_code ~= 0, "exec_ctx must refuse outside write, got " .. tostring(exec_code))

                local piped_cmd = mare.os == "windows" and { PYTHON_CMD, "-c", "import sys; sys.stdin.read()" } or { "cat" }
                local zero_cmd = mare.os == "windows" and { "cmd", "/d", "/c", "exit", "0" } or { "true" }
                -- Validation error matrix with an active owner manager.
                local exited = false
                P:spawn({
                    process_id = "p-piped",
                    command = piped_cmd,
                    tty = false,
                    on_exit = function() exited = true end,
                })
                expect_code("invalid_arguments", "duplicate-id", function()
                    P:spawn({ process_id = "p-piped", command = piped_cmd })
                end)
                expect_code("invalid_arguments", "resize-non-pty", function()
                    P:resize({ process_id = "p-piped", size = { rows = 24, cols = 80 } })
                end)
                expect_code("invalid_arguments", "invalid-base64", function()
                    P:write({ process_id = "p-piped", data_base64 = "!!!not-base64!!!" })
                end)
                expect_code("invalid_arguments", "zero-pty-spawn", function()
                    P:spawn({ process_id = "p-zero", command = zero_cmd, tty = true, size = { rows = 0, cols = 80 } })
                end)
                P:write({ process_id = "p-piped", data_base64 = mare.crypto.base64_encode("hi\n"), close_stdin = true })
                expect_code("invalid_arguments", "write-after-close-stdin", function()
                    P:write({ process_id = "p-piped", data_base64 = mare.crypto.base64_encode("more\n") })
                end)
                expect_code("proc.unknown_process", "unknown-write", function() P:write({ process_id = "nope" }) end)
                expect_code("proc.unknown_process", "unknown-resize", function() P:resize({ process_id = "nope", size = { rows = 24, cols = 80 } }) end)
                expect_code("proc.unknown_process", "unknown-kill", function() P:kill({ process_id = "nope" }) end)
                for _ = 1, 200 do
                    if exited then break end
                    mare.task.sleep(10)
                end
                assert(exited, "p-piped must exit after close_stdin")
                "#,
            )
            .await;

            assert!(!outside_file.exists(), "mare.exec.run sandbox must prevent outside write via proc.spawn");
            rpc_disconnect(&host, "conn-err").await;
            assert_eq!(mare_binding::proc::active_proc_count(), 0);

            // 3. Over remote RPC: `services.call` is removed (`method_not_found`), `proc.spawn` is denied,
            // and `services.register { name = "proc", replace = true }` is rejected.
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let init = call_rpc(&mut cw, &mut cr, 1, "initialize", json!({"protocol_version": "3.0", "capabilities": {"callbacks": true}})).await;
            assert!(init.get("error").is_none(), "{init}");

            let svc_call = call_rpc(
                &mut cw,
                &mut cr,
                2,
                "services.call",
                json!({"service": "proc", "method": "spawn", "arguments": {"process_id": "x", "command": ["true"]}}),
            )
            .await;
            assert_eq!(svc_call["error"]["code"], codes::METHOD_NOT_FOUND, "{svc_call}");

            let svc_reg = call_rpc(
                &mut cw,
                &mut cr,
                3,
                "services.register",
                json!({"name": "proc", "methods": {"spawn": {}}}),
            )
            .await;
            assert!(svc_reg.get("error").is_some(), "replacing built-in proc service via RPC must fail: {svc_reg}");
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}
