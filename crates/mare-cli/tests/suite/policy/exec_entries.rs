//! The enumerated tool entries must honor an active execution context.
use crate::common::*;

async fn setup(env: &TestEnv, h: &mare_host::Host) {
    std::fs::create_dir_all(env.cwd.join("secret")).unwrap();
    std::fs::write(env.cwd.join("secret/a.txt"), "hidden").unwrap();
    std::fs::write(env.cwd.join("visible.txt"), "visible").unwrap();
    global(h, "CWD", env.cwd.to_string_lossy().to_string());
    global(h, "WINDOWS", cfg!(windows));
    exec(
        h,
        r#"
        DENY = { cwd = CWD, sandbox = { level = "read-only", network = false,
            deny_read = { CWD .. "/secret" }, accept_partial = { "fs_deny_read" } } }
        function denied(name, f)
            local ok, value, err = pcall(f)
            local refused = not ok or (value == nil and err ~= nil)
            local msg = ok and tostring(err) or tostring(value)
            local is_sandbox = msg:find("sandbox:", 1, true)
                and (msg:find("denied", 1, true) or msg:find("can't be confined", 1, true))
            assert(refused and is_sandbox, name .. " was not refused by sandbox: " .. msg)
        end
    "#,
    )
    .await;
}

/// reference implementation: every path
/// containing NUL is rejected before entering the underlying operation.
#[tokio::test]
async fn shadowing_private_exec_helpers_cannot_disable_stdlib_checks() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        exec(
            &h,
            r#"
            -- Even if code creates these names, stdlib wrappers must use their own checkers.
            mare.exec.check_process_path = function() end
            mare.exec.reject_nul_path = function() end
            local visible = CWD .. "/visible.txt"
            local secret = CWD .. "/secret/a.txt"
            local f = assert(io.open(visible, "r"))
            assert(f:read("*a") == "visible"); f:close()
            mare.exec.run(DENY, function()
                local cases = {
                    open_read = function() local file = assert(io.open(secret, "r")); file:close() end,
                    lines = function() for _ in io.lines(secret) do break end end,
                    input = function() return io.input(secret) end,
                    open_write = function() return io.open(visible, "w") end,
                    output = function() return io.output(visible) end,
                    remove = function() return os.remove(visible) end,
                    rename = function() return os.rename(visible, visible .. ".moved") end,
                }
                for name, operation in pairs(cases) do denied(name, operation) end
            end)
            local bad = visible .. string.char(0) .. "ignored"
            local nul_cases = {
                function() return io.open(bad, "r") end,
                function() return io.lines(bad) end,
                function() return io.input(bad) end,
                function() return io.output(bad) end,
                function() return os.remove(bad) end,
                function() return os.rename(bad, visible) end,
                function() return os.rename(visible, bad) end,
            }
            for i, operation in ipairs(nul_cases) do
                local ok, err = pcall(operation)
                assert(not ok and tostring(err):find("file path contains NUL", 1, true), i .. ": " .. tostring(err))
            end
            "#,
        )
        .await;
        assert_eq!(std::fs::read_to_string(env.cwd.join("visible.txt")).unwrap(), "visible");
        assert!(!env.cwd.join("visible.txt.moved").exists());
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn file_reads_in_a_denying_context_are_refused() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        exec(
            &h,
            r#"
            local path = CWD .. "/secret/a.txt"
            local cases = {
                read = function() return mare.fs.read(path) end,
                read_window = function() return mare.fs.read_window(path) end,
                sniff = function() return mare.fs.sniff(path) end,
                list = function() return mare.fs.list(CWD .. "/secret") end,
                io_open = function() local f = assert(io.open(path, "r")); f:close(); return true end,
                io_lines = function() for _ in io.lines(path) do break end; return true end,
            }
            for name, f in pairs(cases) do assert(f() ~= nil, name .. " control failed") end
            mare.exec.run(DENY, function()
                for name, f in pairs(cases) do denied(name, f) end
                assert(mare.fs.read(CWD .. "/visible.txt") == "visible")
            end)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn file_writes_in_a_denying_context_are_refused() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        exec(
            &h,
            r#"
            local cases = {
                write = function(p) return mare.fs.write(p, "changed") end,
                mkdir = function(p) mare.fs.mkdir(p); return true end,
                remove = function(p) mare.fs.remove(p); return true end,
                rename = function(p) mare.fs.rename(p, p .. ".moved"); return true end,
                io_open = function(p) local f = assert(io.open(p, "w")); f:write("changed"); f:close(); return true end,
                os_remove = function(p) return os.remove(p) end,
            }
            for name, f in pairs(cases) do
                local p = CWD .. "/control-" .. name
                if name == "remove" or name == "rename" or name == "os_remove" then
                    assert(mare.fs.write(p, "original"))
                end
                assert(f(p), name .. " control failed")
            end
            mare.exec.run(DENY, function()
                for name, f in pairs(cases) do
                    local p = (name == "remove" or name == "rename" or name == "os_remove")
                        and CWD .. "/visible.txt" or CWD .. "/denied-" .. name
                    denied(name, function() return f(p) end)
                end
            end)
        "#,
        )
        .await;
        assert_eq!(std::fs::read_to_string(env.cwd.join("visible.txt")).unwrap(), "visible");
        for name in ["write", "mkdir", "io_open"] {
            assert!(!env.cwd.join(format!("denied-{name}")).exists(), "{name}");
        }
        assert!(!env.cwd.join("visible.txt.moved").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn recursive_walks_in_a_deny_read_context_are_refused() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        exec(
            &h,
            r#"
            local opts = { hidden = true, gitignore = false }
            local function paths(index)
                if index then local i = mare.fs.index(CWD, opts); i:refresh({min_interval_ms = 0}); return i:files() end
                return mare.fs.walk(CWD, opts)
            end
            local function contains(paths, text)
                for _, p in ipairs(paths) do if p:find(text, 1, true) then return true end end
                return false
            end
            for _, index in ipairs({false, true}) do
                local all = paths(index)
                assert(contains(all, "visible.txt") and contains(all, "secret"), "control")
            end
            -- A recursive listing cannot be confined to the readable part of a deny_read tree,
            -- so inside the context it is refused outright (conservative; no partial listing).
            mare.exec.run(DENY, function()
                for _, case in ipairs({
                    {"walk", function() return mare.fs.walk(CWD, opts) end, false},
                    {"walk secret", function() return mare.fs.walk(CWD .. "/secret", opts) end, true},
                    {"index", function() return paths(true) end, false},
                }) do
                    local ok, err = pcall(case[2])
                    local msg = tostring(err)
                    local confined_err = msg:find("cannot be safely confined with deny_read", 1, true)
                    local read_err = case[3] and msg:find("sandbox: read", 1, true) and msg:find("denied", 1, true)
                    assert(not ok and (confined_err or read_err), case[1] .. ": " .. msg)
                end
            end)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

const PROCESS_HELPERS: &str = r#"
    function shell_cmd(n)
        return WINDOWS and ("type nul > spawned-" .. n) or ("touch spawned-" .. n)
    end
    function native_shell_cmd(n)
        local prefix = WINDOWS and ('cd /d "' .. CWD .. '" && ') or ('cd "' .. CWD .. '" && ')
        return prefix .. shell_cmd(n)
    end
    function opts(n)
        return {cmd = WINDOWS and "cmd" or "sh", args = {WINDOWS and "/c" or "-c", shell_cmd(n)}, cwd = CWD}
    end
    function spawn_case(n)
        local o = opts(n)
        if n == 1 then return mare.proc.run(o) end
        if n == 2 then return mare.proc.start(o):wait() end
        if n == 3 then return mare.proc.spawn(o):wait() end
        local p = mare.pty.spawn(o)
        for _ = 1, 500 do
            if p:try_wait() ~= nil then return true end
            mare.task.sleep(10)
        end
        error("PTY did not exit")
    end
"#;

async fn process_cases(force_none: bool) {
    let mut env = TestEnv::new().await;
    let h = env.host().no_session().build().await;
    setup(&env, &h).await;
    exec(&h, PROCESS_HELPERS).await;
    // Controls use the same calls without a context and must actually create their files.
    exec(
        &h,
        r#"
        for i = 1, 4 do spawn_case(i) end
        -- Native shell APIs use the process cwd, so quote the workspace explicitly.
        local pipe = assert(io.popen(native_shell_cmd(5)))
        assert(pipe:close())
        assert(os.execute(native_shell_cmd(6)))
    "#,
    )
    .await;
    for n in 1..=6 {
        let path = env.cwd.join(format!("spawned-{n}"));
        assert!(path.exists(), "control spawn {n}");
        std::fs::remove_file(path).unwrap();
    }
    if force_none {
        env.set_var("MARE_SANDBOX_BACKEND", "none");
    }
    global(&h, "FORCE_NONE", force_none);
    exec(
        &h,
        r#"
        mare.exec.run(DENY, function()
            for i = 1, 4 do
                local ok, result = pcall(spawn_case, i)
                if not ok then
                    assert(tostring(result):find("SANDBOX_UNAVAILABLE", 1, true)
                        or (not FORCE_NONE and tostring(result):find("SANDBOX_INSUFFICIENT", 1, true))
                        or (i == 4 and tostring(result):find("PTY_POLICY", 1, true)),
                        "spawn " .. i .. ": " .. tostring(result))
                else
                    assert(not FORCE_NONE, "backend none allowed spawn " .. i)
                end
            end
            denied("io.popen", function()
                return io.popen(native_shell_cmd(5))
            end)
            denied("os.execute", function()
                return os.execute(native_shell_cmd(6))
            end)
        end)
        if FORCE_NONE then
            -- A PTY requires its persistent session policy to match the tool policy.
            mare.proc.sandbox.set_current(DENY.sandbox, CWD)
            mare.exec.run(DENY, function()
                local ok, err = pcall(spawn_case, 4)
                assert(not ok and tostring(err):find("SANDBOX_UNAVAILABLE", 1, true), tostring(err))
            end)
            mare.proc.sandbox.set_current(nil)
        end
    "#,
    )
    .await;
    for n in 1..=6 {
        assert!(!env.cwd.join(format!("spawned-{n}")).exists(), "spawn {n} escaped");
    }
    h.shutdown().await;
}

#[tokio::test]
async fn process_spawns_in_a_denying_context_are_confined_or_refused() {
    local(process_cases(false)).await
}

#[tokio::test]
async fn process_spawns_without_backend_are_refused() {
    local(process_cases(true)).await
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn process_open_is_refused_before_starting_the_opener() {
    use std::os::unix::fs::PermissionsExt;
    local(async {
        let mut env = TestEnv::new().await;
        let opener = env.cwd.join("xdg-open");
        let marker = env.cwd.join("opener-ran");
        std::fs::write(&opener, format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![env.cwd.clone()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let path = std::env::join_paths(paths).unwrap();
        env.set_var("PATH", path);
        // TestEnv sets BROWSER=true to prevent real browsers from opening; clear it here so
        // this control test exercises the fallback `xdg-open` opener in PATH.
        env.set_var("BROWSER", "");
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        exec(&h, r#"mare.proc.open("http://127.0.0.1/")"#).await;
        assert!(marker.exists(), "control opener did not run");
        std::fs::remove_file(&marker).unwrap();
        exec(
            &h,
            r#"
            mare.exec.run(DENY, function()
                denied("proc.open", function() mare.proc.open("http://127.0.0.1/"); return true end)
            end)
        "#,
        )
        .await;
        assert!(!marker.exists(), "opener ran under denying context");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn process_open_with_browser_true_does_not_invoke_system_opener() {
    local(async {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut env = TestEnv::new().await;
        let marker = env.cwd.join("system-opener-ran");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["xdg-open", "open"] {
                let opener = env.cwd.join(name);
                std::fs::write(&opener, format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display())).unwrap();
                std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let mut paths = vec![env.cwd.clone()];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            env.set_var("PATH", std::env::join_paths(paths).unwrap());
        }
        // TestEnv sets BROWSER=true by default; assert mare.proc.open succeeds as a no-op
        // without invoking the system opener on any platform.
        assert_eq!(std::env::var("BROWSER").as_deref(), Ok("true"));
        let h = env.host().no_session().build().await;
        exec(&h, r#"mare.proc.open("http://127.0.0.1/authorize?client_id=test-client")"#).await;
        assert!(!marker.exists(), "system opener must not run when BROWSER=true");
        h.shutdown().await;
    })
    .await
}

/// Codex `codex-rs/linux-sandbox/tests/suite/sandbox.rs:883`: network policy must fail closed;
/// an allowed HTTP host does not make WebSockets available through a restricted proxy policy.
#[tokio::test]
async fn restricted_websocket_requires_open_network_even_for_allowed_host() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        global(&h, "PORT", server.local_addr().unwrap().port());
        let worker = tokio::spawn(async move {
            let (mut http, _) = tokio::time::timeout(Duration::from_secs(30), server.accept()).await.unwrap().unwrap();
            let mut bytes = [0; 4096];
            assert!(http.read(&mut bytes).await.unwrap() > 0, "HTTP request was empty");
            http.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            server
        });
        let (status, ok, err, count, kind, host, blocked_ok, blocked_err, blocked_count): (
            u16,
            bool,
            String,
            usize,
            String,
            String,
            bool,
            String,
            usize,
        ) = eval(
            &h,
            r#"
            local function restricted()
                return {cwd = CWD, sandbox = {level = "read-only",
                    network = {allow = {"127.0.0.1"}, allow_local = true}}}
            end
            local ctx = restricted()
            local status, ok, err = mare.exec.run(ctx, function()
                local response = mare.http.request({url = "http://127.0.0.1:" .. PORT .. "/", timeout_ms = 10000})
                local ok, err = pcall(mare.http.websocket, {url = "ws://127.0.0.1:" .. PORT .. "/", timeout_ms = 10000})
                return response.status, ok, tostring(err)
            end)
            local evidence = (ctx.denials or {})[1] or {}
            local blocked = restricted()
            local blocked_ok, blocked_err = mare.exec.run(blocked, function()
                local ok, err = pcall(mare.http.websocket, {url = "ws://blocked.example.invalid/", timeout_ms = 10000})
                return ok, tostring(err)
            end)
            return status, ok, err, #(ctx.denials or {}), tostring(evidence.sandbox_denied or ""),
                tostring(evidence.host or ""), blocked_ok, blocked_err, #(blocked.denials or {})
            "#,
        )
        .await;
        let server = worker.await.unwrap();
        assert_eq!(status, 200, "the allowed HTTP request must reach the server");
        for (label, ok, err) in [("allowed", ok, err), ("blocked", blocked_ok, blocked_err)] {
            assert!(
                !ok && err.contains("network access") && err.contains("denied") && err.contains("open network"),
                "{label}: {err}"
            );
        }
        assert_eq!((count, kind.as_str(), host.as_str()), (1, "network", "127.0.0.1"));
        assert_eq!(blocked_count, 1, "a refused WebSocket must record exactly one denial");
        assert!(tokio::time::timeout(Duration::from_millis(50), server.accept()).await.is_err(), "WebSocket connected");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn network_in_a_denying_context_is_refused() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        setup(&env, &h).await;
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        global(&h, "PORT", server.local_addr().unwrap().port());
        let worker = tokio::spawn(async move {
            let (mut http, _) = tokio::time::timeout(Duration::from_secs(30), server.accept()).await.unwrap().unwrap();
            let mut bytes = [0; 4096];
            let _ = http.read(&mut bytes).await.unwrap();
            http.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            let (socket, _) = tokio::time::timeout(Duration::from_secs(30), server.accept()).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::accept_async(socket))
                .await
                .unwrap()
                .unwrap();
            server
        });
        exec(
            &h,
            r#"
            assert(mare.http.request({url = "http://127.0.0.1:" .. PORT .. "/", timeout_ms = 10000}).status == 200)
            assert(mare.http.websocket({url = "ws://127.0.0.1:" .. PORT .. "/", timeout_ms = 5000}))
        "#,
        )
        .await;
        let server = worker.await.unwrap();
        exec(
            &h,
            r#"
            mare.exec.run(DENY, function()
                for name, f in pairs({
                    request = function() return mare.http.request({url = "http://127.0.0.1:" .. PORT .. "/", timeout_ms = 10000}) end,
                    websocket = function() return mare.http.websocket({url = "ws://127.0.0.1:" .. PORT .. "/", timeout_ms = 10000}) end,
                }) do
                    local ok, err = pcall(f)
                    assert(not ok and tostring(err):find("network access", 1, true)
                        and tostring(err):find("denied", 1, true), name .. ": " .. tostring(err))
                end
            end)
        "#,
        )
        .await;
        assert!(tokio::time::timeout(Duration::from_millis(50), server.accept()).await.is_err());

        #[cfg(unix)]
        {
            use std::os::unix::net::UnixListener;
            use tokio::io::{AsyncBufReadExt, BufReader};
            let socket = env.cwd.join("socket");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = tokio::net::UnixListener::from_std(listener.try_clone().unwrap()).unwrap();
            global(&h, "SOCKET", socket.to_string_lossy().to_string());
            let worker = tokio::spawn(async move {
                let (_connect, _) =
                    tokio::time::timeout(Duration::from_secs(5), server.accept()).await.unwrap().unwrap();
                let (request, _) =
                    tokio::time::timeout(Duration::from_secs(5), server.accept()).await.unwrap().unwrap();
                let mut request = BufReader::new(request);
                let mut line = String::new();
                request.read_line(&mut line).await.unwrap();
                assert_eq!(line, "ping\n");
                request.get_mut().write_all(b"pong\n").await.unwrap();
            });
            exec(
                &h,
                r#"
                local conn = assert(mare.net.unix.connect(SOCKET)); conn:close()
                assert(mare.net.unix.request(SOCKET, "ping") == "pong")
            "#,
            )
            .await;
            worker.await.unwrap();
            exec(
                &h,
                r#"
                mare.exec.run(DENY, function()
                    for name, f in pairs({
                        connect = function() return mare.net.unix.connect(SOCKET) end,
                        request = function() return mare.net.unix.request(SOCKET, "ping") end,
                    }) do
                        local value, err = f()
                        assert(value == nil and tostring(err):find("network access", 1, true)
                            and tostring(err):find("denied", 1, true), name .. ": " .. tostring(err))
                    end
                end)
            "#,
            )
            .await;
            assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        }
        h.shutdown().await;
    })
    .await
}
