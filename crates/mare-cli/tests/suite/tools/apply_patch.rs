//! Built-in `apply_patch` tool tests: preflight, ordered commit, chaining, and sandbox.

use super::{call, host, text};
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn apply_patch_respects_the_read_guard() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("p.txt"), "alpha\nbeta\n").unwrap();
        let h = host(&env).await;
        let patch = "*** Begin Patch\n*** Update File: p.txt\n@@\n alpha\n-beta\n+gamma\n*** End Patch";
        let refused = call(&h, "apply_patch", json!({ "input": patch })).await;
        assert_eq!(refused["is_error"], true, "{refused}");
        assert!(text(&refused).contains("not_observed"), "{refused}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("p.txt")).unwrap(), "alpha\nbeta\n");
        call(&h, "read", json!({ "path": "p.txt" })).await;
        let ok = call(&h, "apply_patch", json!({ "input": patch })).await;
        assert!(ok["is_error"] != true, "{ok}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("p.txt")).unwrap(), "alpha\ngamma\n");
        h.shutdown().await;
    })
    .await
}

// --- apply_patch: preflight, ordered commit, per-file outcomes ---

#[tokio::test]
async fn apply_patch_preflight_collects_every_problem_and_writes_nothing() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "one\n").unwrap();
        std::fs::write(env.cwd.join("b.txt"), "two\n").unwrap();
        std::fs::write(env.cwd.join("c.txt"), "three\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "a.txt" })).await;
        // b.txt was never read and c.txt changed on disk after its read: two refusals, and the
        // first (valid) file must not be written either
        call(&h, "read", json!({ "path": "c.txt" })).await;
        std::fs::write(env.cwd.join("c.txt"), "three!\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: b.txt\n@@\n-two\n+TWO\n*** Update File: c.txt\n@@\n-three!\n+THREE\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": patch })).await;
        assert_eq!(r["is_error"], true, "{r}");
        let t = text(&r);
        assert!(t.contains("rejected before any change (2 problems)"), "{t}");
        assert!(t.contains("b.txt") && t.contains("c.txt") && t.contains("No files were modified"), "{t}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("a.txt")).unwrap(), "one\n");
        // a hunk that doesn't match is refused the same way
        let bad = "*** Begin Patch\n*** Update File: a.txt\n@@\n-nope\n+X\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": bad })).await;
        assert!(text(&r).contains("rejected before any change"), "{r}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn apply_patch_stops_at_the_first_failed_file_without_rollback() {
    use std::os::unix::fs::PermissionsExt;
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "one\n").unwrap();
        std::fs::create_dir(env.cwd.join("sub")).unwrap();
        std::fs::write(env.cwd.join("sub/b.txt"), "two\n").unwrap();
        std::fs::write(env.cwd.join("c.txt"), "three\n").unwrap();
        let h = host(&env).await;
        for f in ["a.txt", "sub/b.txt", "c.txt"] {
            call(&h, "read", json!({ "path": f })).await;
        }
        // a real write failure on the second file: its directory turns read-only after preflight
        let sub = env.cwd.join("sub");
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(sub.join("probe"), "").is_ok() {
            // running as root: permissions don't stop writes
            std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
            h.shutdown().await;
            return;
        }
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: sub/b.txt\n@@\n-two\n+TWO\n*** Update File: c.txt\n@@\n-three\n+THREE\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": patch })).await;
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r["is_error"], true, "{r}");
        assert!(text(&r).contains("Patch stopped at file 2 of 3"), "{r}");
        let statuses: Vec<&str> =
            r["details"]["outcomes"].as_array().unwrap().iter().map(|o| o["status"].as_str().unwrap()).collect();
        assert_eq!(statuses, ["completed", "failed", "skipped"], "{r}");
        assert_eq!(r["details"]["exact"], true, "a clean failure leaves the effect known");
        // no rollback: the first file stays written, the rest untouched
        assert_eq!(std::fs::read_to_string(env.cwd.join("a.txt")).unwrap(), "ONE\n");
        assert_eq!(std::fs::read_to_string(sub.join("b.txt")).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(env.cwd.join("c.txt")).unwrap(), "three\n");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn edits_that_change_nothing_are_errors() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("n.txt"), "same\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "n.txt" })).await;
        let patch = "*** Begin Patch\n*** Update File: n.txt\n@@\n-same\n+same\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": patch })).await;
        assert_eq!(r["is_error"], true, "{r}");
        assert!(text(&r).contains("changes nothing"), "{r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn apply_patch_write_error_that_changed_the_file_is_unknown_and_inexact() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "one\n").unwrap();
        std::fs::write(env.cwd.join("u.txt"), "unchanged line\n").unwrap();
        std::fs::write(env.cwd.join("c.txt"), "three\n").unwrap();
        // the write of u.txt changes the file, then errors (reference implementation: I/O error + the file
        // no longer matches its pre-write version → unknown)
        env.set_var("MARE_TEST_FS_FAULT", "write-then-eio:u.txt");
        let h = host(&env).await;
        for f in ["a.txt", "u.txt", "c.txt"] {
            call(&h, "read", json!({ "path": f })).await;
        }
        exec(&h, r#"INEXACT = nil; C.cordis:on("fs/changes-inexact", function(ev) INEXACT = ev end)"#).await;
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: u.txt\n@@\n-unchanged line\n+a much longer replacement line\n*** Update File: c.txt\n@@\n-three\n+THREE\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": patch })).await;
        assert_eq!(r["is_error"], true, "{r}");
        let statuses: Vec<&str> =
            r["details"]["outcomes"].as_array().unwrap().iter().map(|o| o["status"].as_str().unwrap()).collect();
        assert_eq!(statuses, ["completed", "unknown", "skipped"], "{r}");
        assert_eq!(r["details"]["exact"], false, "{r}");
        assert!(text(&r).contains("read it again before retrying"), "{r}");
        let ev: String = eval(&h, r#"return INEXACT and (INEXACT.tool .. " " .. table.concat(INEXACT.paths, ",")) or "none""#).await;
        assert!(ev.starts_with("apply_patch ") && ev.ends_with("u.txt") && !ev.contains(','), "{ev}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("c.txt")).unwrap(), "three\n", "skipped stays untouched");
        h.shutdown().await;
    })
    .await
}

/// sections of one file chain (each sees the previous section's result); a move keeps the
/// mode; `a` → `./a` keeps the file; a move onto a hard link of itself removes only the source;
/// Add File onto an unread existing file is refused
#[cfg(unix)]
#[tokio::test]
async fn apply_patch_chains_one_files_sections_and_moves_safely() {
    use std::os::unix::fs::PermissionsExt;
    local(async {
        let env = TestEnv::new().await;
        let p = |n: &str| env.cwd.join(n);
        std::fs::write(p("f.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(p("run.sh"), "echo hi\n").unwrap();
        std::fs::set_permissions(p("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(p("same.txt"), "x\n").unwrap();
        std::fs::write(p("h1.txt"), "old\n").unwrap();
        std::fs::hard_link(p("h1.txt"), p("h2.txt")).unwrap();
        std::fs::write(p("exists.txt"), "keep\n").unwrap();
        let h = host(&env).await;
        for f in ["f.txt", "run.sh", "same.txt", "h1.txt"] {
            call(&h, "read", json!({ "path": f })).await;
        }
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n+A\n*** Update File: f.txt\n@@\n-c\n+C\n\
                     *** Update File: run.sh\n*** Move to: bin/run.sh\n@@\n-echo hi\n+echo moved\n\
                     *** Update File: same.txt\n*** Move to: ./same.txt\n@@\n-x\n+y\n\
                     *** Update File: h1.txt\n*** Move to: h2.txt\n@@\n-old\n+new\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": patch })).await;
        assert!(r["is_error"] != true, "{r}");
        assert_eq!(std::fs::read_to_string(p("f.txt")).unwrap(), "A\nb\nC\n");
        let mode = std::fs::metadata(p("bin/run.sh")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the moved file keeps its mode");
        assert!(!p("run.sh").exists());
        assert_eq!(std::fs::read_to_string(p("same.txt")).unwrap(), "y\n", "a move onto itself keeps the file");
        assert!(!p("h1.txt").exists() && std::fs::read_to_string(p("h2.txt")).unwrap() == "new\n");

        let add = "*** Begin Patch\n*** Add File: exists.txt\n+clobber\n*** End Patch";
        let r = call(&h, "apply_patch", json!({ "input": add })).await;
        assert!(r["is_error"] == true && text(&r).contains("not_observed"), "{r}");
        assert_eq!(std::fs::read_to_string(p("exists.txt")).unwrap(), "keep\n");
        h.shutdown().await;
    })
    .await
}

/// apply_patch simulates under the file lock: an edit of the same file that finished first is
/// not a "stale" failure, both changes land
#[tokio::test]
async fn apply_patch_and_edit_of_one_file_are_queued_not_stale() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("r.txt"), "one\ntwo\nthree\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "r.txt" })).await;
        let both: String = eval(
            &h,
            r#"local res = {}
               local e = mare.task.spawn(function()
                 res.e = T:execute({ id = "e", name = "edit",
                   arguments = { path = "r.txt", old_string = "one", new_string = "ONE" } }, {})
               end)
               local p = mare.task.spawn(function()
                 res.p = T:execute({ id = "p", name = "apply_patch", arguments = { input =
                   "*** Begin Patch\n*** Update File: r.txt\n@@\n-three\n+THREE\n*** End Patch" } }, {})
               end)
               e:wait(); p:wait()
               return tostring(res.e.is_error) .. "," .. tostring(res.p.is_error)"#,
        )
        .await;
        assert_eq!(both, "nil,nil", "{both}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("r.txt")).unwrap(), "ONE\ntwo\nTHREE\n");
        h.shutdown().await;
    })
    .await
}

/// a file the sandbox can't read: apply_patch says so instead of leaking its content through a
/// "failed to find expected lines" error
#[cfg(unix)]
#[tokio::test]
async fn apply_patch_checks_reads_against_the_sandbox() {
    local(async {
        let env = TestEnv::new().await;
        let secret = env.home.path().join("secret.txt");
        std::fs::write(&secret, "token\n").unwrap();
        let h = host(&env).await;
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        let msg: String = eval(
            &h,
            r#"mare.proc.sandbox.set_current({ level = "read-only", restrict_read = true })
               local r = T:execute({ id = "s", name = "apply_patch", arguments = { input =
                 "*** Begin Patch\n*** Update File: " .. SECRET .. "\n@@\n-nope\n+x\n*** End Patch" } }, {})
               mare.proc.sandbox.set_current(nil)
               return type(r.content) == "string" and r.content or r.content[1].text"#,
        )
        .await;
        assert!(msg.contains("sandbox") && !msg.contains("expected lines"), "{msg}");
        h.shutdown().await;
    })
    .await
}
