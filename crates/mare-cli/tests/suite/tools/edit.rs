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

/// Issue #184: file edit locks live only in one Lua VM (`local locks = {}`), so two concurrent
/// Mare runtimes editing the same file can both pass the freshness check, replace the file, and
/// report success while silently losing one edit.
#[test]
fn concurrent_edits_across_two_runtimes_do_not_silently_overwrite() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let env = rt.block_on(TestEnv::new());
    let shared = env.cwd.join("shared.txt");
    let opts1 = env.host().builtins(&["tools-apply-patch"]).no_session().opts;
    let opts2 = env.host().builtins(&["tools-apply-patch"]).no_session().opts;

    let rounds = 40usize;
    let start_bar = Arc::new(Barrier::new(2));
    let done_bar = Arc::new(Barrier::new(2));
    let intent_count = Arc::new(AtomicUsize::new(0));
    let silent_overwrites = Arc::new(AtomicUsize::new(0));

    let spawn_worker = |id: usize,
                        opts: mare_host::HostOptions,
                        shared: std::path::PathBuf,
                        start_bar: Arc<Barrier>,
                        done_bar: Arc<Barrier>,
                        intent_count: Arc<AtomicUsize>,
                        silent_overwrites: Arc<AtomicUsize>| {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let ls = tokio::task::LocalSet::new();
            ls.block_on(&rt, async move {
                let h = mare_host::Host::new(opts).await.unwrap();
                expose(&h);
                let ic = intent_count.clone();
                let sync_fn = h
                    .lua()
                    .create_function(move |_, ()| {
                        let prev = ic.fetch_add(1, Ordering::SeqCst);
                        let target = (prev / 2 + 1) * 2;
                        let deadline = Instant::now() + Duration::from_millis(80);
                        while ic.load(Ordering::SeqCst) < target && Instant::now() < deadline {
                            std::thread::yield_now();
                        }
                        Ok(())
                    })
                    .unwrap();
                h.lua().globals().set("REPRO_SYNC_INTENT", sync_fn).unwrap();
                exec(
                    &h,
                    r#"
                    C.cordis:get("policy"):voter("fs/edit-intent", function()
                        REPRO_SYNC_INTENT()
                        return nil
                    end)
                    "#,
                )
                .await;

                for _ in 0..rounds {
                    if id == 0 {
                        std::fs::write(&shared, "line_a\nline_b\n").unwrap();
                    }
                    start_bar.wait();
                    call(&h, "read", json!({ "path": "shared.txt" })).await;
                    start_bar.wait();

                    let (old_s, new_s) = if id == 0 { ("line_a", "LINE_A") } else { ("line_b", "LINE_B") };
                    let res =
                        call(&h, "edit", json!({ "path": "shared.txt", "old_string": old_s, "new_string": new_s }))
                            .await;
                    let ok = res["is_error"] != true;
                    if !ok {
                        let msg = text(&res);
                        assert!(
                            msg.contains("stale") || msg.contains("changed"),
                            "conflicting runtime failed with unexpected error instead of stale-edit: {msg}"
                        );
                    }
                    done_bar.wait();
                    if id == 0 {
                        let body = std::fs::read_to_string(&shared).unwrap();
                        if ok && body != "LINE_A\nLINE_B\n" && body.contains("LINE_B") {
                            silent_overwrites.fetch_add(1, Ordering::SeqCst);
                        }
                    } else if ok {
                        let body = std::fs::read_to_string(&shared).unwrap();
                        if body != "LINE_A\nLINE_B\n" && body.contains("LINE_A") {
                            silent_overwrites.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    done_bar.wait();
                    if silent_overwrites.load(Ordering::SeqCst) > 0 {
                        break;
                    }
                }
                h.shutdown().await;
            });
        })
    };

    let t1 = spawn_worker(
        0,
        opts1,
        shared.clone(),
        start_bar.clone(),
        done_bar.clone(),
        intent_count.clone(),
        silent_overwrites.clone(),
    );
    let t2 = spawn_worker(1, opts2, shared.clone(), start_bar, done_bar, intent_count, silent_overwrites.clone());
    t1.join().unwrap();
    t2.join().unwrap();

    let overwrites = silent_overwrites.load(Ordering::SeqCst);
    assert_eq!(
        overwrites,
        0,
        "two concurrent Mare runtimes both reported edit success while one edit silently overwrote the other (overwrites={overwrites}, final={:?})",
        std::fs::read_to_string(&shared).unwrap_or_default()
    );
}

#[test]
fn concurrent_edits_across_two_processes_with_different_mare_homes() {
    use std::collections::BTreeMap;

    let work = tempfile::tempdir().unwrap();
    let home_a = tempfile::tempdir().unwrap();
    let home_b = tempfile::tempdir().unwrap();
    let shared = work.path().join("shared.txt");
    std::fs::write(&shared, "line_a\nline_b\n").unwrap();

    // Process A reads `shared.txt`, waits until Process B has also read it, then enters
    // `edit` and holds the keyed file lock inside `fs/edit-intent` while Process B attempts
    // its concurrent edit under a completely different `MARE_HOME`.
    let plugins_a = home_a.path().join("config/plugins");
    let plugins_b = home_b.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_a).unwrap();
    std::fs::create_dir_all(&plugins_b).unwrap();
    std::fs::write(
        plugins_a.join("sync_a.lua"),
        r#"
        return {
            name = "sync_a",
            inject = { "policy" },
            apply = function(ctx)
                ctx:on("item/completed", function(ev)
                    if ev and ev.item and ev.item.type == "tool_call" and ev.item.name == "read" then
                        assert(mare.fs.write(mare.cwd .. "/a-read-done", "1"))
                        for _ = 1, 500 do
                            if mare.fs.stat(mare.cwd .. "/b-read-done") then break end
                            mare.task.sleep(10)
                        end
                    end
                end)
                ctx.policy:voter("fs/edit-intent", function()
                    assert(mare.fs.write(mare.cwd .. "/a-holding-lock", "1"))
                    for _ = 1, 500 do
                        if mare.fs.stat(mare.cwd .. "/b-attempting-edit") then break end
                        mare.task.sleep(10)
                    end
                    -- Keep holding the OS lock briefly after B enters edit so B must block on it
                    -- if and only if both MARE_HOME profiles share the same keyed lock namespace.
                    mare.task.sleep(150)
                    return nil
                end)
            end,
        }
        "#,
    )
    .unwrap();
    std::fs::write(
        plugins_b.join("sync_b.lua"),
        r#"
        return {
            name = "sync_b",
            inject = { "policy" },
            apply = function(ctx)
                ctx:on("item/completed", function(ev)
                    if ev and ev.item and ev.item.type == "tool_call" and ev.item.name == "read" then
                        assert(mare.fs.write(mare.cwd .. "/b-read-done", "1"))
                        for _ = 1, 500 do
                            if mare.fs.stat(mare.cwd .. "/a-holding-lock") then break end
                            mare.task.sleep(10)
                        end
                    end
                end)
                ctx.policy:voter("tool/before", function(call)
                    if call and call.name == "edit" then
                        assert(mare.fs.write(mare.cwd .. "/b-attempting-edit", "1"))
                    end
                    return nil
                end)
            end,
        }
        "#,
    )
    .unwrap();

    let script_a = home_a.path().join("faux_a.json");
    let script_b = home_b.path().join("faux_b.json");
    std::fs::write(
        &script_a,
        json!([
            { "tool_calls": [{ "id": "ra", "name": "read", "arguments": { "path": "shared.txt" } }] },
            { "tool_calls": [{ "id": "ea", "name": "edit", "arguments": { "path": "shared.txt", "old_string": "line_a", "new_string": "LINE_A" } }] },
            { "text": "done-a" }
        ])
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        &script_b,
        json!([
            { "tool_calls": [{ "id": "rb", "name": "read", "arguments": { "path": "shared.txt" } }] },
            { "tool_calls": [{ "id": "eb", "name": "edit", "arguments": { "path": "shared.txt", "old_string": "line_b", "new_string": "LINE_B" } }] },
            { "text": "done-b" }
        ])
        .to_string(),
    )
    .unwrap();

    let run_proc =
        |home: std::path::PathBuf, script: std::path::PathBuf, cwd: std::path::PathBuf, label: &'static str| {
            std::thread::spawn(move || {
                let env_map = BTreeMap::from([
                    ("MARE_HOME".to_string(), home.display().to_string()),
                    ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
                ]);
                let mut client =
                    crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env_map, &cwd);
                let init = client.call("initialize", json!({ "protocol_version": "3.0" }));
                let session_id = init["session"]["session_id"].as_str().expect("session_id").to_string();
                client.call("events.subscribe", json!({ "session_id": session_id, "include": ["item/*", "turn/*"] }));
                let turn = client.call("agent.prompt", json!({ "session_id": session_id, "input": label }));
                assert_eq!(turn["status"], "started", "{turn}");
                let _ = client.call("agent.wait", json!({ "session_id": session_id, "turn_id": turn["turn_id"] }));
                let mut edit_error: Option<String> = None;
                for ev in &client.events {
                    if ev["type"] == "item/completed"
                        && ev["data"]["item"]["type"] == "tool_call"
                        && ev["data"]["item"]["name"] == "edit"
                    {
                        let data = &ev["data"];
                        let is_err = data["item"]["message"]["is_error"] == true
                            || data["status"].as_str().is_some_and(|s| s != "completed");
                        if is_err {
                            edit_error =
                                Some(data["item"]["message"]["content"][0]["text"].as_str().unwrap_or("").to_string());
                        }
                    }
                }
                client.close_stdin();
                let _ = client.drain_and_wait();
                edit_error
            })
        };

    let t_a = run_proc(home_a.path().to_path_buf(), script_a, work.path().to_path_buf(), "proc-a");
    let t_b = run_proc(home_b.path().to_path_buf(), script_b, work.path().to_path_buf(), "proc-b");
    let err_a = t_a.join().unwrap();
    let err_b = t_b.join().unwrap();

    assert!(err_a.is_none(), "process A holding the lock first should succeed, got error: {err_a:?}");
    let err_b = err_b.expect(
        "process B with a different MARE_HOME must serialize on the same OS lock and see stale file instead of silently overwriting process A",
    );
    assert!(
        err_b.contains("stale") || err_b.contains("changed"),
        "expected stale-edit error in process B, got: {err_b}"
    );
    assert_eq!(
        std::fs::read_to_string(&shared).unwrap(),
        "LINE_A\nline_b\n",
        "process A's edit must not be silently overwritten by process B under a different MARE_HOME"
    );
}
