//! Built-in `edit` tool tests: matching, read guard, atomic writes, locks, and streaming.

use super::{call, host, text};
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn replace_edit_rejects_missing_text_and_file() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("e.txt"), "a\nb\na\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "e.txt" })).await;
        let missing = call(&h, "edit", json!({ "path": "e.txt", "old_string": "zzz", "new_string": "y" })).await;
        let no_file = call(&h, "edit", json!({ "path": "nope.txt", "old_string": "a", "new_string": "y" })).await;
        for r in [&missing, &no_file] {
            assert_eq!(r["is_error"], true, "{r}");
        }
        assert_eq!(std::fs::read_to_string(env.cwd.join("e.txt")).unwrap(), "a\nb\na\n", "failed edits change nothing");
        let ok = call(&h, "edit", json!({ "path": "e.txt", "old_string": "b", "new_string": "B" })).await;
        assert!(text(&ok).contains("(+1 -1)"), "{ok}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_ambiguous_old_string_is_refused_with_ambiguous_edit_and_the_matching_lines() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = "[a]\nenabled=false\n[b]\nenabled=false\n";
        std::fs::write(env.cwd.join("config.txt"), cfg).unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "config.txt" })).await;
        let r = call(
            &h,
            "edit",
            json!({ "path": "config.txt", "old_string": "enabled=false", "new_string": "enabled=true" }),
        )
        .await;
        assert_eq!(r["is_error"], true, "{r}");
        let t = text(&r);
        assert!(t.contains("ambiguous_edit") && t.contains("2 locations") && t.contains("2, 4"), "{t}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("config.txt")).unwrap(), cfg, "nothing changed");
        let ok = call(
            &h,
            "edit",
            json!({ "path": "config.txt", "old_string": "[b]\nenabled=false", "new_string": "[b]\nenabled=true" }),
        )
        .await;
        assert!(ok["is_error"] != true, "{ok}");
        assert_eq!(
            std::fs::read_to_string(env.cwd.join("config.txt")).unwrap(),
            "[a]\nenabled=false\n[b]\nenabled=true\n"
        );
        h.shutdown().await;
    })
    .await
}

// --- read before edit --------------------------------------------------------------

#[tokio::test]
async fn edit_without_a_prior_read_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("g.txt"), "one\n").unwrap();
        let h = host(&env).await;
        let r = call(&h, "edit", json!({ "path": "g.txt", "old_string": "one", "new_string": "two" })).await;
        assert_eq!(r["is_error"], true, "{r}");
        assert!(text(&r).contains("not_observed"), "{r}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("g.txt")).unwrap(), "one\n");
        // after reading, the same edit goes through; a second edit needs no re-read
        call(&h, "read", json!({ "path": "g.txt" })).await;
        let ok = call(&h, "edit", json!({ "path": "g.txt", "old_string": "one", "new_string": "two" })).await;
        assert!(ok["is_error"] != true, "{ok}");
        let again = call(&h, "edit", json!({ "path": "g.txt", "old_string": "two", "new_string": "three" })).await;
        assert!(again["is_error"] != true, "{again}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("g.txt")).unwrap(), "three\n");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn edit_after_the_file_changed_on_disk_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        let f = env.cwd.join("c.txt");
        std::fs::write(&f, "v1\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "c.txt" })).await;
        std::fs::write(&f, "v2 by someone else\n").unwrap();
        let r = call(&h, "edit", json!({ "path": "c.txt", "old_string": "v2", "new_string": "x" })).await;
        assert_eq!(r["is_error"], true, "{r}");
        assert!(text(&r).contains("changed since you read it"), "{r}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v2 by someone else\n");
        // overwriting with write is guarded the same way
        let w = call(&h, "write", json!({ "path": "c.txt", "content": "mine\n" })).await;
        assert_eq!(w["is_error"], true, "{w}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn edits_are_atomic_and_keep_crlf_and_permissions() {
    local(async {
        let env = TestEnv::new().await;
        let f = env.cwd.join("w.txt");
        std::fs::write(&f, "a\r\nb\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "w.txt" })).await;
        let r = call(&h, "edit", json!({ "path": "w.txt", "old_string": "b", "new_string": "B" })).await;
        assert!(r["is_error"] != true, "{r}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\r\nB\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o640);
        }
        // replaced by rename: no temp files left behind next to the target
        let stray: Vec<_> = std::fs::read_dir(&env.cwd)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_edits_of_one_file_are_queued() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("q.txt"), "a\nb\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "q.txt" })).await;
        // two edits started together: without the queue the second would see a stale version
        let both: String = eval(
            &h,
            r#"local res = {}
               local function go(i, old, new)
                 return mare.task.spawn(function()
                   res[i] = T:execute({ id = "c" .. i, name = "edit",
                     arguments = { path = "q.txt", old_string = old, new_string = new } }, {})
                 end)
               end
               local t1, t2 = go(1, "a", "A"), go(2, "b", "B")
               t1:wait(); t2:wait()
               return mare.json.encode(res)"#,
        )
        .await;
        let res: serde_json::Value = serde_json::from_str(&both).unwrap();
        for (i, r) in res.as_array().unwrap().iter().enumerate() {
            assert!(r["is_error"] != true, "concurrent edit {i} failed: {r}\nall: {res}");
        }
        assert_eq!(std::fs::read_to_string(env.cwd.join("q.txt")).unwrap(), "A\nB\n");
        h.shutdown().await;
    })
    .await
}

/// Deterministic regression for concurrent `edit` pipeline queuing: when a second edit arrives
/// while the first edit is in flight (between `started` persistence and atomic rename), it must
/// wait on the per-file lock before freezing its access snapshot rather than failing with
/// `"Tool call blocked: its approval changed before execution"`.
#[tokio::test]
async fn concurrent_edit_waits_for_inflight_edit_before_freezing_access_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("q.txt"), "a\nb\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "q.txt" })).await;
        let out: String = eval(
            &h,
            r#"local res = {}
               local t1_done = mare.task.token()
               local t2
               local t1 = mare.task.spawn(function()
                 res[1] = T:execute({ id = "c1", name = "edit",
                   arguments = { path = "q.txt", old_string = "a", new_string = "A" } }, {
                   on_state = function(_, state)
                     if state == "started" then
                       t2 = mare.task.spawn(function()
                         res[2] = T:execute({ id = "c2", name = "edit",
                           arguments = { path = "q.txt", old_string = "b", new_string = "B" } }, {
                           on_state = function(_, s2)
                             if s2 == "started" then
                               t1_done:wait()
                             end
                             return true
                           end,
                         })
                       end)
                       mare.task.sleep(50)
                     end
                     return true
                   end,
                 })
                 t1_done:cancel()
               end)
               t1:wait()
               assert(t2 ~= nil, "second edit task was not spawned")
               t2:wait()
               return mare.json.encode(res)"#,
        )
        .await;
        let res: serde_json::Value = serde_json::from_str(&out).unwrap();
        for (i, r) in res.as_array().unwrap().iter().enumerate() {
            assert!(r["is_error"] != true, "concurrent edit {i} failed: {r}\nall: {res}");
        }
        assert_eq!(std::fs::read_to_string(env.cwd.join("q.txt")).unwrap(), "A\nB\n");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn a_file_read_through_a_symlink_counts_as_read_for_its_real_path() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("real.txt"), "one\n").unwrap();
        std::os::unix::fs::symlink(env.cwd.join("real.txt"), env.cwd.join("link.txt")).unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "link.txt" })).await;
        // keyed by file identity, not by the spelling of the path
        let r = call(&h, "edit", json!({ "path": "real.txt", "old_string": "one", "new_string": "two" })).await;
        assert!(r["is_error"] != true, "{r}");
        // and the record follows the file through the atomic replace: a second edit needs no re-read
        let r = call(&h, "edit", json!({ "path": "link.txt", "old_string": "two", "new_string": "three" })).await;
        assert!(r["is_error"] != true, "{r}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("real.txt")).unwrap(), "three\n");
        assert!(
            std::fs::symlink_metadata(env.cwd.join("link.txt")).unwrap().file_type().is_symlink(),
            "the link stays a link"
        );
        h.shutdown().await;
    })
    .await
}

// --- comparison-study batch two: typed refusals, bounded reads, chained patches ---

/// non-UTF-8 text is a typed `not_text` refusal; edits load at most `edit_max_bytes` (exactly at
/// the cap is fine); big files are read as a streamed window and still count as read
#[tokio::test]
async fn edits_refuse_non_text_and_oversized_files_and_big_reads_stream() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("latin1.txt"), b"caf\xe9\n").unwrap();
        std::fs::write(env.cwd.join("cap.txt"), format!("{}\n", "a".repeat(63))).unwrap(); // 64 bytes
        std::fs::write(env.cwd.join("over.txt"), format!("{}\n", "b".repeat(64))).unwrap(); // 65 bytes
        let big: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(env.cwd.join("big.txt"), &big).unwrap();
        let h = env
            .host()
            .no_session()
            .config(json!({ "plugins": { "tools": { "edit_max_bytes": 64, "read_stream_min_bytes": 32 } } }))
            .build()
            .await;
        expose(&h);
        let r = call(&h, "read", json!({ "path": "latin1.txt" })).await;
        assert!(r["is_error"] == true && r["error_code"] == "not_text", "{r}");
        let r = call(&h, "edit", json!({ "path": "latin1.txt", "old_string": "caf", "new_string": "x" })).await;
        assert!(r["is_error"] == true && text(&r).contains("not_text"), "{r}");

        call(&h, "read", json!({ "path": "cap.txt" })).await;
        let r =
            call(&h, "edit", json!({ "path": "cap.txt", "old_string": "aaa", "new_string": "c", "replace_all": true }))
                .await;
        assert!(r["is_error"] != true, "exactly at the cap: {r}");
        call(&h, "read", json!({ "path": "over.txt" })).await;
        let r =
            call(&h, "edit", json!({ "path": "over.txt", "old_string": "b", "new_string": "c", "replace_all": true }))
                .await;
        assert!(r["is_error"] == true && text(&r).contains("too_large"), "{r}");

        let r = call(&h, "read", json!({ "path": "big.txt", "offset": 10, "limit": 2 })).await;
        let t = text(&r);
        assert!(t.contains("line 10") && t.contains("line 11") && !t.contains("line 12\n"), "{t}");
        // the streamed window still recorded the whole file's version (what an edit checks)
        let same: bool = eval(
            &h,
            r#"local o = T:observation("big.txt")
               return o.hash == mare.crypto.sha256(mare.fs.read("big.txt")) and o.size == #mare.fs.read("big.txt")"#,
        )
        .await;
        assert!(same);
        h.shutdown().await;
    })
    .await
}

/// The per-file lock holds across an atomic write: the writer renames a new file over the old one
/// (a new identity), and a caller that arrives after the rename still waits for the lock instead
/// of keying it by the new identity and running alongside (a flaky stale error on Linux CI).
#[tokio::test]
async fn the_file_lock_holds_across_an_atomic_rewrite() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("r.txt"), "one\n").unwrap();
        let h = host(&env).await;
        let order: String = eval(
            &h,
            r#"local log, tx, rx = {}, mare.task.channel()
               local a = mare.task.spawn(function()
                 T:with_file_lock({ "r.txt" }, function()
                   mare.fs.write("r.txt", "two\n", { atomic = true })
                   log[#log + 1] = "a-wrote"
                   rx:recv()
                   log[#log + 1] = "a-done"
                 end)
               end)
               for _ = 1, 6000 do if #log > 0 then break end mare.task.sleep(10) end
               local b = mare.task.spawn(function()
                 T:with_file_lock({ "r.txt" }, function() log[#log + 1] = "b" end)
               end)
               mare.task.sleep(100)
               tx:send(true)
               a:wait(); b:wait()
               return table.concat(log, ",")"#,
        )
        .await;
        assert_eq!(order, "a-wrote,a-done,b", "the second caller ran inside the first one's lock");
        h.shutdown().await;
    })
    .await
}

/// editing a hard-linked file updates the shared content (written in place, not renamed over)
#[cfg(unix)]
#[tokio::test]
async fn edits_of_a_hard_linked_file_keep_the_link() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("l1.txt"), "v1\n").unwrap();
        std::fs::hard_link(env.cwd.join("l1.txt"), env.cwd.join("l2.txt")).unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "l1.txt" })).await;
        let r = call(&h, "edit", json!({ "path": "l1.txt", "old_string": "v1", "new_string": "v2" })).await;
        assert!(r["is_error"] != true, "{r}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("l2.txt")).unwrap(), "v2\n");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_edits_via_different_path_spellings_share_one_lock_and_queue() {
    local(async {
        let env = TestEnv::new().await;
        let sub = env.cwd.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let target = sub.join("alias.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();
        let h = host(&env).await;
        call(&h, "read", json!({ "path": "sub/alias.txt" })).await;
        let out: String = eval(
            &h,
            r#"local res = {}
               local t1 = mare.task.spawn(function()
                 res[1] = T:execute({ id = "c1", name = "edit",
                   arguments = { path = "sub/./alias.txt", old_string = "alpha", new_string = "ALPHA" } }, {})
               end)
               local t2 = mare.task.spawn(function()
                 res[2] = T:execute({ id = "c2", name = "edit",
                   arguments = { path = "sub/../sub/alias.txt", old_string = "beta", new_string = "BETA" } }, {})
               end)
               t1:wait(); t2:wait()
               return mare.json.encode(res)"#,
        )
        .await;
        let res: serde_json::Value = serde_json::from_str(&out).unwrap();
        for (i, r) in res.as_array().unwrap().iter().enumerate() {
            assert!(r["is_error"] != true, "alias edit {i} failed: {r}\nall: {res}");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ALPHA\nBETA\n");
        h.shutdown().await;
    })
    .await
}

/// If a waiter inside `tools:with_file_lock` is forcibly aborted by `mare.task.select`,
/// releasing the lock must not hand ownership to the dead waiter (and any already-acquired
/// prefix keys in a multi-file lock must be rolled back).
#[tokio::test]
async fn aborted_file_lock_waiter_does_not_leak_lock_ownership() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "a\n").unwrap();
        std::fs::write(env.cwd.join("b.txt"), "b\n").unwrap();
        let h = host(&env).await;
        let (single_ok, multi_ok): (bool, bool) = eval(
            &h,
            r#"-- Part 1: single-file lock waiter aborted by mare.task.select
               local h_ready_tx, h_ready_rx = mare.task.channel()
               local h_rel_tx, h_rel_rx = mare.task.channel()
               local holder = mare.task.spawn(function()
                 T:with_file_lock({ "a.txt" }, function()
                   h_ready_tx:send(true)
                   h_rel_rx:recv()
                 end)
               end)
               h_ready_rx:recv()

               -- Waiter W1 blocks on a.txt and is forcibly aborted after 30ms by select.
               mare.task.select(
                 function()
                   T:with_file_lock({ "a.txt" }, function() end)
                 end,
                 function()
                   mare.task.sleep(30)
                   return "aborted"
                 end
               )
               -- Holder releases a.txt; W2 must acquire it instead of hanging forever.
               h_rel_tx:send(true)
               holder:wait()
               local _, single_res = mare.task.select(
                 function()
                   T:with_file_lock({ "a.txt" }, function() end)
                   return "acquired"
                 end,
                 function()
                   mare.task.sleep(1000)
                   return "timeout"
                 end
               )

               -- Part 2: multi-file lock where W1 acquires a.txt, blocks on b.txt, and is aborted.
               local h2_ready_tx, h2_ready_rx = mare.task.channel()
               local h2_rel_tx, h2_rel_rx = mare.task.channel()
               local holder2 = mare.task.spawn(function()
                 T:with_file_lock({ "b.txt" }, function()
                   h2_ready_tx:send(true)
                   h2_rel_rx:recv()
                 end)
               end)
               h2_ready_rx:recv()

               mare.task.select(
                 function()
                   T:with_file_lock({ "a.txt", "b.txt" }, function() end)
                 end,
                 function()
                   mare.task.sleep(30)
                   return "aborted"
                 end
               )
               h2_rel_tx:send(true)
               holder2:wait()
               local _, multi_res = mare.task.select(
                 function()
                   T:with_file_lock({ "a.txt", "b.txt" }, function() end)
                   return "acquired"
                 end,
                 function()
                   mare.task.sleep(1000)
                   return "timeout"
                 end
               )
               return single_res == "acquired", multi_res == "acquired""#,
        )
        .await;
        assert!(single_ok, "aborted single-file waiter leaked lock ownership");
        assert!(multi_ok, "aborted multi-file waiter leaked prefix or waited key");
        h.shutdown().await;
    })
    .await
}
