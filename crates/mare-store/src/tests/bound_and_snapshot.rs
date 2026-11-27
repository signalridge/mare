use super::*;
use serde_json::json;

#[cfg(unix)]
#[test]
fn readonly_bound_read_keeps_original_through_path_aba_and_refuses_changes() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("view.jsonl");
    let old = dir.path().join("old.jsonl");
    let forged = dir.path().join("forged.jsonl");
    std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"session_id\":\"original\"}\n").unwrap();
    std::fs::write(&forged, "{\"type\":\"session\",\"version\":3,\"session_id\":\"forged\"}\n").unwrap();
    let (a, b, c) = (path.clone(), old.clone(), forged.clone());
    crate::entries::BEFORE_BOUND_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &b).unwrap();
            std::fs::rename(&c, &a).unwrap();
        }))
    });
    let (a, b, c) = (path.clone(), old.clone(), forged.clone());
    crate::entries::AFTER_BOUND_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &c).unwrap();
            std::fs::rename(&b, &a).unwrap();
        }))
    });
    assert_eq!(read_entries_bound(&path, false).unwrap().0[0]["session_id"], "original");
    let changed = path.clone();
    crate::entries::AFTER_BOUND_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            writeln!(std::fs::OpenOptions::new().append(true).open(changed).unwrap(), "{{\"type\":\"name\"}}").unwrap();
        }))
    });
    assert!(read_entries_bound(&path, false).unwrap_err().to_string().contains("changed"));
}

#[test]
fn readonly_bound_read_validates_header_and_leniently_skips_damaged_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"session\",\"version\":3,\"session_id\":\"s\"}\ninvalid\n{\"type\":\"name\",\"name\":\"ok\"}\n{\"torn\"",
    )
    .unwrap();
    assert!(read_entries_bound(&path, false).is_err());
    let (entries, skipped) = read_entries_bound(&path, true).unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1]["name"], "ok");
    std::fs::write(&path, "{\"type\":\"session\",\"version\":99,\"session_id\":\"s\"}\n").unwrap();
    assert!(read_entries_bound(&path, true).is_err());
}

#[test]
fn writer_matches_equivalent_spelling_but_not_hardlinks_or_replaced_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut writer = SessionFile::open(&path).unwrap();
    writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();
    let alternate = std::path::PathBuf::from(format!("{}/./session.jsonl", dir.path().display()));
    assert!(writer.matches_path(&alternate), "different spelling of the same lock path must match");
    let alias = dir.path().join("hardlink.jsonl");
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(!writer.matches_path(&alias), "a different lock name must not reuse this writer");
    std::fs::rename(&path, dir.path().join("old.jsonl")).unwrap();
    std::fs::write(&path, SESSION).unwrap();
    assert!(!writer.matches_path(&path), "a replacement file must not reuse the old handle");
}

#[cfg(unix)]
#[test]
fn writer_does_not_match_a_replaced_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut writer = SessionFile::open(&path).unwrap();
    writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();
    let lock = lock_path(&path);
    std::fs::rename(&lock, dir.path().join("old.lock")).unwrap();
    std::fs::write(&lock, "replacement").unwrap();
    assert!(!writer.matches_path(&path), "a different lock inode must not reuse the writer");
}

#[cfg(unix)]
#[test]
fn checkpoint_refuses_replacement_even_with_the_same_length() {
    let dir = tempfile::tempdir().unwrap();
    for replace_lock in [false, true] {
        let path = dir.path().join(format!("session-{replace_lock}.jsonl"));
        let mut writer = SessionFile::open(&path).unwrap();
        writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();
        let victim = if replace_lock { lock_path(&path) } else { path.clone() };
        let bytes = std::fs::read(&victim).unwrap();
        std::fs::rename(&victim, victim.with_extension("moved")).unwrap();
        std::fs::write(&victim, bytes).unwrap();
        assert!(writer.checkpoint().unwrap().is_none());
    }
}

#[cfg(unix)]
#[test]
fn snapshot_reads_the_held_file_through_a_path_replacement_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let moved = dir.path().join("original.jsonl");
    let replacement = dir.path().join("replacement.jsonl");
    let mut writer = SessionFile::open(&path).unwrap();
    writer.append(&json!({"type":"session","version":3,"session_id":"original"}), true).unwrap();
    std::fs::write(&replacement, "{\"type\":\"session\",\"version\":3,\"session_id\":\"forged\"}\n").unwrap();
    let (a, b, c) = (path.clone(), moved.clone(), replacement.clone());
    crate::session_file::BEFORE_SNAPSHOT_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &b).unwrap();
            std::fs::rename(&c, &a).unwrap();
        }))
    });
    let (a, b, c) = (path.clone(), moved.clone(), replacement.clone());
    crate::session_file::AFTER_SNAPSHOT_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &c).unwrap();
            std::fs::rename(&b, &a).unwrap();
        }))
    });
    let entries = writer.read_snapshot(None).unwrap();
    assert_eq!(entries[0]["session_id"], "original");
    assert!(writer.matches_path(&path));
    let (a, b, c) = (path.clone(), moved.clone(), replacement.clone());
    crate::session_file::BEFORE_SNAPSHOT_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &b).unwrap();
            std::fs::rename(&c, &a).unwrap();
        }))
    });
    let (a, b, c) = (path.clone(), moved.clone(), replacement.clone());
    crate::session_file::AFTER_SNAPSHOT_READ.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &c).unwrap();
            std::fs::rename(&b, &a).unwrap();
        }))
    });
    let (tail, header, stamp) = writer.read_tail_snapshot().unwrap();
    assert_eq!(header["session_id"], "original");
    assert_eq!(tail.entries[0]["session_id"], "original");
    assert_eq!(writer.read_snapshot(Some(stamp)).unwrap()[0]["session_id"], "original");
}

#[test]
fn detached_reads_detect_concurrent_append_and_keep_session_locked_after_writer_drop() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    use std::sync::mpsc;
    use std::time::Duration;

    for tail in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut writer = SessionFile::create_new(&path).unwrap();
        writer.append(&json!({"type":"session","version":3,"session_id":"old"}), true).unwrap();
        let reader = writer.snapshot_reader().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            crate::session_file::BEFORE_SNAPSHOT_READ.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    ready_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(3));
                }));
            });
            if tail { reader.read_tail_snapshot().map(|_| ()) } else { reader.read_snapshot(None).map(|_| ()) }
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.append(&json!({"type":"state","state":"closed"}), true).unwrap();
        drop(writer);
        let reopen = SessionFile::open_existing(&path, None);
        assert!(reopen.is_err(), "detached reader must keep the lock");
        release_tx.send(()).unwrap();
        let error = task.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
        assert!(SessionFile::open_existing(&path, None).is_ok());
        let entries = crate::read_entries(&path).unwrap();
        assert_eq!(entries.last().unwrap()["state"], "closed");
    }
}

#[test]
fn detached_snapshot_rejects_concurrent_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut writer = SessionFile::create_new(&path).unwrap();
    writer.append(&json!({"type":"session","version":3,"session_id":"old"}), true).unwrap();
    let reader = writer.snapshot_reader().unwrap();
    let changed_path = path.clone();
    crate::session_file::BEFORE_SNAPSHOT_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::OpenOptions::new().write(true).open(changed_path).unwrap().set_len(0).unwrap();
        }));
    });
    assert!(reader.read_snapshot(None).is_err());
}

#[test]
fn held_tail_revision_refuses_a_file_changed_before_full_read_or_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut writer = SessionFile::open(&path).unwrap();
    writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();
    let (_, _, stamp) = writer.read_tail_snapshot().unwrap();
    writer.append(&json!({"type":"message","entry_id":"later"}), true).unwrap();
    assert!(!writer.matches_snapshot(stamp));
    assert!(writer.read_snapshot(Some(stamp)).unwrap_err().to_string().contains("changed"));
}

#[test]
fn replacement_after_validation_is_never_opened_or_created_over() {
    use crate::session_file::AFTER_VALIDATION;
    let dir = tempfile::tempdir().unwrap();
    for exists in [false, true] {
        let p = dir.path().join(format!("{exists}.jsonl"));
        if exists {
            std::fs::write(&p, SESSION).unwrap();
        }
        let future = "{\"type\":\"session\",\"version\":99,\"session_id\":\"future\"}\n";
        let path = p.clone();
        AFTER_VALIDATION.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let replacement = path.with_extension("replacement");
                std::fs::write(&replacement, future).unwrap();
                std::fs::rename(replacement, &path).unwrap();
            }))
        });
        assert!(SessionFile::open(&p).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), future);
    }
}

#[cfg(unix)]
#[test]
fn open_validates_the_file_it_holds_through_path_aba() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let moved = dir.path().join("original.jsonl");
    let replacement = dir.path().join("replacement.jsonl");
    for original in ["{\"type\":\"wrong\",\"version\":2}\n{}\n".to_string(), format!("{SESSION}invalid\n")] {
        std::fs::write(&path, original).unwrap();
        std::fs::write(&replacement, format!("{SESSION}{{}}\n")).unwrap();
        let (a, b, c) = (path.clone(), moved.clone(), replacement.clone());
        crate::session_file::BEFORE_OPEN_VALIDATION.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&a, &b).unwrap();
                std::fs::rename(&c, &a).unwrap();
            }))
        });
        let (a, b, c) = (path.clone(), moved.clone(), replacement.clone());
        crate::session_file::AFTER_OPEN_VALIDATION.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&a, &c).unwrap();
                std::fs::rename(&b, &a).unwrap();
            }))
        });
        assert!(SessionFile::open_existing(&path, None).is_err(), "corrupt original must not be opened");
    }
}

#[cfg(unix)]
#[test]
fn open_refuses_lock_replacement_after_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(&path, SESSION).unwrap();
    let lock = lock_path(&path);
    let moved = dir.path().join("old.lock");
    crate::session_file::AFTER_VALIDATION.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&lock, &moved).unwrap();
            std::fs::write(&lock, "replacement").unwrap();
        }))
    });
    assert!(SessionFile::open_existing(&path, None).is_err());
}

#[test]
fn append_rejects_a_replaced_session_path_without_modifying_either_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let saved = dir.path().join("saved.jsonl");
    std::fs::write(&path, SESSION).unwrap();
    let mut writer = SessionFile::open(&path).unwrap();
    std::fs::rename(&path, &saved).unwrap();
    std::fs::write(&path, SESSION.replace("\\\"s\\\"", "\\\"replacement\\\"")).unwrap();
    let before = std::fs::read(&saved).unwrap();
    let replacement = std::fs::read(&path).unwrap();
    let error = writer.append_batch(&[serde_json::json!({"type":"name","name":"must reject"})], true).unwrap_err();
    assert!(error.to_string().contains("identity changed"), "{error}");
    assert_eq!(std::fs::read(&saved).unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), replacement);
}

#[test]
fn replacement_during_sync_refuses_ack_and_never_writes_replacement_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let saved = dir.path().join("saved.jsonl");
    std::fs::write(&path, SESSION).unwrap();
    let mut writer = SessionFile::open(&path).unwrap();
    let (a, b) = (path.clone(), saved.clone());
    crate::session_file::BEFORE_DURABLE_SYNC.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&a, &b).unwrap();
            std::fs::write(&a, SESSION).unwrap();
        }));
    });
    let error = writer.append_batch(&[serde_json::json!({"type":"name","name":"unconfirmed"})], true).unwrap_err();
    assert!(error.to_string().contains("identity changed"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), SESSION);
    assert!(!crate::lock::rollback_path(&path).exists(), "must not publish rollback intent for replacement");
    assert!(writer.flush().is_err(), "a replaced file must never be acknowledged by a later flush");
    assert!(writer.matches_path_checked(&path).is_err(), "a poisoned writer is not safely detached for retirement");
}

#[cfg(unix)]
#[test]
fn checked_writer_binding_distinguishes_detachment_from_identity_io_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut writer = SessionFile::open(&path).unwrap();
    writer.append(&json!({"type":"session","version":3,"session_id":"s"}), true).unwrap();
    assert!(writer.matches_path_checked(&path).unwrap());
    assert!(!writer.matches_path_checked(&dir.path().join("missing.jsonl")).unwrap());
    let invalid_parent = path.join("child.jsonl");
    assert_eq!(writer.matches_path_checked(&invalid_parent).unwrap_err().kind(), std::io::ErrorKind::NotADirectory);
    assert!(!writer.matches_path(&invalid_parent), "ordinary guards remain fail closed");
    writer.append(&json!({"type":"name","name":"still usable"}), true).unwrap();
}

#[cfg(unix)]
#[test]
fn directory_sync_reports_missing_or_invalid_parent() {
    let temp = tempfile::tempdir().unwrap();
    assert!(crate::sync_dir(&temp.path().join("missing").join("journal.jsonl")).is_err());
    std::fs::write(temp.path().join("file"), b"not a directory").unwrap();
    assert!(crate::sync_dir(&temp.path().join("file").join("journal.jsonl")).is_err());
    crate::sync_dir(&temp.path().join("journal.jsonl")).unwrap();
}
