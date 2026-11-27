use super::*;

/// Order is the index's own counter: a row moves up when it gains messages (whatever the
/// clock says), a rename / label / archive changes its column without reordering, and a
/// session written while the index was offline is caught up from its watermark.
#[test]
fn rows_follow_their_files_and_sort_by_activity() {
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let (a, b) = (root.join("--w--/a.jsonl"), root.join("--w--/b.jsonl"));
    let mut ix = Index::open(root).unwrap();
    assert_eq!(ix.status(), &Status::Created);
    assert!(ix.ensure_built().unwrap());
    append(&a, &[header("a", "/w"), user("1", "first", 2000)]);
    append(&b, &[header("b", "/w"), user("1", "second", 1000)]); // the clock went back
    ix.update(&a, Some("/w")).unwrap();
    ix.update(&b, Some("/w")).unwrap();
    assert_eq!(ids(&all(&ix)), ["b", "a"]);
    append(&a, &[json!({"type":"name","name":"Named"}), json!({"type":"archive","archived":true,"created_at_ms":5})]);
    let r = ix.update(&a, None).unwrap().unwrap();
    assert_eq!((r.title.as_deref(), r.archived_at_ms), (Some("Named"), Some(5)));
    assert_eq!(ids(&all(&ix)), ["b", "a"], "a rename / archive doesn't reorder");
    append(&a, &[user("2", "more", 10)]);
    ix.update(&a, None).unwrap();
    assert_eq!(ids(&all(&ix)), ["a", "b"], "new messages move it up");
    assert_eq!(all(&ix)[0].message_count, 2);
    // the live writer's directory is kept over the header's
    assert_eq!(all(&ix)[0].cwd, "/w");
    let unchanged = all(&ix);
    ix.update(&a, None).unwrap();
    assert_eq!(all(&ix), unchanged, "nothing appended: nothing changes");
    // a file deleted behind the index's back loses its row on the next update
    std::fs::remove_file(&b).unwrap();
    assert_eq!(ix.update(&b, None).unwrap(), None);
    assert_eq!(ids(&all(&ix)), ["a"]);
}

#[test]
fn listing_filters_pages_and_counts() {
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let mut ix = Index::open(root).unwrap();
    ix.ensure_built().unwrap();
    for (i, cwd) in ["/w", "/w", "/x", "/w"].iter().enumerate() {
        let p = root.join(format!("--s--/{i}.jsonl"));
        append(&p, &[header(&format!("id{i}"), cwd), user("1", "hi", i as i64)]);
        ix.update(&p, None).unwrap();
    }
    let empty = root.join("--s--/empty.jsonl");
    append(&empty, &[header("idE", "/w")]);
    ix.update(&empty, None).unwrap();
    let agent = root.join("--s--/agents/r/000-a.jsonl");
    append(&agent, &[json!({"type":"session","version":3,"session_id":"ag","parent_session_id":"id0","cwd":"/w"})]);
    ix.update(&agent, None).unwrap();
    append(&root.join("--s--/1.jsonl"), &[json!({"type":"archive","archived":true})]);
    ix.update(&root.join("--s--/1.jsonl"), None).unwrap();

    let q = |f: &dyn Fn(&mut Query)| {
        let mut q = Query { archived: Some(false), ..Default::default() };
        f(&mut q);
        let (rows, total) = ix.list(&q).unwrap();
        (ids(&rows).into_iter().map(str::to_string).collect::<Vec<_>>(), total)
    };
    assert_eq!(q(&|_| {}).0, ["idE", "id3", "id2", "id0"], "no agents, no archived");
    assert_eq!(q(&|q| q.nonempty = true).0, ["id3", "id2", "id0"]);
    assert_eq!(q(&|q| q.cwd = Some("/w".into())).0, ["idE", "id3", "id0"]);
    assert_eq!(q(&|q| q.archived = Some(true)).0, ["id1"]);
    assert_eq!(q(&|q| (q.limit, q.offset) = (Some(2), 1)), (vec!["id3".to_string(), "id2".to_string()], 4));
    assert_eq!(q(&|q| q.session_id_prefix = Some("id2".into())).0, ["id2"]);
    assert_eq!(q(&|q| (q.kinds, q.parent_session_id) = (Kinds::Agent, Some("id0".into()))).0, ["ag"]);
    // the degraded listing (no database) filters the same way
    let mut dq = Query { archived: Some(false), cwd: Some("/w".into()), ..Default::default() };
    let (rows, _) = scan_list(root, &dq).unwrap();
    let mut got = ids(&rows);
    got.sort();
    assert_eq!(got, ["id0", "id3", "idE"]);
    dq.kinds = Kinds::Agent;
    assert_eq!(ids(&scan_list(root, &dq).unwrap().0), ["ag"]);
    // deleting a session drops its sub-agents' rows too
    ix.remove(Some("id0"), None).unwrap();
    assert!(all(&ix).iter().all(|r| r.session_id != "id0" && r.session_id != "ag"));
}

#[test]
fn first_build_ignores_legacy_jsonl_index() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let session = root.join("--w--/a.jsonl");
    append(&session, &[header("a", "/w"), user("1", "x", 1)]);
    let old = dir.path().join("data/index.jsonl");
    append(&old, &[json!({"checkpoint":session,"len":12,"mtime_ns":34})]);
    let before = std::fs::read(&old).unwrap();
    let mut ix = Index::open(&root).unwrap();
    assert!(ix.ensure_built().unwrap());
    assert_eq!(ids(&all(&ix)), ["a"]);
    assert_eq!(ix.checkpoint(&session).unwrap(), None);
    assert_eq!(std::fs::read(&old).unwrap(), before);
    assert!(!dir.path().join("data/index.jsonl.migrated").exists());
}

#[test]
fn rebuilding_preserves_existing_sequence_and_exact_checkpoints() {
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let (a, b, c) = (root.join("--w--/a.jsonl"), root.join("--w--/b.jsonl"), root.join("--w--/c.jsonl"));
    append(&a, &[header("a", "/w"), user("1", "x", 3000)]);
    append(&b, &[header("b", "/w"), user("1", "y", 1000)]);
    append(&c, &[header("c", "/w"), user("1", "z", 2000)]);
    let mut index = Index::open(root).unwrap();
    assert!(index.ensure_built().unwrap());
    // Set an order which differs from wall-clock timestamps.
    append(&a, &[user("2", "later", 4)]);
    index.update(&a, None).unwrap();
    append(&b, &[user("2", "later", 5)]);
    index.update(&b, None).unwrap();
    let expected: Vec<String> = all(&index).iter().map(|row| row.session_id.clone()).collect();
    assert_eq!(expected[..2], ["b", "a"]);
    let ca = checkpoint(&a);
    let cb = checkpoint(&b);
    index.set_checkpoint(&a, &ca).unwrap();
    index.set_checkpoint(&b, &cb).unwrap();
    assert!(!index.ensure_built().unwrap(), "the initial build is not repeated");
    for _ in 0..2 {
        assert_eq!(index.rebuild().unwrap(), 3);
        assert_eq!(all(&index).iter().map(|row| row.session_id.clone()).collect::<Vec<_>>(), expected);
        assert_eq!(index.checkpoint(&a).unwrap(), Some(ca.clone()));
        assert_eq!(index.checkpoint(&b).unwrap(), Some(cb.clone()));
    }
}

/// A corrupt database is set aside and rebuilt from the session files.
#[test]
fn an_unusable_database_is_set_aside_and_rebuilt() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = OwnedTempDir::new();
    let root = dir.path();
    let a = root.join("--w--/a.jsonl");
    append(&a, &[header("a", "/w"), user("1", "x", 1)]);
    let db = root.join(DB_NAME);
    std::fs::write(&db, b"not a database, just some text that is long enough to have a header").unwrap();
    let mut ix = Index::open(root).unwrap();
    assert!(matches!(ix.status(), Status::Rebuilt(_)), "{:?}", ix.status());
    assert!(ix.ensure_built().unwrap());
    assert!(ix.take_notice().is_some());
    assert_eq!(ix.take_notice(), None);
    assert_eq!(ids(&all(&ix)), ["a"]);
    let kept = std::fs::read_dir(root).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains(".bad-"));
    assert!(kept, "the corrupt file is kept aside");
}

/// Reconcile: unchanged files are only stat'ed, grown ones caught up, unknown ones added, rows
/// of deleted files dropped.
#[test]
fn reconcile_brings_the_index_in_line_with_the_files() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let (a, b) = (root.join("--w--/a.jsonl"), root.join("--w--/b.jsonl"));
    append(&a, &[header("a", "/w"), user("1", "x", 1)]);
    append(&b, &[header("b", "/w"), user("1", "y", 2)]);
    let s = ready(&root).unwrap();
    assert_eq!(reconcile(&s).unwrap(), Reconciled { files: 2, ..Default::default() }, "nothing to do");
    append(&a, &[user("2", "more", 3)]); // written by a mare that crashed before indexing it
    std::fs::remove_file(&b).unwrap();
    let c = root.join("--v--/c.jsonl");
    append(&c, &[header("c", "/v")]);
    let r = reconcile(&s).unwrap();
    assert_eq!((r.files, r.updated, r.removed), (2, 2, 1));
    let ix = s.lock().unwrap();
    let rows = all(&ix);
    assert_eq!(ids(&rows), ["a", "c"]);
    assert_eq!(rows[0].message_count, 2);
}

#[cfg(unix)]
#[test]
fn reconcile_does_not_catch_up_across_an_atomically_replaced_session() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("aaaa", "/old"), user("1", "first", 1)]);
    let s = ready(&root).unwrap();
    let before = all(&s.lock().unwrap());
    assert_eq!(ids(&before), ["aaaa"]);
    assert_eq!(before[0].indexed_len as u64, std::fs::metadata(&path).unwrap().len());

    // Keep the old prefix's byte length while replacing its id, then add a longer tail. A
    // watermark-only catch-up reads the tail and incorrectly combines it with the old row.
    let replacement = path.with_extension("replacement");
    append(
        &replacement,
        &[
            header("bbbb", "/new"),
            user("1", "first", 1),
            json!({
                "type":"message",
                "entry_id":"2",
                "created_at_ms":2,
                "message":{"role":"assistant","model_id":"new-model","provider_id":"new-provider","usage":{"input":7,"output":9},"content":[]}
            }),
        ],
    );
    assert!(std::fs::metadata(&replacement).unwrap().len() > before[0].indexed_len as u64);
    std::fs::rename(&replacement, &path).unwrap();

    let reconciled = reconcile(&s).unwrap();
    assert_eq!(reconciled.updated, 1);
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["bbbb"]);
    assert_eq!(rows[0].cwd, "/new");
    assert_eq!(rows[0].message_count, 2);
    assert_eq!(rows[0].model.as_deref(), Some("new-model"));
    assert_eq!(rows[0].provider.as_deref(), Some("new-provider"));
    assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (7, 9));
}

#[cfg(unix)]
#[test]
fn reconcile_retries_when_the_source_is_replaced_during_catch_up() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("aaaa", "/old"), user("1", "first", 1)]);
    let s = ready(&root).unwrap();
    append(&path, &[user("2", "append that triggers catch-up", 2)]);

    let replacement = path.with_extension("replacement");
    append(
        &replacement,
        &[
            header("bbbb", "/new"),
            user("1", "first", 1),
            json!({
                "type":"message",
                "entry_id":"2",
                "created_at_ms":2,
                "message":{"role":"assistant","model_id":"new-model","provider_id":"new-provider","usage":{"input":7,"output":9},"content":[]}
            }),
        ],
    );

    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    scan::after_next_read_from_open(move || {
        opened_tx.send(()).unwrap();
        release_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    });
    let source = path.clone();
    let replacer = std::thread::spawn(move || {
        opened_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        std::fs::rename(&replacement, &source).unwrap();
        release_tx.send(()).unwrap();
    });

    let reconciled = reconcile(&s).unwrap();
    replacer.join().unwrap();
    assert_eq!(reconciled.updated, 1);
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["bbbb"]);
    assert_eq!(rows[0].message_count, 2);
    assert_eq!(rows[0].cwd, "/new");
    assert_eq!(rows[0].model.as_deref(), Some("new-model"));
}

#[cfg(unix)]
#[test]
fn reconcile_detects_an_aba_replacement_with_the_same_id_length_and_mtime() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("same-id", "/w"), user("1", "same body", 1)]);
    let original = std::fs::read(&path).unwrap();
    let original_stamp = crate::stamp(&path).unwrap();
    let original_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let s = ready(&root).unwrap();
    let initial = all(&s.lock().unwrap()).remove(0);
    let checkpoint = checkpoint(&path);
    s.lock().unwrap().set_checkpoint(&path, &checkpoint).unwrap();

    // The path changes A -> B -> A, and the final A has the original byte length and mtime but
    // a new inode. The indexed file identity must distinguish this from an unchanged file.
    let between = path.with_extension("between");
    let replacement = path.with_extension("replacement");
    let final_a = path.with_extension("final");
    append(&replacement, &[header("other-id", "/other"), user("1", "other body", 2)]);
    std::fs::rename(&path, &between).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    std::fs::rename(&path, &replacement).unwrap();
    std::fs::write(&final_a, original).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&final_a).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(original_modified)).unwrap();
    std::fs::rename(&final_a, &path).unwrap();

    let final_stamp = crate::stamp(&path).unwrap();
    assert_eq!(final_stamp.len, original_stamp.len);
    assert_eq!(final_stamp.mtime_ns, original_stamp.mtime_ns);
    assert_ne!((final_stamp.dev, final_stamp.ino), (original_stamp.dev, original_stamp.ino));

    let reconciled = reconcile(&s).unwrap();
    assert_eq!(reconciled.updated, 1);
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["same-id"]);
    assert_eq!(rows[0].seq, initial.seq, "same logical session keeps its order");
    assert_eq!(s.lock().unwrap().checkpoint(&path).unwrap(), None, "the old file's checkpoint is invalid");
}

#[cfg(unix)]
#[test]
fn replacing_a_path_with_an_existing_id_removes_its_old_row() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let old_path = root.join("--w--/old.jsonl");
    let canonical_path = root.join("--w--/canonical.jsonl");
    append(&old_path, &[header("old-id", "/w"), user("1", "old", 1)]);
    append(&canonical_path, &[header("new-id", "/w"), user("1", "canonical", 2)]);
    let s = ready(&root).unwrap();

    let replacement = old_path.with_extension("replacement");
    append(&replacement, &[header("new-id", "/w"), user("1", "duplicate", 3)]);
    std::fs::rename(&replacement, &old_path).unwrap();
    let reconciled = reconcile(&s).unwrap();
    assert_eq!((reconciled.updated, reconciled.removed), (0, 1));

    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["new-id"]);
    assert_eq!(rows[0].path, norm(&canonical_path));
    assert_eq!(s.lock().unwrap().count().unwrap(), 1);

    // A second physical copy of an already-indexed ID also stays out of the unique-ID index.
    append(&old_path, &[header("new-id", "/w"), user("1", "duplicate", 3)]);
    let reconciled = reconcile(&s).unwrap();
    assert_eq!((reconciled.updated, reconciled.removed), (0, 0));
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["new-id"]);
    assert_eq!(rows[0].path, norm(&canonical_path));
    assert_eq!(s.lock().unwrap().count().unwrap(), 1);
}

#[cfg(unix)]
#[test]
fn list_checked_reloads_after_a_duplicate_id_removes_the_requested_row() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let changed = root.join("--w--/changed.jsonl");
    let canonical = root.join("--w--/canonical.jsonl");
    append(&changed, &[header("old-id", "/w"), user("1", "old", 1)]);
    append(&canonical, &[header("new-id", "/w"), user("1", "canonical", 2)]);
    let mut index = Index::open(&root).unwrap();
    index.ensure_built().unwrap();

    let replacement = changed.with_extension("replacement");
    append(&replacement, &[header("new-id", "/w"), user("1", "duplicate", 3)]);
    std::fs::rename(&replacement, &changed).unwrap();

    let query = Query { kinds: Kinds::Any, session_id: Some("old-id".into()), ..Default::default() };
    let (rows, total) = index.list_checked(&query).unwrap();
    assert!(rows.is_empty(), "list_checked must rerun the query after the update deletes its old row");
    assert_eq!(total, 0);
    assert_eq!(ids(&all(&index)), ["new-id"]);
}

#[cfg(unix)]
#[test]
fn duplicate_resolution_checks_the_other_paths_current_header() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let changed = root.join("--w--/z-changed.jsonl");
    let prior_duplicate = root.join("--w--/a-prior.jsonl");
    append(&changed, &[header("old-id", "/w"), user("1", "old", 1)]);
    append(&prior_duplicate, &[header("new-id", "/w"), user("1", "prior", 2)]);
    let s = ready(&root).unwrap();

    // Reconcile visits z-changed first. The row at a-prior still has new-id, while its current
    // file has already changed to other-id. Validate the duplicate path before suppressing z.
    let changed_replacement = changed.with_extension("replacement");
    append(&changed_replacement, &[header("new-id", "/w"), user("1", "new", 3)]);
    std::fs::rename(&changed_replacement, &changed).unwrap();
    let prior_replacement = prior_duplicate.with_extension("replacement");
    append(&prior_replacement, &[header("other-id", "/w"), user("1", "other", 4)]);
    std::fs::rename(&prior_replacement, &prior_duplicate).unwrap();

    let reconciled = reconcile(&s).unwrap();
    assert_eq!((reconciled.updated, reconciled.removed), (2, 1));
    let rows = all(&s.lock().unwrap());
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.session_id == "new-id" && row.path == norm(&changed)));
    assert!(rows.iter().any(|row| row.session_id == "other-id" && row.path == norm(&prior_duplicate)));
}

#[cfg(unix)]
#[test]
fn rebuilding_does_not_transfer_a_checkpoint_or_writer_metadata_to_a_new_id() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("old-id", "/header-old"), user("1", "old", 1)]);
    let s = ready(&root).unwrap();
    s.lock().unwrap().update(&path, Some("/writer-override")).unwrap();
    s.lock().unwrap().set_checkpoint(&path, &checkpoint(&path)).unwrap();

    let replacement = path.with_extension("replacement");
    append(&replacement, &[header("new-id", "/header-new"), user("1", "new", 2)]);
    std::fs::rename(&replacement, &path).unwrap();
    assert_eq!(s.lock().unwrap().rebuild().unwrap(), 1);

    let ix = s.lock().unwrap();
    let rows = all(&ix);
    assert_eq!(ids(&rows), ["new-id"]);
    assert_eq!(rows[0].cwd, "/header-new");
    assert_eq!(ix.checkpoint(&path).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn a_path_checkpoint_is_rechecked_against_file_identity_before_prefix_validation() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("same-id", "/w"), user("1", "old valid row", 1)]);
    let original = std::fs::read(&path).unwrap();
    let shared = ready(&root).unwrap();

    let mut writer = crate::SessionFile::open_existing(&path, None).unwrap();
    let checkpoint = writer.checkpoint().unwrap().unwrap();
    drop(writer);
    shared.lock().unwrap().set_checkpoint(&path, &checkpoint).unwrap();
    let indexed_checkpoint = shared.lock().unwrap().checkpoint(&path).unwrap().unwrap();
    assert_eq!(indexed_checkpoint, checkpoint);

    // The checkpoint ends at a newline in both files. An old path-only lookup skips this bad
    // prefix because the replacement is longer, even though it has a different physical file.
    let header_len = original.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let old_tail_len = checkpoint.len as usize - header_len;
    let mut replacement_bytes = original[..header_len].to_vec();
    replacement_bytes.extend(vec![b'x'; old_tail_len - 1]);
    replacement_bytes.push(b'\n');
    replacement_bytes.extend_from_slice(b"{\"type\":\"name\",\"name\":\"valid tail\"}\n");
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, replacement_bytes).unwrap();
    std::fs::rename(&replacement, &path).unwrap();

    assert!(shared.lock().unwrap().checkpoint(&path).unwrap().is_none());
    let error = match crate::SessionFile::open_existing(&path, Some(&indexed_checkpoint)) {
        Ok(_) => panic!("a checkpoint from the replaced file must not skip its malformed prefix"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(crate::CORRUPT), "{error}");
}

#[cfg(unix)]
#[test]
fn update_catches_up_handles_rename_and_allows_concurrent_read_before_commit() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("sess-1", "/w"), user("1", "initial", 1)]);

    let s = ready(&root).unwrap();
    // 1. Incremental catch-up appends cleanly.
    append(&path, &[user("2", "followup", 2)]);
    let row = s.lock().unwrap().update(&path, None).unwrap().unwrap();
    assert_eq!(row.message_count, 2);
    assert_eq!(row.last_input.as_deref(), Some("followup"));

    // 2. While `update` is scanning/parsing outside `TransactionBehavior::Immediate`, a concurrent
    // reader connection can read SQLite without blocking and a concurrent rename replacement causes
    // a clean retry/update to the replacement file.
    let replacement = path.with_extension("replacement");
    append(&replacement, &[header("sess-2", "/w2"), user("1", "replaced", 3)]);

    let db_path = root.join(crate::index::DB_NAME);
    let source = path.clone();
    let lock_free = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lock_free_hook = lock_free.clone();
    scan::after_next_read_from_open(move || {
        // Acquiring and rolling back BEGIN IMMEDIATE with busy_timeout(0) proves the first
        // updater is NOT holding TransactionBehavior::Immediate while scanning/parsing the file.
        let mut conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.busy_timeout(std::time::Duration::from_millis(0)).unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        tx.rollback().unwrap();
        lock_free_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::rename(&replacement, &source).unwrap();
    });

    append(&path, &[user("3", "trigger-rescan", 4)]);
    let updated = s.lock().unwrap().update(&path, None).unwrap().unwrap();
    assert!(lock_free.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(updated.session_id, "sess-2");
    assert_eq!(updated.last_input.as_deref(), Some("replaced"));
}

#[test]
fn rebuild_skips_file_with_leading_blank_line_and_builds_valid_sessions() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let valid = root.join("--w--/valid.jsonl");
    let blank_first = root.join("--w--/leading-blank.jsonl");
    append(&valid, &[header("valid-sess", "/w"), user("1", "hello", 1)]);
    std::fs::create_dir_all(blank_first.parent().unwrap()).unwrap();
    std::fs::write(&blank_first, format!("\n{}\n", header("ignored-sess", "/w"))).unwrap();

    let s = ready(&root).unwrap();
    let mut ix = s.lock().unwrap();
    let built = ix.rebuild().unwrap();
    assert_eq!(built, 1);
    assert_eq!(ids(&all(&ix)), ["valid-sess"]);
}

#[cfg(unix)]
#[test]
fn stale_none_parse_does_not_delete_restored_valid_row_across_connections() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    let backup = path.with_extension("backup");
    append(&path, &[header("orig-id", "/w"), user("1", "keep me", 1)]);

    let mut ix1 = Index::open(&root).unwrap();
    ix1.ensure_built().unwrap();
    let mut ix2 = Index::open(&root).unwrap();

    // Move valid file aside and temporarily replace `path` with a header lacking `session_id` (parse_full -> None).
    std::fs::rename(&path, &backup).unwrap();
    std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"cwd\":\"/w\"}\n").unwrap();

    let restore_path = path.clone();
    let backup_path = backup.clone();
    crate::index::before_next_none_delete(move || {
        // Restore the exact original file (same inode/stamp so conn2 keeps `old == row`) and run conn2.
        std::fs::rename(&backup_path, &restore_path).unwrap();
        let row2 = ix2.update(&restore_path, None).unwrap().unwrap();
        assert_eq!(row2.session_id, "orig-id");
    });

    let row1 = ix1.update(&path, None).unwrap().unwrap();
    assert_eq!(row1.session_id, "orig-id");
    let rows = all(&ix1);
    assert_eq!(ids(&rows), ["orig-id"]);
    assert_eq!(rows[0].last_input.as_deref(), Some("keep me"));
}
