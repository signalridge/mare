use super::*;
use serde_json::json;

#[test]
fn delete_takes_the_lock_and_removes_everything_of_the_session() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("2026-01-01T00-00-00_abcd.jsonl");
    std::fs::write(&p, "{\"type\":\"session\",\"session_id\":\"S1\",\"version\":3}\n").unwrap();
    let att = dir.path().join("2026-01-01T00-00-00_abcd.attachments");
    std::fs::create_dir_all(&att).unwrap();
    std::fs::write(att.join("x.png"), "img").unwrap();
    let snaps = dir.path().join("2026-01-01T00-00-00_abcd.jsonl.snapshots");
    std::fs::create_dir_all(snaps.join("blobs")).unwrap();
    // sub-agent sessions: a run of only ours goes whole; a shared run keeps the others'
    let mine = dir.path().join("agents/run1");
    let shared = dir.path().join("agents/run2");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let child = |d: &Path, n: &str, parent: &str| {
        std::fs::write(
            d.join(n),
            format!(
                "{{\"type\":\"session\",\"version\":3,\"session_id\":\"{n}\",\"parent_session_id\":\"{parent}\"}}\n"
            ),
        )
        .unwrap()
    };
    child(&mine, "000-a.jsonl", "S1");
    child(&shared, "000-b.jsonl", "S1");
    child(&shared, "001-c.jsonl", "OTHER");
    // locked by another holder (stands in for another process): refused, nothing removed
    let other = File::open(lock_path(&p)).or_else(|_| File::create(lock_path(&p))).unwrap();
    other.try_lock().unwrap();
    let err = delete_session(&p).unwrap_err().to_string();
    assert!(err.contains(LOCKED), "{err}");
    assert!(p.exists() && att.exists() && mine.exists());
    drop(other);
    delete_session(&p).unwrap();
    for gone in [p.clone(), lock_path(&p), att, snaps, mine, shared.join("000-b.jsonl")] {
        assert!(!gone.exists(), "{} left behind", gone.display());
    }
    assert!(shared.join("001-c.jsonl").exists(), "another session's sub-agent stays");
    assert!(delete_session(&p).unwrap_err().to_string().contains(NOT_FOUND));
}

#[test]
fn deleting_a_session_rejects_non_sessions_and_preserves_unrelated_child_files() {
    let dir = tempfile::tempdir().unwrap();
    let ordinary = dir.path().join("ordinary.txt");
    std::fs::write(&ordinary, "keep me").unwrap();
    assert!(delete_session(&ordinary).is_err());
    assert_eq!(std::fs::read_to_string(&ordinary).unwrap(), "keep me");
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, "{\"type\":\"session\",\"version\":3,\"session_id\":\"S\"}\n").unwrap();
    let run = dir.path().join("agents/run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(
        run.join("000-child.jsonl"),
        "{\"type\":\"session\",\"version\":3,\"session_id\":\"C\",\"parent_session_id\":\"S\"}\n",
    )
    .unwrap();
    std::fs::write(run.join("unrelated.txt"), "keep me too").unwrap();
    std::fs::write(run.join("unrelated.jsonl"), "{\"type\":\"other\",\"parent_session_id\":\"S\"}\n").unwrap();
    delete_session(&session).unwrap();
    assert!(!run.join("000-child.jsonl").exists());
    assert_eq!(std::fs::read_to_string(run.join("unrelated.txt")).unwrap(), "keep me too");
    assert!(run.join("unrelated.jsonl").exists(), "a non-session record is not a child session");
}

#[cfg(unix)]
#[test]
fn deleting_a_session_does_not_follow_an_agents_directory_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    std::fs::write(&session, "{\"type\":\"session\",\"version\":3,\"session_id\":\"S\"}\n").unwrap();
    let run = outside.path().join("run");
    std::fs::create_dir(&run).unwrap();
    let child = run.join("child.jsonl");
    std::fs::write(&child, "{\"type\":\"session\",\"version\":3,\"session_id\":\"C\",\"parent_session_id\":\"S\"}\n")
        .unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("agents")).unwrap();
    delete_session(&session).unwrap();
    assert!(child.exists(), "an agents symlink must not make outside files part of the session");
}

#[test]
fn slug() {
    assert_eq!(cwd_slug(Path::new("/Users/me/my proj")), "--Users-me-my-proj--");
}

#[test]
fn deletion_refuses_a_parent_with_an_open_child_transcript() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent.jsonl");
    let child = dir.path().join("agents/run/001-child.jsonl");
    let mut p = SessionFile::open(&parent).unwrap();
    p.append(&json!({"type":"session", "version":CURRENT_VERSION, "session_id":"parent"}), true).unwrap();
    drop(p);
    let mut c = SessionFile::open(&child).unwrap();
    c.append(
        &json!({"type":"session", "version":CURRENT_VERSION, "session_id":"child", "parent_session_id":"parent"}),
        true,
    )
    .unwrap();
    let err = delete_session(&parent).unwrap_err().to_string();
    assert!(err.contains(OPEN_HERE), "{err}");
    assert!(parent.exists() && child.exists(), "refusal leaves both files intact");
    c.append(&json!({"type":"state", "state":"closed"}), true).unwrap();
    drop(c);
    delete_session(&parent).unwrap();
    assert!(!parent.exists() && !child.exists());
}

#[test]
fn deletion_refuses_a_child_created_after_its_snapshot() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent.jsonl");
    let child = dir.path().join("agents/run/001-late.jsonl");
    let mut p = SessionFile::open(&parent).unwrap();
    p.append(&json!({"type":"session", "version":CURRENT_VERSION, "session_id":"parent"}), true).unwrap();
    drop(p);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::rc::Rc::new(std::cell::RefCell::new(None));
    let child_for_hook = child.clone();
    let worker_for_hook = worker.clone();
    crate::files::AFTER_CHILD_SNAPSHOT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            // A separate writer disregards the parent lock and starts after the snapshot.
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let join = std::thread::spawn(move || {
                let mut f = SessionFile::open(&child_for_hook).unwrap();
                f.append(
                    &json!({"type":"session", "version":CURRENT_VERSION, "session_id":"late", "parent_session_id":"parent"}),
                    true,
                )
                .unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                drop(f);
            });
            ready_rx.recv().unwrap();
            *worker_for_hook.borrow_mut() = Some(join);
        }));
    });
    let error = delete_session(&parent).unwrap_err().to_string();
    assert!(error.contains("child sessions changed"), "{error}");
    assert!(parent.exists() && child.exists(), "no file was removed");
    release_tx.send(()).unwrap();
    worker.borrow_mut().take().unwrap().join().unwrap();
    delete_session(&parent).unwrap();
    assert!(!parent.exists() && !child.exists());
}

#[test]
fn unsupported_child_is_locked_before_deleting_parent_and_all_backups() {
    let _serial = crate::FORK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent.jsonl");
    let child = dir.path().join("agents/r/child.jsonl");
    std::fs::create_dir_all(child.parent().unwrap()).unwrap();
    std::fs::write(&parent, SESSION).unwrap();
    let body = "{\"type\":\"session\",\"session_id\":\"c\",\"parent_session_id\":\"s\",\"version\":99}\n";
    std::fs::write(&child, body).unwrap();
    let unrelated = dir.path().join("parent.jsonl.v1.bak");
    std::fs::write(&unrelated, "keep").unwrap();
    let held = lock_session(&child).unwrap();
    assert!(delete_session(&parent).is_err(), "an active child still prevents deletion");
    assert_eq!(std::fs::read_to_string(&parent).unwrap(), SESSION);
    assert_eq!(std::fs::read_to_string(&child).unwrap(), body);
    assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "keep");
    drop(held);
    delete_session(&parent).unwrap();
    assert!(!parent.exists() && !child.exists());
    assert!(unrelated.exists(), "legacy .v<N>.bak migration backups are no longer managed session files");
}
