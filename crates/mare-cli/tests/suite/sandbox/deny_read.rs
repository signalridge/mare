//! `deny_read` enforcement and tool-context isolation for store, locks, spills, and sockets.

use super::host;
#[cfg(target_os = "linux")]
use super::mechanism;
use crate::common::*;
use crate::suite::sandbox_expect::require_backend;

#[tokio::test]
#[cfg(unix)]
async fn unix_socket_checks_the_process_cwd_it_connects_to() {
    local(async {
        let env = TestEnv::new().await;
        let process_cwd = std::env::current_dir().unwrap();
        let dir = tempfile::Builder::new().prefix("mare-s8-sock-").tempdir_in(&process_cwd).unwrap();
        let socket = dir.path().join("listener.sock");
        let rel = socket.strip_prefix(&process_cwd).unwrap().to_string_lossy().to_string();
        // Bind the same process-cwd-relative path passed to the tool; an absolute
        // checkout path can exceed the Unix socket address length limit.
        let _listener = std::os::unix::net::UnixListener::bind(&rel).unwrap();
        let h = host(&env).await;
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        global(&h, "REL", rel);
        let (connect_denied, request_denied): (bool, bool) = eval(
            &h,
            r#"
            return mare.exec.run({ cwd = CWD, sandbox = {
                level = "read-only", network = false, unix_sockets = { CWD .. "/" .. REL }
            } }, function()
                local a, ae = mare.net.unix.connect(REL, {timeout_ms = 30})
                local b, be = mare.net.unix.request(REL, "test", {timeout_ms = 30})
                return a == nil and tostring(ae):find("sandbox", 1, true) ~= nil,
                       b == nil and tostring(be):find("sandbox", 1, true) ~= nil
            end)
        "#,
        )
        .await;
        assert!(connect_denied && request_denied);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
#[cfg(unix)]
async fn deny_read_store_uses_the_process_cwd_it_opens() {
    local(async {
        let env = TestEnv::new().await;
        let process_cwd = std::env::current_dir().unwrap();
        let dir = tempfile::Builder::new().prefix("mare-s8-store-").tempdir_in(&process_cwd).unwrap();
        let secret = dir.path().join("secret.jsonl");
        let allowed = dir.path().join("allowed.jsonl");
        let line = "{\"type\":\"message\",\"message\":{\"content\":\"needle\"}}\n";
        std::fs::write(&secret, line).unwrap();
        std::fs::write(&allowed, line).unwrap();
        let rel = secret.strip_prefix(&process_cwd).unwrap().to_string_lossy().to_string();
        let rel_allowed = allowed.strip_prefix(&process_cwd).unwrap().to_string_lossy().to_string();
        let h = host(&env).await;
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        global(&h, "REL", rel);
        global(&h, "ALLOWED", rel_allowed);
        let (failures, sibling, trusted): (String, bool, bool) = eval(
            &h,
            r#"
            local bad, sibling = mare.exec.run({ cwd = CWD, sandbox = {
                level = "read-only", network = true, deny_read = { SECRET }, accept_partial = { "fs_deny_read" }
            } }, function()
                local bad = {}
                local probes = {
                    read = function() return mare.store.read(REL) end,
                    compaction = function() return mare.store.read_from_compaction(REL) end,
                    tail = function() return mare.store.read_tail(REL, 1) end,
                    contains = function() return mare.store.file_contains(REL, "needle") end,
                    search = function() return mare.store.search(REL, "needle") end,
                }
                for name, f in pairs(probes) do
                    local ok, e = pcall(f)
                    if ok or not tostring(e):find("sandbox", 1, true) then bad[#bad + 1] = name end
                end
                local unavailable = table.concat(mare.exec.current().sandbox.unavailable, ",")
                return table.concat(bad, ","), mare.store.file_contains(ALLOWED, "needle") and unavailable:find("fs_deny_read", 1, true) ~= nil
            end)
            return bad, sibling, mare.store.file_contains(REL, "needle")
        "#,
        )
        .await;
        assert_eq!(failures, "");
        assert!(sibling && trusted);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
#[cfg(unix)]
async fn strict_deny_rejects_tool_before_hardlink_read() {
    local(async {
        let env = TestEnv::new().await;
        let secret = env.cwd.join("secret");
        let alias = env.cwd.join("alias");
        std::fs::write(&secret, "sensitive").unwrap();
        std::fs::hard_link(&secret, &alias).unwrap();
        let h = host(&env).await;
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        global(&h, "ALIAS", alias.to_string_lossy().to_string());
        let (ok, error, ran): (bool, String, bool) = eval(&h, r#"
            RAN = false
            local ok, e = pcall(mare.exec.run, { sandbox = { level = "read-only", network = true, deny_read = { SECRET } } }, function()
                RAN = true
                return mare.fs.read(ALIAS)
            end)
            return ok, tostring(e), RAN
        "#).await;
        assert!(!ok && !ran && error.contains("fs_deny_read"), "{error}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
#[cfg(unix)]
async fn deny_read_guards_state_workspace_locks_and_spills() {
    local(async {
        let env = TestEnv::new().await;
        let private = env.cwd.join("private");
        std::fs::create_dir(&private).unwrap();
        let secret = private.join("secret");
        std::fs::write(&secret, "keep me").unwrap();
        let h = host(&env).await;
        let state_path: String = eval(&h, "return mare.state.path").await;
        let spill_dir = std::path::Path::new(&state_path).parent().unwrap().join("spill");
        std::fs::create_dir_all(&spill_dir).unwrap();
        let spill_alias = spill_dir.join(format!(
            "bash-s8-{}-{}.log",
            std::process::id(),
            env.cwd.file_name().unwrap().to_string_lossy()
        ));
        std::os::unix::fs::symlink(&secret, &spill_alias).unwrap();
        global(&h, "PRIVATE", private.to_string_lossy().to_string());
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        global(&h, "SPILL_ALIAS", spill_alias.to_string_lossy().to_string());
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (bad, strict_run, strict_job, trusted): (String, bool, bool, String) = eval(
            &h,
            r#"
            mare.state.set("s8_probe", "retained")
            local state = mare.state.path
            local strict = { level = "read-only", network = true, deny_read = { SECRET } }
            local strict_run = pcall(mare.proc.run, { cmd = "true", sandbox = strict, spill = SECRET })
            local strict_job = pcall(mare.proc.start, { cmd = "true", sandbox = strict, spill = SECRET })
            local bad = mare.exec.run({ cwd = CWD, sandbox = {
                level = "workspace-write", network = true, deny_read = { PRIVATE, state },
                accept_partial = { "fs_deny_read", "fs_protect" }
            } }, function()
                local failures = {}
                local probes = {
                    state_read = function() return mare.state.read() end,
                    state_set = function() return mare.state.set("s8_probe", "changed") end,
                    state_update = function() return mare.state.update(function(s) s.s8_probe = "changed" end) end,
                    lock = function() return mare.lock.file(SECRET) end,
                    workspace_add = function() return mare.workspace.add(PRIVATE) end,
                    workspace_contains = function() return mare.workspace.contains(PRIVATE) end,
                    peers = function() return mare.peers.list() end,
                    crash = function() return mare.crash.begin("probe") end,
                    mcp_credentials = function() return mare.mcp.has_credentials("x", "https://example.com") end,
                    mcp_spill = function() return mare.mcp.spill("data") end,
                    mcp_log = function() return mare.mcp.log("x", "info", "data") end,
                    mcp_connect = function() return mare.mcp.connect({ name = "x", command = "true" }) end,
                    proc_spill = function() return mare.proc.run({cmd = "true", spill = SECRET}) end,
                    job_spill = function() return mare.proc.start({cmd = "true", spill = SECRET}) end,
                    spill_write = function() return mare.proc.spill_write("bash", SPILL_ALIAS, "changed") end,
                    spill_remove = function() return mare.proc.spill_remove("bash", SPILL_ALIAS) end,
                    spill_alias = function() return mare.proc.run({cmd = "true", spill = SPILL_ALIAS, spill_prefix = "bash"}) end,
                    unix_listen = function()
                        local server = mare.net.unix.listen(SECRET, function() end)
                        if not server then error("refused") end
                    end,
                }
                for name, f in pairs(probes) do if pcall(f) then failures[#failures + 1] = name end end
                local g = mare.lock.file(CWD .. "/allowed.lock"); g:release()
                return table.concat(failures, ",")
            end)
            return bad, strict_run, strict_job, mare.state.read().s8_probe
        "#,
        )
        .await;
        assert_eq!(bad, "");
        assert!(!strict_run && !strict_job);
        assert_eq!(trusted, "retained");
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "keep me");
        assert!(spill_alias.symlink_metadata().unwrap().file_type().is_symlink());
        std::fs::remove_file(spill_alias).unwrap();
        h.shutdown().await;
    })
    .await
}

/// Outside any tool context, a command's own (partially enforced) deny_read still covers the
/// spill file mare writes for it: run, piped start and tty start refuse before touching it.
#[tokio::test]
#[cfg(unix)]
async fn call_sandbox_deny_read_refuses_spill_without_exec_context() {
    local(async {
        let env = TestEnv::new().await;
        let secret = env.cwd.join("secret");
        std::fs::write(&secret, "keep me").unwrap();
        let h = host(&env).await;
        if !require_backend(&h, "call_sandbox_deny_read_refuses_spill_without_exec_context", mare_binding::ossandbox::expect::ANY).await { return h.shutdown().await; }
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        global(&h, "ALLOWED", env.cwd.join("allowed.log").to_string_lossy().to_string());
        let (errors, allowed): (String, String) = eval(
            &h,
            r#"
            local sandbox = { level = "read-only", network = true, deny_read = { SECRET }, accept_partial = { "fs_deny_read" } }
            local function unchanged()
                local f = assert(io.open(SECRET, "rb")); local data = f:read("a"); f:close()
                return data == "keep me"
            end
            local probes = {
                { "run", function(spill) return mare.proc.run({ cmd = "printf", args = { "ok" }, sandbox = sandbox, spill = spill }) end },
                { "start", function(spill) return mare.proc.start({ cmd = "printf", args = { "ok" }, sandbox = sandbox, spill = spill }) end },
                { "tty", function(spill) return mare.proc.start({ cmd = "printf", args = { "ok" }, tty = true, sandbox = sandbox, spill = spill }) end },
            }
            local errors = {}
            for _, probe in ipairs(probes) do
                local ok, e = pcall(probe[2], SECRET)
                local refused = not ok and tostring(e):find("sandbox: spill path intersects a protected or deny_read path", 1, true) ~= nil
                errors[#errors + 1] = probe[1] .. "=" .. tostring(refused and unchanged())
            end
            local r = mare.proc.run({ cmd = "printf", args = { "ok" }, sandbox = sandbox, spill = ALLOWED })
            local f = assert(io.open(ALLOWED, "rb")); local spilled = f:read("a"); f:close()
            return table.concat(errors, ","), r.code .. ":" .. r.sandbox.enforcement .. ":" .. spilled
        "#,
        )
        .await;
        assert_eq!(errors, "run=true,start=true,tty=true");
        assert_eq!(allowed, "0:partial:ok");
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "keep me");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn explicit_deny_read_is_refused_or_partial_for_process_and_pty() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("secret"), "secret-data").unwrap();
        std::fs::write(env.cwd.join("sibling"), "visible").unwrap();
        let h = host(&env).await;
        let mech = mechanism(&h).await;
        assert_eq!(
            mech, "bwrap",
            "Linux explicit deny_read test requires bwrap backend (got {mech})"
        );
        let (proc_refused, pty_refused, code, enforcement, output, terminal): (String, String, i64, String, String, String) = eval(
            &h,
            r#"local strict = { level = "read-only", network = true, deny_read = { "secret" } }
               local _, pe = pcall(mare.proc.run, { cmd = "true", sandbox = strict })
               local _, te = pcall(mare.pty.spawn, { cmd = "true", sandbox = strict })
               local partial = { level = "read-only", network = true, deny_read = { "secret" }, accept_partial = { "fs_deny_read" } }
               local script = "test ! -s secret && test \"$(cat sibling)\" = visible && echo MASKED"
               local r = mare.proc.run({ cmd = "sh", args = { "-c", script }, sandbox = partial })
               local p = mare.pty.spawn({ cmd = "sh", args = { "-c", script }, sandbox = partial })
               local seen = ""
               while true do local chunk = p:read(); if not chunk then break end; seen = seen .. chunk end
               return tostring(pe), tostring(te), r.code, r.sandbox.enforcement, r.stdout, seen"#,
        ).await;
        assert!(proc_refused.contains("fs_deny_read") && pty_refused.contains("fs_deny_read"));
        assert_eq!(code, 0, "{output}");
        assert_eq!(enforcement, "partial");
        assert!(output.contains("MASKED") && terminal.contains("MASKED"), "{output:?} {terminal:?}");
        h.shutdown().await;
    }).await
}

/// Store primitives and pre-opened journal handles must respect the caller's tool context.
#[tokio::test]
async fn store_cannot_bypass_the_tool_execution_context() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let path = outside.path().join("session.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"session_id\":\"test\"}\n").unwrap();
        global(&h, "OUTSIDE", path.to_string_lossy().to_string());
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (delete, append, close, read, search, handle_leaks): (bool, bool, bool, bool, bool, String) = eval(
            &h,
            r#"local journal = mare.store.open(OUTSIDE, { existing = true, auxiliary = true })
               local result = table.pack(mare.exec.run({ cwd = CWD, sandbox = {
                   level = "read-only", restrict_read = true, readable = { CWD }, network = false,
               } }, function()
                   local leaks = {}
                   local probes = {
                       append_batch = function() return journal:append_batch({}) end,
                       flush = function() return journal:flush() end,
                       activate = function() return journal:activate() end,
                       checkpoint = function() return journal:checkpoint() end,
                       path = function() return journal:path() end,
                       stats = function() return journal:stats() end,
                   }
                   for name, f in pairs(probes) do if pcall(f) then leaks[#leaks + 1] = name end end
                   local search = pcall(mare.store.search, OUTSIDE, "session")
                   return pcall(mare.store.delete, OUTSIDE),
                          pcall(function() journal:append({ type = "message" }) end),
                          pcall(function() journal:close() end),
                          pcall(mare.store.read, OUTSIDE),
                          search, table.concat(leaks, ",")
               end))
               journal:close()
               return table.unpack(result, 1, 6)"#,
        )
        .await;
        assert!(!delete && !append && !close && !read && !search, "store operations escaped tool restrictions");
        assert!(handle_leaks.is_empty(), "unrestricted handle methods: {handle_leaks}");
        std::fs::write(
            env.cwd.join("allowed.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"session_id\":\"allowed\"}\n",
        )
        .unwrap();
        let (leaks, allowed): (String, bool) = eval(
            &h,
            r#"return mare.exec.run({ cwd = CWD, sandbox = {
            level = "read-only", restrict_read = true, readable = { CWD }, network = false,
        } }, function()
            local bad = {}
            local probes = {
                open = function() return mare.store.open(OUTSIDE) end,
                files = function() return mare.store.files(OUTSIDE) end,
                list = function() return mare.store.list(CWD) end,
                read_tail = function() return mare.store.read_tail(OUTSIDE, 1) end,
                read_from_compaction = function() return mare.store.read_from_compaction(OUTSIDE) end,
                file_contains = function() return mare.store.file_contains(OUTSIDE, "test") end,
                index_append = function() return mare.store.index_append({}) end,
                index_read = function() return mare.store.index_read() end,
                index_reconcile = function() return mare.store.index_reconcile() end,
                index_notice = function() return mare.store.index_notice() end,
                index_rebuild = function() return mare.store.index_rebuild() end,
            }
            for name, f in pairs(probes) do if pcall(f) then bad[#bad + 1] = name end end
            return table.concat(bad, ","), pcall(mare.store.read, CWD .. "/allowed.jsonl")
        end)"#,
        )
        .await;
        assert!(leaks.is_empty(), "unrestricted store operations: {leaks}");
        assert!(allowed, "reading an allowed session remains possible");
        assert!(path.exists(), "the session must remain on disk");
        h.shutdown().await;
    })
    .await
}
