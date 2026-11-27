use super::*;

#[tokio::test]
async fn integration_tests_block_non_loopback_http_by_default() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            local ok, e = pcall(function() return mare.http.request({ url = "https://example.com/" }) end)
            return ok, tostring(e)
            "#,
        )
        .await;
        assert!(!ok && err.contains("offline test guard"), "in-process host must block public HTTP: {err}");
        h.shutdown().await;

        // Child `mare` binary spawned by integration tests receives MARE_TEST_OFFLINE=1 via mare_cmd().
        let (loop_port, _) = http_server(String::new());
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
            local ok, e = pcall(function() return mare.http.request({{ url = "http://1.1.1.1/" }}) end)
            assert(not ok and tostring(e):find("offline test guard", 1, true), "expected offline guard, got: " .. tostring(e))
            local r = mare.http.request({{ url = "http://127.0.0.1:{loop_port}/ok" }})
            assert(r:text() == "ok", "loopback HTTP must remain allowed")
            print("NET_OK")
            os.exit(0)
            "#
            ),
        )
        .unwrap();
        let out = mare_cmd()
            .args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "hi"])
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .run_output();
        assert!(out.status.success(), "mare_bin failed: {}", String::from_utf8_lossy(&out.stderr));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("NET_OK"),
            "init.lua did not complete: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    })
    .await
}

/// Only `tests/common` names the `mare` binary. `mare_bin()` is private
/// there, so the compiler rejects any other way to start `mare` without the offline guard; this
/// check closes the one remaining door, writing the binary path out by hand. The literal can't be
/// aliased, so a plain substring search is complete (no Rust lexing).
#[test]
fn only_tests_common_names_the_mare_binary() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    // tests/common is one module split by responsibility (common-mod-split): any of its files may name the binary
    let common = tests.join("common");
    // spelled in two halves so this file doesn't match itself
    let needle = format!("{}{}", "CARGO_BIN_EXE_", "mare\"");
    let mut stack = vec![tests];
    let mut violations = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") && !path.starts_with(&common) {
                let src = std::fs::read_to_string(&path).unwrap();
                if src.contains(&needle) {
                    violations.push(path.display().to_string());
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "start mare through tests/common (mare_cmd and friends), not by path: {violations:?}"
    );
}

#[tokio::test]
async fn allow_live_overrides_offline_guard_via_mock_proxy() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                hits_srv.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = "from-mock-proxy";
                let resp =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            local ok, r = pcall(function() return mare.http.request({ url = "http://example.com/" }) end)
            if not ok then
                print("ERR:" .. tostring(r))
                os.exit(2)
            end
            print("BODY:" .. r:text())
            os.exit(0)
            "#,
        )
        .unwrap();

        // 1. Isolated child with MARE_TEST_OFFLINE=1 and NO MARE_TEST_ALLOW_LIVE:
        //    must reject http://example.com/ and never touch the mock proxy.
        let mut cmd_offline = mare_cmd();
        cmd_offline
            .args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "hi"])
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .env("MARE_TEST_OFFLINE", "1")
            .env_remove("MARE_TEST_ALLOW_LIVE")
            .env("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"))
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        let out_offline = run_output_async(cmd_offline, DEFAULT_CMD_TIMEOUT).await;
        let out_offline_text =
            format!("{}{}", String::from_utf8_lossy(&out_offline.stdout), String::from_utf8_lossy(&out_offline.stderr));
        assert!(!out_offline.status.success(), "expected offline child to fail: {out_offline_text}");
        assert!(out_offline_text.contains("offline test guard"), "expected offline guard error: {out_offline_text}");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "mock proxy must not be contacted when offline");

        // 2. Same isolated child with MARE_TEST_OFFLINE=1 AND MARE_TEST_ALLOW_LIVE=1:
        //    must bypass the offline guard and hit the local mock proxy.
        let mut cmd_live = mare_cmd();
        cmd_live
            .args(["-p", "--model", "faux/echo", "--no-session", "--yolo", "hi"])
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .env("HOME", env.home.path())
            .env("MARE_TEST_OFFLINE", "1")
            .env("MARE_TEST_ALLOW_LIVE", "1")
            .env("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"))
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        let out_live = run_output_async(cmd_live, DEFAULT_CMD_TIMEOUT).await;
        let out_live_text = String::from_utf8_lossy(&out_live.stdout);
        assert!(
            out_live.status.success(),
            "expected live-opted-in child to succeed, stderr: {}",
            String::from_utf8_lossy(&out_live.stderr)
        );
        assert!(out_live_text.contains("BODY:from-mock-proxy"), "expected proxy response: {out_live_text}");
        assert!(hits.load(Ordering::SeqCst) >= 1, "mock proxy must receive the request when MARE_TEST_ALLOW_LIVE=1");

        srv.abort();
    })
    .await
}

#[tokio::test]
async fn acp_and_pty_children_enforce_offline_guard_by_default() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;

    local(async {
        let (loop_port, _) = http_server(String::new());
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let marker = env.home.path().join("offline_ok");
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
                local ok, e = pcall(function() return mare.http.request({{ url = "http://1.1.1.1/" }}) end)
                assert(not ok and tostring(e):find("offline test guard", 1, true), "expected offline guard, got: " .. tostring(e))
                local r = mare.http.request({{ url = "http://127.0.0.1:{loop_port}/ok" }})
                assert(r:text() == "ok", "loopback HTTP must remain allowed")
                local f = assert(io.open({marker:?}, "w"))
                f:write("ok")
                f:close()
                if os.getenv("MARE_PTY_MARKER") == "1" then
                    print("CHILD_OFFLINE_OK")
                end
                "#,
                marker = marker.to_string_lossy()
            ),
        )
        .unwrap();

        // 1. ACP child launched via mare_cmd()
        let faux_path = env.home.path().join("faux.json");
        std::fs::write(&faux_path, "[]").unwrap();
        let mut cmd = mare_cmd();
        cmd.args(["--model", "faux/echo", "acp"])
            .current_dir(&env.cwd)
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &faux_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut acp = ChildGroup::spawn(&mut cmd).unwrap();
        let mut stdin = acp.0.stdin.take().unwrap();
        let mut stdout = BufReader::new(acp.0.stdout.take().unwrap());
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"clientCapabilities":{{}}}}}}"#).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(line.contains(r#""id":1"#), "expected initialize response, got: {line}");
        let new_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": env.cwd, "mcpServers": [] }
        });
        writeln!(stdin, "{new_req}").unwrap();
        stdin.flush().unwrap();
        let mut session_new_reply = String::new();
        loop {
            session_new_reply.clear();
            let n = stdout.read_line(&mut session_new_reply).unwrap();
            assert!(n > 0, "acp stdout closed before session/new response");
            if session_new_reply.contains(r#""id":2"#) {
                break;
            }
        }
        drop(stdin);
        let status = acp.0.wait().unwrap();
        assert!(status.success(), "acp child failed: reply={session_new_reply:?}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ok");
        std::fs::remove_file(&marker).unwrap();

        // 2. PTY child with MARE_* stripped (matching pty.rs) + explicit MARE_TEST_OFFLINE=1 re-injection
        let mut pty_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(k, _)| k.to_str().is_none_or(|s| !s.starts_with("MARE_") && s != "TERM"))
            .collect();
        pty_env.push(("MARE_HOME".into(), env.home.path().as_os_str().into()));
        pty_env.push(("HOME".into(), env.home.path().as_os_str().into()));
        pty_env.push(("TERM".into(), "xterm-256color".into()));
        pty_env.push(("MARE_NO_TERM_PROBE".into(), "1".into()));
        pty_env.push(("MARE_PTY_MARKER".into(), "1".into()));
        let prog = mare_pty_program(&mut pty_env);
        let args = ["-p", "--model", "faux/echo", "--no-session", "--yolo", "hi"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let cwd = env.cwd.to_string_lossy().into_owned();
        let (mut term, mut reader, _writer) = mare_binding::platform::Terminal::spawn(
            mare_binding::platform::TerminalCommand {
                prog,
                args: &args,
                cwd: &cwd,
                env: pty_env,
                cols: 100,
                rows: 30,
            },
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if String::from_utf8_lossy(&buf).contains("CHILD_OFFLINE_OK") {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        let output = rx.recv_timeout(std::time::Duration::from_secs(15)).expect("timed out waiting for PTY child");
        drop(_writer);
        let _ = term.kill();
        let _ = term.wait();
        let _ = reader_thread.join();
        assert!(output.contains("CHILD_OFFLINE_OK"), "PTY child did not pass offline+loopback check: {output}");
    })
    .await
}
