//! Sandbox policy checks, escalation, tool-context admission, and path resolution.

use super::host;
use crate::common::*;
use crate::suite::sandbox_expect::require_backend;

#[tokio::test]
async fn path_checks_share_the_spawn_policy_rules() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (w_in, w_out, why, net_ok, net_bad): (bool, bool, String, bool, bool) = eval(
            &h,
            r#"local S = mare.proc.sandbox
               local pol = { level = "workspace-write", network = { allow = { "*.crates.io" } } }
               local a = S.check("write", "sub/new.txt", pol)
               local b, why = S.check("write", "/etc/mare-x", pol)
               return a, b, why, S.check("network", "static.crates.io", pol), S.check("network", "evil.com", pol)"#,
        )
        .await;
        assert!(w_in && !w_out && why.contains("outside the writable roots"), "{why}");
        assert!(net_ok && !net_bad);
        let bare: String = eval(
            &h,
            r#"local ok, e = pcall(mare.proc.sandbox.policy, { network = { allow = { "*" } } }) return tostring(e)"#,
        )
        .await;
        assert!(bare.contains("bare `*`"), "{bare}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn set_current_round_trips_and_validates_extra_roots() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (removed, level, marker, bad_root): (bool, String, String, String) = eval(
            &h,
            r#"local S = mare.proc.sandbox
               local cur = S.set_current({ level = "workspace-write", network = false })
               local again = S.set_current(S.current())          -- round trip
               assert(again.level == cur.level and #again.roots == #cur.roots)
               local level = S.current().level
               S.set_current(nil)
               local ok, e = pcall(S.set_current, { writable = { "/definitely/missing/dir" } })
               return S.escalate == nil, level, cur.marker, tostring(e)"#,
        )
        .await;
        assert!(removed, "mare.proc.sandbox.escalate must be removed");
        assert_eq!(level, "workspace-write");
        assert_eq!(marker, "[sandbox: file access denied under workspace-write mode]");
        assert!(bad_root.contains("SANDBOX_INVALID_ROOT"), "{bad_root}");
        h.shutdown().await;
    })
    .await
}

/// A tool must not widen its admitted policy through the current or original context.
#[tokio::test]
async fn tool_context_policy_cannot_be_widened_after_admission() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (current, original, inherited, setter, denials, view_denials): (bool, bool, bool, bool, i64, i64) = eval(
            &h,
            r#"local function run(change, child)
                 local ctx = { cwd = CWD, sandbox = { level = "read-only", network = false } }
                 local ok, set = mare.exec.run(ctx, function()
                     local view = mare.exec.current()
                     local did_set = pcall(mare.proc.sandbox.set_current, "danger-full-access")
                     change(ctx, view)
                     local function attempt() return pcall(mare.fs.write, CWD .. "/escape", "x") end
                     if child then return coroutine.wrap(attempt)(), did_set end
                     return attempt(), did_set
                 end)
                 return ok, set, #(ctx.denials or {})
               end
               local a, setter, da = run(function(_, view) pcall(function() view.sandbox = "danger-full-access" end) end)
               local b, _, db = run(function(ctx) ctx.sandbox.level = "danger-full-access" end)
               local c, _, dc = run(function(ctx) ctx.sandbox.level = "danger-full-access" end, true)
               local visible = mare.exec.run({ cwd = CWD, sandbox = "read-only" }, function()
                   pcall(mare.fs.write, CWD .. "/escape", "x")
                   return #(mare.exec.current().denials or {})
               end)
               return a, b, c, setter, da + db + dc, visible"#,
        ).await;
        assert!(!current && !original && !inherited, "context mutations must never allow a write");
        assert!(!setter, "a tool cannot change the session policy");
        assert_eq!(denials, 3, "the pipeline receives each refusal");
        assert_eq!(view_denials, 1, "the read-only view reports refusals during the call");
        assert!(!env.cwd.join("escape").exists());
        h.shutdown().await;
    }).await
}

/// Raw Seatbelt text cannot replace or silently bypass an admitted tool policy.
#[tokio::test]
async fn a_tool_cannot_supply_an_opaque_seatbelt_profile() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let (restricted, unconfined, narrowed, intersect): (String, String, String, String) = eval(
            &h,
            r#"local function try(base)
                 local ok, err = mare.exec.run({ sandbox = base }, function()
                     return pcall(mare.proc.run, { cmd = "true", sandbox = {
                         profile = "(version 1)(allow default)" } })
                 end)
                 return ok and "ran" or tostring(err)
               end
               local ok, err = mare.exec.run({ sandbox = { profile = "(version 1)(allow default)" } }, function()
                   return pcall(mare.proc.run, { cmd = "true", sandbox = "read-only" })
               end)
               local iok, ierr = pcall(mare.proc.sandbox.intersect, "read-only", { profile = "(version 1)(allow default)" })
               return try("read-only"), try("danger-full-access"), ok and "ran" or tostring(err),
                      iok and "accepted" or tostring(ierr)"#,
        )
        .await;
        assert!(restricted.contains("profile") && restricted.contains("tool"), "{restricted}");
        assert!(unconfined.contains("profile") && unconfined.contains("tool"), "{unconfined}");
        assert!(narrowed.contains("profile") && narrowed.contains("narrow"), "{narrowed}");
        assert!(intersect.contains("opaque") && intersect.contains("intersect"), "{intersect}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_tools_own_policy_combines_per_dimension_with_the_context() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(
            &h,
            "a_tools_own_policy_combines_per_dimension_with_the_context",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
        {
            return h.shutdown().await;
        }
        let (level, network, disjoint): (String, String, String) = eval(
            &h,
            r#"-- context: workspace writes, network blocked; the tool asks for read-only + open network
               local ctx = { sandbox = { level = "workspace-write", network = false } }
               local r = mare.exec.run(ctx, function()
                   return mare.proc.run({ cmd = "true", sandbox = { level = "read-only", network = true } })
               end)
               local ctx2 = { sandbox = { level = "workspace-write", network = { allow = { "a.example" } } } }
               local r2 = mare.exec.run(ctx2, function()
                   return mare.proc.run({ cmd = "true", sandbox = { level = "workspace-write",
                       network = { allow = { "b.example" } } } })
               end)
               return r.sandbox.level, r.sandbox.network, r2.sandbox.network"#,
        )
        .await;
        assert_eq!(
            (level.as_str(), network.as_str()),
            ("read-only", "blocked"),
            "each dimension takes the stricter side"
        );
        // disjoint host lists: nothing both allow — the network ends up blocked, not refused
        assert_eq!(disjoint, "blocked");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn dangling_symlinks_count_as_outside_the_sandbox() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let out_dir = dunce::canonicalize(outside.path()).unwrap();
        let target = out_dir.join("created-through-link");
        std::os::unix::fs::symlink(&target, env.cwd.join("link")).unwrap();
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        global(&h, "OUT", out_dir.to_string_lossy().to_string());
        let (direct, checked, wrote): (bool, bool, String) = eval(
            &h,
            r#"local pol = { level = "workspace-write", writable = { OUT }, exclude_tmpdir_env_var = true, exclude_slash_tmp = true }
               local d = mare.proc.sandbox.check("write", OUT .. "/direct", pol)
               local c = mare.proc.sandbox.check("write", CWD .. "/link", pol)
               local _, err = mare.exec.run({ cwd = CWD, sandbox = pol }, function()
                   return pcall(mare.fs.write, CWD .. "/link", "x")
               end)
               return d, c, tostring(err)"#,
        )
        .await;
        assert!(direct, "the explicit target parent directory is writable");
        assert!(!checked, "a dangling link inside the workspace is not writable");
        assert!(wrote.contains("denied"), "{wrote}");
        assert!(!target.exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn paths_resolve_like_the_kernel_and_protected_names_hold() {
    // peer batch five items 1, 2, 7, 8: `link/..` follows the link first, a
    // directory swapped for a link between calls is re-resolved, missing .mare is protected,
    // missing .git is not, existing metadata and pointer files are protected, and read-only grants no temp
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let out = dunce::canonicalize(outside.path()).unwrap();
        std::fs::create_dir(out.join("sub")).unwrap();
        std::os::unix::fs::symlink(out.join("sub"), env.cwd.join("link")).unwrap();
        std::fs::create_dir(env.cwd.join("d")).unwrap();
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        global(&h, "OUT", out.to_string_lossy().to_string());
        let r: String = eval(
            &h,
            r#"local pol = { level = "workspace-write", exclude_tmpdir_env_var = true, exclude_slash_tmp = true }
               local function w(p)
                   local ok, err = mare.exec.run({ cwd = CWD, sandbox = pol }, function()
                       return pcall(mare.fs.write, p, "x")
                   end)
                   return ok and "ok" or (tostring(err):find("denied") and "denied" or tostring(err))
               end
               local chk = function(p, po) return tostring(mare.proc.sandbox.check("write", p, po or pol)) end
               local first = w(CWD .. "/d/f.txt")
               os.rename(CWD .. "/d", CWD .. "/d.old")
               mare.proc.run({ cmd = "ln", args = { "-s", OUT, CWD .. "/d" } })
               return table.concat({
                   w(CWD .. "/link/../escape.txt"), first, w(CWD .. "/d/f.txt"),
                   chk(CWD .. "/.mare/config.lua"), chk(CWD .. "/.git/config"),
                   chk((os.getenv("TMPDIR") or "/tmp") .. "/x", { level = "read-only" }),
               }, ",")"#,
        )
        .await;
        assert_eq!(r, "denied,ok,denied,false,true,false", "{r}");
        std::fs::create_dir(env.cwd.join(".mare")).unwrap();
        let existing_mare: bool = eval(
            &h,
            r#"return mare.proc.sandbox.check("write", CWD .. "/.mare/config.lua", { level = "workspace-write" })"#,
        )
        .await;
        assert!(!existing_mare, "an existing .mare remains protected");
        assert!(!out.join("escape.txt").exists() && !out.join("f.txt").exists());
        // a `.git` pointer file (worktree / submodule) and, where the file system folds case, `.GIT`
        std::fs::write(env.cwd.join(".git"), "gitdir: /elsewhere\n").unwrap();
        let (ptr, upper): (bool, bool) = eval(
            &h,
            r#"local pol = { level = "workspace-write", exclude_tmpdir_env_var = true, exclude_slash_tmp = true }
               return mare.proc.sandbox.check("write", CWD .. "/.git", pol),
                   mare.proc.sandbox.check("write", CWD .. "/.GIT/hooks/pre-commit", pol)"#,
        )
        .await;
        assert!(!ptr, ".git pointer file is protected");
        if cfg!(target_os = "macos") {
            assert!(!upper, ".GIT on a case-insensitive volume is .git");
        }
        h.shutdown().await;
    })
    .await
}
