use super::*;

/// Fake line server: echoes each line as {"ok":<line>}; lines containing "silent" get no answer.
fn echo_server(sock: &std::path::Path) {
    let listener = UnixListener::bind(sock).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut w = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if !line.contains("silent") {
                        let _ = writeln!(w, "{{\"ok\":{line}}}");
                    }
                }
            });
        }
    });
}

#[tokio::test]
async fn unix_client_requests_persists_times_out_and_reports_errors() {
    local(async {
        let env = TestEnv::new().await;
        let sock = env.home.path().join("s.sock");
        echo_server(&sock);
        let h = env.host().no_session().build().await;
        global(&h, "SOCK", sock.to_string_lossy().to_string());
        let (one, a, b, timeout_err, missing_err): (String, String, String, String, String) = eval(
            &h,
            r#"
            local u = mare.net.unix
            local one = u.request(SOCK, '{"id":1}', { timeout_ms = 1000 })
            local c = assert(u.connect(SOCK, { timeout_ms = 1000 }))
            assert(c:write_line('{"id":2}'))
            local a = c:read_line({ timeout_ms = 1000 })
            assert(c:write_line('{"id":3}\n'))
            local b = c:read_line({ timeout_ms = 1000 })
            c:close()
            local r, terr = u.request(SOCK, '{"method":"silent"}', { timeout_ms = 200 })
            assert(r == nil)
            local _, merr = u.connect(SOCK .. ".missing", { timeout_ms = 200 })
            return one, a, b, terr, merr
        "#,
        )
        .await;
        assert_eq!(one, r#"{"ok":{"id":1}}"#);
        assert_eq!((a.as_str(), b.as_str()), (r#"{"ok":{"id":2}}"#, r#"{"ok":{"id":3}}"#));
        assert_eq!(timeout_err, "timeout");
        assert!(!missing_err.is_empty());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unix_listen_serves_concurrent_clients_with_0600_and_unlinks_on_close() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let lsock = env.home.path().join("peers/l.sock");
        global(&h, "LSOCK", lsock.to_string_lossy().to_string());
        let (r1, r2, busy): (String, String, bool) = eval(
            &h,
            r#"
            local u = mare.net.unix
            SRV = assert(u.listen(LSOCK, function(conn)
                local line = conn:read_line()
                conn:write_line("echo:" .. line)
                conn:close()
            end))
            local c1, c2 = assert(u.connect(LSOCK)), assert(u.connect(LSOCK))
            c2:write_line("b"); c1:write_line("a")
            local r1, r2 = c1:read_line(), c2:read_line()
            return r1, r2, u.listen(LSOCK, function() end) == nil   -- live socket: refused
        "#,
        )
        .await;
        assert_eq!((r1.as_str(), r2.as_str(), busy), ("echo:a", "echo:b", true));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&lsock).unwrap().permissions().mode() & 0o777, 0o600);
        exec(&h, "SRV:close()").await;
        assert!(!lsock.exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn wait_events_wrap_outermost_ui_prompts_including_replacements() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let log: Vec<String> = eval(
            &h,
            r#"
            local log = {}
            C.cordis:on("ui/wait-started", function(e) log[#log + 1] = "start:" .. e.kind .. ":" .. tostring(e.title) end)
            C.cordis:on("ui/wait-completed", function(e) log[#log + 1] = "end:" .. e.kind end)
            mare.ui.select = function(opts) return 1 end
            assert(mare.ui.confirm("Push?") == true)           -- confirm → select is one wait
            local saved = mare.ui.input                         -- what a frontend saves before forwarding
            mare.ui.input = function() return "pw" end
            assert(mare.ui.input({ title = "Password" }) == "pw")
            mare.ui.input = saved                               -- restoring must not double-wrap
            mare.ui.input = function() return "x" end
            mare.ui.input({ title = "Again" })
            return log
        "#,
        )
        .await;
        assert_eq!(
            log,
            [
                "start:confirm:Push?",
                "end:confirm",
                "start:input:Password",
                "end:input",
                "start:input:Again",
                "end:input"
            ]
        );
        h.shutdown().await;
    })
    .await
}
