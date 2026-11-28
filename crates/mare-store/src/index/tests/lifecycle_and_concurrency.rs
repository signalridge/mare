use super::*;

#[test]
fn reconcile_does_not_read_unchanged_session_contents() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    append(&root.join("--w--/session.jsonl"), &[header("session", "/w"), user("1", "first", 1)]);
    let s = ready(&root).unwrap();

    scan::reset_read_from_opens();
    let reconciled = reconcile(&s).unwrap();
    assert_eq!(reconciled.updated, 0);
    assert_eq!(reconciled.failed, 0);
    assert_eq!(scan::read_from_opens(), 0, "the unchanged reconcile path must stay stat-only");
}

#[cfg(unix)]
#[test]
fn initial_ready_build_retries_if_a_source_is_replaced_before_commit() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("old-id", "/old"), user("1", "old", 1)]);
    let replacement = path.with_extension("replacement");
    append(&replacement, &[header("new-id", "/new"), user("1", "new", 2)]);
    let source = path.clone();
    after_next_build_scan(move || std::fs::rename(&replacement, &source).unwrap());

    // `ready` is the same lazy build path used by app/session index reads.
    let s = ready(&root).unwrap();
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["new-id"]);
    assert_eq!(rows[0].cwd, "/new");
    assert_eq!(rows[0].message_count, 1);
}

const ROWS_PER_WRITER: usize = 25;

/// Child mode: when MARE_INDEX_CHILD is set, this "test" is a writer process.
#[test]
fn index_writer_child() {
    let Ok(root) = std::env::var("MARE_INDEX_CHILD") else { return };
    let who = std::env::var("MARE_INDEX_WHO").unwrap_or_default();
    let root = PathBuf::from(root);
    let mut ix = Index::open(&root).unwrap();
    for i in 0..ROWS_PER_WRITER {
        let p = root.join(format!("--w{who}--/{i}.jsonl"));
        append(&p, &[header(&format!("{who}-{i}"), "/w"), user("1", "x", i as i64)]);
        ix.update(&p, None).unwrap();
        append(&p, &[user("2", "y", i as i64)]);
        ix.update(&p, None).unwrap();
    }
}

/// Several processes writing at once: every row lands, every `seq` is distinct.
#[test]
fn concurrent_writers_from_processes() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var("MARE_INDEX_CHILD").is_ok() {
        return;
    }
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let s = ready(&root).unwrap();
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|w| {
            std::process::Command::new(&exe)
                .args(["--exact", "index::tests::lifecycle_and_concurrency::index_writer_child", "--test-threads=1"])
                .env("MARE_INDEX_CHILD", &root)
                .env("MARE_INDEX_WHO", w.to_string())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    // reconcile concurrently: it must interleave, never lose a row
    for _ in 0..5 {
        let _ = reconcile(&s);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let outputs: Vec<_> = children.into_iter().map(|c| c.wait_with_output().unwrap()).collect();
    for (w, out) in outputs.iter().enumerate() {
        assert!(
            out.status.success(),
            "writer {w} failed ({}):\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let ix = s.lock().unwrap();
    let rows = all(&ix);
    assert_eq!(rows.len(), 4 * ROWS_PER_WRITER);
    assert!(rows.iter().all(|r| r.message_count == 2), "every catch-up landed");
    let seqs: HashSet<i64> = rows.iter().map(|r| r.seq).collect();
    assert_eq!(seqs.len(), rows.len());
}

#[test]
fn update_rebases_when_concurrent_reconcile_commits_same_generation() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    let mut writer = Index::open(&root).unwrap();
    writer.ensure_built().unwrap();
    let mut peer = Index::open(&root).unwrap();

    // Capture a header-only row (L0), clear the table, then append message 1 (L1).
    append(&path, &[header("s1", "/w")]);
    let row_l0 = peer.update(&path, None).unwrap().unwrap();
    peer.conn.execute("DELETE FROM sessions", []).unwrap();
    append(&path, &[user("1", "first", 1)]);

    // Simulate reconcile committing L0 before writer's first tx (and L1 if it ever retried).
    let calls1 = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let calls1_hook = calls1.clone();
    let peer_path = path.clone();
    let peer_l0 = row_l0.clone();
    before_update_tx(Some(Box::new(move || {
        let n = calls1_hook.get() + 1;
        calls1_hook.set(n);
        if n == 1 {
            write_row(&peer.conn, &peer_l0).unwrap();
        } else {
            peer.update(&peer_path, None).unwrap();
        }
    })));
    let landed = writer.update(&path, None);
    before_update_tx(None);
    let landed = landed.unwrap().unwrap();
    assert_eq!(calls1.get(), 1, "prefix concurrent commit must rebase in the first transaction without retrying");
    assert_eq!(landed.message_count, 1);
    assert!(landed.seq > row_l0.seq, "rebasing a prefix commit that gained a message must advance seq");

    // Also verify identical-watermark concurrent commit succeeds without burning a seq.
    append(&path, &[user("2", "second", 2)]);
    let mut peer2 = Index::open(&root).unwrap();
    let peer2_path = path.clone();
    let committed_seq = std::rc::Rc::new(std::cell::Cell::new(0i64));
    let committed_seq_hook = committed_seq.clone();
    let calls2 = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let calls2_hook = calls2.clone();
    before_update_tx(Some(Box::new(move || {
        calls2_hook.set(calls2_hook.get() + 1);
        let r = peer2.update(&peer2_path, None).unwrap().unwrap();
        committed_seq_hook.set(r.seq);
    })));
    let same = writer.update(&path, None);
    before_update_tx(None);
    let same = same.unwrap().unwrap();
    assert_eq!(
        calls2.get(),
        1,
        "identical-watermark concurrent commit must rebase in the first transaction without retrying"
    );
    assert_eq!(same.message_count, 2);
    assert_eq!(same.seq, committed_seq.get());
}

/// `cargo test -p mare-store bench_500 -- --ignored --nocapture`
#[test]
#[ignore = "benchmark, not a test: run by hand with --ignored --nocapture"]
fn bench_500_sessions() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let proj = root.join("--w--");
    for i in 0..500 {
        let p = proj.join(format!("2026-01-01T00-{:02}-{:02}_s{i:04}.jsonl", i / 60, i % 60));
        // every 50th session is big (~30MB): thousands of messages with padded text
        let msgs = if i % 50 == 0 { 30_000 } else { 40 };
        let mut lines = vec![header(&format!("s{i:04}"), "/w")];
        for m in 0..msgs {
            let text = format!("message {m} {}", "lorem ipsum ".repeat(if msgs > 1000 { 80 } else { 4 }));
            lines.push(user(&format!("m{m}"), &text, m));
        }
        append(&p, &lines);
    }
    let total: u64 = session_files(&root).iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
    let t = std::time::Instant::now();
    let s = ready(&root).unwrap();
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = std::time::Instant::now();
    let r = reconcile(&s).unwrap();
    let reconcile_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = std::time::Instant::now();
    let (page, n) = s.lock().unwrap().list(&Query { limit: Some(100), ..Default::default() }).unwrap();
    let list_ms = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "500 sessions ({:.0} MB): build {build_ms:.0} ms · reconcile (unchanged) {reconcile_ms:.1} ms ({} files) · \
         first page {list_ms:.2} ms ({} of {n})",
        total as f64 / 1e6,
        r.files,
        page.len()
    );
    assert!(list_ms < 100.0);
}

#[test]
fn close_idle_refuses_active_owners_without_losing_checkpoint_validation() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("close-idle", "/w"), user("1", "body", 1)]);
    let index = ready(&root).unwrap();
    let checkpoint = checkpoint(&path);
    index.lock().unwrap().set_checkpoint(&path, &checkpoint).unwrap();
    let generation = checkpoint_generation(&path).unwrap();
    let identity = (generation.file_dev as u64, generation.file_ino as u64);
    assert!(close_idle(&root).unwrap_err().to_string().contains("active owners"));
    assert!(checkpoint_is_current(&path, &checkpoint, identity).unwrap());
    index.lock().unwrap().conn.execute("UPDATE sessions SET verified_len = NULL", []).unwrap();
    assert!(!checkpoint_is_current(&path, &checkpoint, identity).unwrap());
    drop(index);
    close_idle(&root).unwrap();
    dir.close().unwrap();
}

#[test]
fn close_idle_releases_cached_files_before_root_removal() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let index = shared(&root).unwrap();
    let weak = Arc::downgrade(&index);
    drop(index);
    assert!(weak.upgrade().is_some(), "the process cache owns the connection");
    #[cfg(windows)]
    {
        let error =
            std::fs::remove_dir_all(dir.path()).expect_err("cached SQLite and lock handles must block deletion");
        eprintln!(
            "cached index deletion proof: {}: {error}; entries: {:?}",
            dir.path().display(),
            std::fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().path()).collect::<Vec<_>>()
        );
    }
    close_idle(&root).unwrap();
    assert!(weak.upgrade().is_none(), "explicit close must release the cache owner");
    dir.close().unwrap();
}

#[test]
fn close_idle_restores_cache_when_sqlite_close_fails() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let index = shared(&root).unwrap();
    // SAFETY: the raw statement is kept valid by the cached connection until finalized below.
    let statement = unsafe {
        let connection = index.lock().unwrap();
        let mut statement = std::ptr::null_mut();
        let result = rusqlite::ffi::sqlite3_prepare_v2(
            connection.conn.handle(),
            c"SELECT 1".as_ptr(),
            -1,
            &mut statement,
            std::ptr::null_mut(),
        );
        assert_eq!(result, rusqlite::ffi::SQLITE_OK);
        statement
    };
    drop(index);
    assert!(close_idle(&root).is_err(), "unfinalized statement must report SQLITE_BUSY");
    let reopened = shared(&root).unwrap();
    assert_eq!(reopened.lock().unwrap().conn.query_row("SELECT 2", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    // SAFETY: this is the live statement created above, finalized exactly once.
    unsafe { assert_eq!(rusqlite::ffi::sqlite3_finalize(statement), rusqlite::ffi::SQLITE_OK) };
    drop(reopened);
    close_idle(&root).unwrap();
    dir.close().unwrap();
}
