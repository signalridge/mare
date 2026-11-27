use super::*;

#[test]
fn a_cancelled_append_rollback_is_rebuilt_before_the_retry_is_indexed() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    append(&path, &[header("session", "/w"), user("1", "first", 1)]);
    let original = std::fs::read(&path).unwrap();
    let s = ready(&root).unwrap();
    s.lock().unwrap().set_checkpoint(&path, &checkpoint(&path)).unwrap();

    append(&path, &[user("2", "cancelled", 2)]);
    s.lock().unwrap().update(&path, None).unwrap();
    assert_eq!(all(&s.lock().unwrap())[0].message_count, 2);

    // Model the writer truncating an uncommitted/cancelled append back to its previous boundary.
    std::fs::write(&path, original).unwrap();
    s.lock().unwrap().update(&path, None).unwrap();
    let ix = s.lock().unwrap();
    let row = all(&ix).remove(0);
    assert_eq!(row.message_count, 1);
    assert_eq!(row.last_input.as_deref(), Some("first"));
    assert_eq!(ix.checkpoint(&path).unwrap(), None, "rollback invalidates the append-era checkpoint");
    drop(ix);

    append(&path, &[user("2", "retried", 3)]);
    s.lock().unwrap().update(&path, None).unwrap();
    let rows = all(&s.lock().unwrap());
    assert_eq!(rows[0].message_count, 2, "the retried record is counted once");
    assert_eq!(rows[0].last_input.as_deref(), Some("retried"));
}

#[test]
fn reconcile_catches_a_retry_after_indexing_an_in_flight_batch() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    let mut writer = crate::SessionFile::open(&path).unwrap();
    writer.append_batch(&[header("session", "/w"), user("1", "first", 1)], true).unwrap();
    let s = ready(&root).unwrap();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let source = path.clone();
    let writer_thread = std::thread::spawn(move || {
        crate::session_file::BEFORE_ROLLBACK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            }));
        });
        crate::session_file::FAIL_NEXT_SYNC.with(|fail| fail.set(true));
        assert!(writer.append_batch(&[user("2", "cancelled", 2)], true).is_err());
        // Retry immediately after the rollback, without an intervening reconcile.
        writer
            .append_batch(
                &[
                    user("2", "retried??", 2),
                    json!({
                        "type":"message",
                        "entry_id":"3",
                        "created_at_ms":3,
                        "message":{"role":"assistant","model_id":"retry-model","provider_id":"retry-provider","usage":{"input":7,"output":9},"content":[]}
                    }),
                ],
                true,
            )
            .unwrap();
    });

    // Pause the failed append after its complete bytes are visible but before truncation. A
    // real concurrent index catch-up advances beyond the writer's rollback point.
    entered_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    let indexed = s.lock().unwrap().update(&path, None).unwrap().unwrap();
    assert_eq!(indexed.last_input.as_deref(), Some("cancelled"));
    release_tx.send(()).unwrap();
    writer_thread.join().unwrap();

    // The retry reuses the cancelled record's byte length, making the stale watermark a valid
    // line boundary again, and appends farther beyond it.
    let bytes = std::fs::read(&source).unwrap();
    assert_eq!(scan::rollback_revision(&source).unwrap().2, 2, "only the failed batch advances the rollback revision");
    assert!(bytes.len() > indexed.indexed_len as usize);
    assert_eq!(bytes[indexed.indexed_len as usize - 1], b'\n');

    let reconciled = reconcile(&s).unwrap();
    assert_eq!(reconciled.updated, 1);
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["session"]);
    assert_eq!(rows[0].message_count, 3);
    assert_eq!(rows[0].last_input.as_deref(), Some("retried??"));
    assert_eq!(rows[0].model.as_deref(), Some("retry-model"));
    assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (7, 9));
}

#[test]
fn reconcile_reports_a_live_rollback_as_pending_until_the_writer_finishes() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    let mut initial = crate::SessionFile::open(&path).unwrap();
    initial.append_batch(&[header("session", "/w"), user("1", "first", 1)], true).unwrap();
    drop(initial);
    let s = ready(&root).unwrap();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let mut writer = crate::SessionFile::open(&writer_path).unwrap();
        crate::session_file::AFTER_ROLLBACK_TRUNCATE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            }));
        });
        crate::session_file::FAIL_NEXT_SYNC.with(|fail| fail.set(true));
        assert!(writer.append_batch(&[user("2", "cancelled", 2)], true).is_err());
    });

    entered_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    let during = reconcile(&s).unwrap();
    assert_eq!(during.pending, 1, "a picker can distinguish an in-flight rollback from a failed file");
    assert_eq!(during.failed, 0);
    release_tx.send(()).unwrap();
    writer.join().unwrap();

    let after = reconcile(&s).unwrap();
    assert_eq!(after.pending, 0);
    assert_eq!(after.failed, 0);
    let rows = all(&s.lock().unwrap());
    assert_eq!(rows[0].message_count, 1);
    assert_eq!(rows[0].last_input.as_deref(), Some("first"));
}

#[test]
fn reconcile_removes_a_first_batch_rolled_back_to_an_empty_file() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/new.jsonl");
    let mut writer = crate::SessionFile::open(&path).unwrap();
    let s = ready(&root).unwrap();
    let source_for_index = path.clone();
    let index_for_hook = s.clone();
    crate::session_file::BEFORE_ROLLBACK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            index_for_hook.lock().unwrap().update(&source_for_index, None).unwrap();
        }));
    });
    crate::session_file::FAIL_NEXT_SYNC.with(|fail| fail.set(true));
    assert!(writer.append_batch(&[header("new-session", "/w"), user("1", "cancelled", 1)], true).is_err());
    assert_eq!(ids(&all(&s.lock().unwrap())), ["new-session"], "the racing catch-up saw the batch before rollback");

    let reconciled = reconcile(&s).unwrap();
    assert_eq!((reconciled.updated, reconciled.removed), (0, 1));
    assert_eq!(s.lock().unwrap().count().unwrap(), 0);

    writer.append_batch(&[header("new-session", "/w"), user("1", "retried", 2)], true).unwrap();
    let reconciled = reconcile(&s).unwrap();
    assert_eq!(reconciled.updated, 1);
    let rows = all(&s.lock().unwrap());
    assert_eq!(ids(&rows), ["new-session"]);
    assert_eq!(rows[0].last_input.as_deref(), Some("retried"));
}

#[test]
fn reopen_recovers_a_rollback_interrupted_between_the_two_durable_marks() {
    let dir = OwnedTempDir::new();
    let root = dir.path().join("sessions");
    let path = root.join("--w--/session.jsonl");
    let mut writer = crate::SessionFile::open(&path).unwrap();
    writer.append_batch(&[header("session", "/w"), user("1", "first", 1)], true).unwrap();
    let s = ready(&root).unwrap();
    let source_for_index = path.clone();
    let index_for_hook = s.clone();
    crate::session_file::BEFORE_ROLLBACK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            index_for_hook.lock().unwrap().update(&source_for_index, None).unwrap();
        }));
    });
    let pending_path = path.clone();
    crate::session_file::AFTER_ROLLBACK_TRUNCATE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let error = scan::rollback_revision(&pending_path).unwrap_err().to_string();
            assert!(error.contains(crate::ROLLBACK_IN_PROGRESS), "{error}");
            std::panic::resume_unwind(Box::new("simulated process exit before the final revision sync"))
        }));
    });
    crate::session_file::FAIL_NEXT_SYNC.with(|fail| fail.set(true));

    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer.append_batch(&[user("2", "cancelled", 2)], true).unwrap();
    }));
    assert!(crashed.is_err(), "the crash seam fires after the truncated prefix was synced");
    drop(writer); // process exit releases the OS session lock; the durable revision remains pending/odd
    let pending = scan::rollback_revision(&path).unwrap_err().to_string();
    assert!(pending.contains(crate::ROLLBACK_IN_PROGRESS), "the index observes, but does not clear, pending state");

    // Reopening under the exclusive session lock completes the pending revision before retrying.
    let mut retry = crate::SessionFile::open(&path).unwrap();
    assert_eq!(scan::rollback_revision(&path).unwrap().2, 2, "only the journal reopen completes the revision");
    retry
        .append_batch(
            &[
                user("2", "retried??", 2),
                json!({
                    "type":"message",
                    "entry_id":"3",
                    "created_at_ms":3,
                    "message":{"role":"assistant","model_id":"retry-model","provider_id":"retry-provider","usage":{"input":7,"output":9},"content":[]}
                }),
            ],
            true,
        )
        .unwrap();
    drop(retry);

    let reconciled = reconcile(&s).unwrap();
    assert_eq!(reconciled.updated, 1);
    let rows = all(&s.lock().unwrap());
    assert_eq!(rows[0].message_count, 3);
    assert_eq!(rows[0].last_input.as_deref(), Some("retried??"));
    assert_eq!(rows[0].model.as_deref(), Some("retry-model"));
    assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (7, 9));
}

/// Child process for `failed_batch_is_truncated_after_a_process_exit_before_rollback_truncate`.
#[test]
fn rollback_crash_child() {
    let Some(path) = std::env::var_os("MARE_STORE_ROLLBACK_CRASH_PATH") else { return };
    let path = std::path::PathBuf::from(path);
    let mut writer = crate::SessionFile::open(&path).unwrap();
    crate::session_file::AFTER_ROLLBACK_MARKER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| std::process::exit(73)));
    });
    crate::session_file::FAIL_NEXT_SYNC.with(|fail| fail.set(true));
    writer.append(&json!({"type":"custom","kind":"agent/run-end","data":{"run_id":"failed"}}), true).unwrap();
    panic!("the child must exit after persisting rollback intent");
}

#[test]
fn failed_batch_is_truncated_after_a_process_exit_before_rollback_truncate() {
    let dir = OwnedTempDir::new();
    let path = dir.path().join("--w--/session.jsonl");
    let mut writer = crate::SessionFile::open(&path).unwrap();
    writer.append_batch(&[header("session", "/w"), user("1", "committed", 1)], true).unwrap();
    drop(writer);

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "index::tests::rollback::rollback_crash_child"])
        .env("MARE_STORE_ROLLBACK_CRASH_PATH", &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73), "child must exit at the durable rollback intent");
    assert!(crate::read_entries(&path).unwrap().iter().any(|entry| entry["kind"] == "agent/run-end"));
    assert!(
        scan::rollback_revision(&path).unwrap_err().to_string().contains(crate::ROLLBACK_IN_PROGRESS),
        "an interrupted rollback must stay unavailable to index readers"
    );

    let recovered = crate::SessionFile::open(&path).unwrap();
    drop(recovered);
    let entries = crate::read_entries(&path).unwrap();
    assert_eq!(entries.len(), 2, "reopen truncates the failed batch back to its saved start offset");
    assert!(!entries.iter().any(|entry| entry["kind"] == "agent/run-end"));
    assert!(!crate::lock::rollback_path(&path).exists());
}

/// An orphaned `.lock.rollback` marker with NO live writer holding `.jsonl.lock` must NOT
/// be reported as `pending` by `reconcile()` (which causes Lua `refresh.reconcile()` to spin
/// indefinitely), while still refusing to index the uncommitted file until next open.
#[test]
fn orphaned_rollback_marker_without_live_lock_is_not_counted_pending() {
    let dir = OwnedTempDir::new();
    let path = dir.path().join("--w--/orphaned.jsonl");
    let mut writer = crate::SessionFile::open(&path).unwrap();
    writer.append_batch(&[header("orphaned", "/w"), user("1", "committed", 1)], true).unwrap();
    drop(writer);

    // Drop a valid orphaned `.lock.rollback` marker without any live writer holding `.jsonl.lock`.
    let mut marker = Vec::with_capacity(32);
    marker.extend_from_slice(b"MRROLL01");
    marker.extend_from_slice(&[0u8; 24]);
    std::fs::write(crate::lock::rollback_path(&path), marker).unwrap();

    let s = shared(dir.path()).unwrap();
    let reconciled = reconcile(&s).unwrap();
    assert_eq!(
        reconciled.pending, 0,
        "orphaned .lock.rollback without live session lock must NOT be counted as pending: {reconciled:?}"
    );
    assert_eq!(reconciled.failed, 1, "orphaned .lock.rollback should be counted as failed: {reconciled:?}");
    // Uncommitted bytes must still NOT be indexed until SessionFile::open repairs the marker.
    assert!(scan::rollback_revision(&path).unwrap_err().to_string().contains(crate::ROLLBACK_IN_PROGRESS));
}
