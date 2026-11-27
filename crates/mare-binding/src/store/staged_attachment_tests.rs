#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mlua::{Lua, Table};

use mare_store::SessionFile;

use super::attachments::{
    StagingDir, attachment_dir, before_attachment_directory_sync, install_staged_attachments, write_staged_attachment,
};
use super::index::BEFORE_INDEX_WORK;
use super::install;
use super::session::{LuaSessionFile, SessionState, SnapshotRevision, read_session};

/// Session close can finish on a worker thread and populate the process index cache.
struct OwnedSessionDir {
    directory: Option<tempfile::TempDir>,
    blocking: crate::blocking::Scope,
}

impl OwnedSessionDir {
    fn new() -> Self {
        Self { directory: Some(tempfile::tempdir().unwrap()), blocking: crate::blocking::Scope::default() }
    }

    fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
}

impl Drop for OwnedSessionDir {
    fn drop(&mut self) {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(self.blocking.close_and_wait());
        let directory = self.directory.take().unwrap();
        let mut errors = Vec::new();
        if let Err(error) = mare_store::index::close_idle(directory.path()) {
            errors.push(format!("close binding test index {}: {error}", directory.path().display()));
        }
        let path = directory.path().to_path_buf();
        if let Err(error) = directory.close() {
            errors.push(format!("remove binding test directory {}: {error}", path.display()));
        }
        if !errors.is_empty() {
            let message = errors.join("\n");
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

#[test]
fn queued_index_work_keeps_its_submitted_home() {
    const CHILD: &str = "MARE_TEST_INDEX_HOME_SWITCH";
    if let Some(directory) = std::env::var_os(CHILD) {
        let directory = PathBuf::from(directory);
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
            tokio::task::LocalSet::new().run_until(async {
                let lua = Lua::new();
                let mare = lua.create_table().unwrap();
                install(&lua, &mare).unwrap();
                let store: Table = mare.get("store").unwrap();
                let reconcile: mlua::Function = store.get("index_reconcile").unwrap();
                let (started, start) = tokio::sync::oneshot::channel();
                let (release, released) = std::sync::mpsc::channel();
                *BEFORE_INDEX_WORK.lock().unwrap() = Some(Box::new(move || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                }));
                let pending = tokio::task::spawn_local(async move { reconcile.call_async::<Table>(()).await });
                start.await.unwrap();
                // SAFETY: this branch runs in an isolated child process; the worker has
                // reached the barrier after capturing its submitted root.
                unsafe { std::env::set_var("MARE_HOME", directory.join("other")) };
                release.send(()).unwrap();
                pending.await.unwrap().unwrap();
                crate::blocking::close_and_wait(&lua).await;
                let root = directory.join("original/state/sessions");
                assert!(root.join("index.sqlite").is_file());
                assert!(!directory.join("other").exists());
                mare_store::index::close_idle(&root).unwrap();
            }),
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "store::staged_attachment_tests::queued_index_work_keeps_its_submitted_home", "--nocapture"])
        .env(CHILD, directory.path())
        .env("MARE_HOME", directory.path().join("original"))
        .output()
        .unwrap();
    assert!(result.status.success(), "isolated index work failed: {result:?}");
    directory.close().unwrap();
}

#[test]
fn active_read_does_not_block_close_marker_or_identity_check() {
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = OwnedSessionDir::new();
    let path = dir.path().join("session.jsonl");
    let mut writer = SessionFile::create_new(&path).unwrap();
    writer
        .append(&serde_json::json!({"type":"session", "version":mare_store::CURRENT_VERSION, "session_id":"old"}), true)
        .unwrap();
    let file = Arc::new(SessionState::new(writer, dir.path().to_path_buf(), dir.blocking.clone()));
    let lua = Lua::new();
    let handle = lua
        .create_userdata(LuaSessionFile {
            file: file.clone(),
            journal: crate::exec_gate::JournalHold::new(&lua, true, false).unwrap(),
            created_new: true,
        })
        .unwrap();
    lua.globals().set("handle", handle).unwrap();
    lua.globals().set("path", path.to_string_lossy().to_string()).unwrap();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    let revision = lua
        .create_userdata(SnapshotRevision {
            file: file.clone(),
            stamp: mare_store::stamp(&path).unwrap(),
            generation: 0,
        })
        .unwrap();
    lua.globals().set("revision", revision).unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let reader_file = file.clone();
    let reader = std::thread::spawn(move || {
        read_session(&reader_file, None, |f| {
            ready_tx.send(()).unwrap();
            assert!(release_rx.recv_timeout(Duration::from_secs(30)).is_ok(), "reader timed out waiting for release");
            f.read_snapshot(None)
        })
    });
    ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    lua.load("assert(handle:matches_path(path)); assert(mare.store.snapshot_matches(handle, revision)); handle:append({type='state', state='closed'}, {sync=true}); assert(not mare.store.snapshot_matches(handle, revision)); handle:close()").exec().unwrap();
    assert!(SessionFile::open_existing(&path, None).is_err(), "the reader must retain the session lock");
    release_tx.send(()).unwrap();
    let read_error = reader.join().unwrap().unwrap_err();
    assert!(read_error.to_string().contains("changed"), "{read_error}");
    assert!(SessionFile::open_existing(&path, None).is_ok(), "session must be released after reading");
    let bytes = std::fs::read_to_string(&path).unwrap();
    assert!(bytes.contains("\"state\":\"closed\""));
}

#[test]
fn close_during_blocking_read_returns_before_read_releases_session() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = OwnedSessionDir::new();
    let path = dir.path().join("session.jsonl");
    let file = Arc::new(SessionState::new(
        SessionFile::create_new(&path).unwrap(),
        dir.path().to_path_buf(),
        dir.blocking.clone(),
    ));
    let lua = Lua::new();
    let handle = lua
        .create_userdata(LuaSessionFile {
            file: file.clone(),
            journal: crate::exec_gate::JournalHold::new(&lua, true, false).unwrap(),
            created_new: true,
        })
        .unwrap();
    lua.globals().set("handle", handle).unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker_file = file.clone();
    let worker = std::thread::spawn(move || {
        let _guard = worker_file.file.lock().unwrap();
        ready_tx.send(()).unwrap();
        // Bound the regression even if close incorrectly waits for this lock.
        assert!(release_rx.recv_timeout(Duration::from_secs(30)).is_ok(), "worker timed out waiting for release");
    });
    ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    lua.load("handle:close()").exec().unwrap();
    assert!(file.lock().is_err(), "closed handle must reject new work immediately");
    lua.load("assert(not pcall(function() handle:matches_path('unused') end)); handle:close()").exec().unwrap();
    assert!(SessionFile::open_with(&path, None).is_err(), "read still owns session lock");
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(dir.blocking.close_and_wait());
    assert!(SessionFile::open_with(&path, None).is_ok(), "session was not released after read completed");
}

#[cfg(unix)]
#[test]
fn staging_crash_child() {
    let Some(path) = std::env::var_os("MARE_TEST_STAGING_CRASH") else { return };
    let path = PathBuf::from(path);
    mare_shared::private::create_dir_new(&path).unwrap();
    let _stage = Arc::new(StagingDir::capture(path.clone()).unwrap());
    std::fs::write(path.join("image.png"), b"staged").unwrap();
    println!("STAGING_READY");
    std::io::stdout().flush().unwrap();
    std::thread::park();
}

#[cfg(unix)]
#[test]
fn hard_process_kill_can_leave_a_private_staging_directory() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("stage");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "store::staged_attachment_tests::staging_crash_child", "--nocapture"])
        .env("MARE_TEST_STAGING_CRASH", &path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0, "child exited before staging");
        if line.contains("STAGING_READY") {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(std::fs::read(path.join("image.png")).unwrap(), b"staged");
}

#[test]
fn cancelled_owner_cleans_only_after_the_blocking_write_releases_its_reference() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("stage");
    mare_shared::private::create_dir_new(&path).unwrap();
    let stage = Arc::new(StagingDir::capture(path.clone()).unwrap());
    let worker = stage.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        write_staged_attachment(worker, "image.png".into(), b"image".to_vec()).unwrap();
    });
    ready_rx.recv().unwrap();
    drop(stage);
    assert!(path.exists(), "a running write still owns the directory");
    release_tx.send(()).unwrap();
    handle.join().unwrap();
    assert!(!path.exists(), "the last write must clean up after itself");
}

#[test]
fn install_refuses_replaced_stage_and_target_directories() {
    let root = tempfile::tempdir().unwrap();
    let stage_path = root.path().join("stage.attachments");
    mare_shared::private::create_dir_new(&stage_path).unwrap();
    let stage = Arc::new(StagingDir::capture(stage_path.clone()).unwrap());
    write_staged_attachment(stage.clone(), "image.png".into(), b"image".to_vec()).unwrap();

    let journal = root.path().join("fork.jsonl");
    std::fs::write(&journal, b"").unwrap();
    let displaced_stage = root.path().join("stage-displaced");
    std::fs::rename(&stage_path, &displaced_stage).unwrap();
    mare_shared::private::create_dir_new(&stage_path).unwrap();
    let lua = Lua::new();
    assert!(
        install_staged_attachments(&lua, &stage, &journal).is_err(),
        "replacing the staging directory must be refused before installation"
    );
    assert!(!attachment_dir(&journal).exists(), "failed stage check must not create target attachments");

    // Restore a valid stage and replace the target parent during directory sync.
    let stage2_path = root.path().join("stage2.attachments");
    mare_shared::private::create_dir_new(&stage2_path).unwrap();
    let stage2 = Arc::new(StagingDir::capture(stage2_path).unwrap());
    write_staged_attachment(stage2.clone(), "image.png".into(), b"image".to_vec()).unwrap();

    let target_dir = root.path().join("session-dir");
    mare_shared::private::create_dir_new(&target_dir).unwrap();
    let target_journal = target_dir.join("fork.jsonl");
    std::fs::write(&target_journal, b"").unwrap();
    let displaced_target = root.path().join("session-dir-displaced");
    let target_dir_clone = target_dir.clone();
    let displaced_target_clone = displaced_target.clone();
    before_attachment_directory_sync(&lua, move |_| {
        std::fs::rename(&target_dir_clone, &displaced_target_clone)?;
        mare_shared::private::create_dir_new(&target_dir_clone)
    });
    assert!(
        install_staged_attachments(&lua, &stage2, &target_journal).is_err(),
        "replacing the target parent directory before sync must be refused"
    );
}
