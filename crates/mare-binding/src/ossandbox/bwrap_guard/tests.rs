use super::*;
use crate::ossandbox::{Backend, Policy, Prepared, level::Level};
use mlua::Value;
use std::time::{Duration, Instant};

fn policy(root: &Path) -> Policy {
    Policy::build(Level::WorkspaceWrite, root, None, &Value::Boolean(true)).unwrap()
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn configure_lifecycle_fds(command: &mut std::process::Command, fds: Option<[std::os::unix::io::RawFd; 2]>) {
    if let Some(fds) = fds {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || super::super::bwrap_lifecycle::inherit(fds));
        }
    }
}

fn spawn_fake_proc_watch(
    proc_root: &Path,
    watch: ProcessWatch,
    lease: Vec<SyntheticMount>,
    pid_alive: impl Fn(u32) -> std::io::Result<bool> + Send + Sync + 'static,
) -> (std::sync::mpsc::Receiver<()>, std::thread::JoinHandle<()>) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let proc_root = proc_root.to_path_buf();
    let pid_alive = std::sync::Arc::new(pid_alive);
    let handle = std::thread::spawn(move || {
        TEST_PROC_ROOT.with(|cell| *cell.borrow_mut() = Some(proc_root));
        TEST_PID_ALIVE.with(|cell| *cell.borrow_mut() = Some(pid_alive));
        watch.wait();
        TEST_PID_ALIVE.with(|cell| *cell.borrow_mut() = None);
        TEST_PROC_ROOT.with(|cell| *cell.borrow_mut() = None);
        drop(lease);
        let _ = done_tx.send(());
    });
    (done_rx, handle)
}

#[test]
fn unrelated_process_churn_does_not_report_a_lifecycle_gap_with_pidfd_monitoring() {
    if !crate::ossandbox::expect::require(
        "unrelated_process_churn_does_not_report_a_lifecycle_gap_with_pidfd_monitoring",
        crate::ossandbox::backend::backend().name(),
        &["bwrap"],
    ) {
        return;
    }
    if !super::super::bwrap_lifecycle::supported() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let policy = policy(dir.path());
    FORCE_SCAN_ENOENT.with(|count| count.set(MAX_PROC_OBSERVATIONS));
    assert!(
        !policy.missing(Backend::Bwrap).contains(&super::super::Cap::FsProtect),
        "unrelated process exits must not report a protection gap"
    );
    let prepared = Prepared::from_policy(policy, dir.path()).expect("preparation must not require a full proc scan");
    drop(prepared);
    assert!(!dir.path().join(".mare").exists());
    FORCE_SCAN_ENOENT.with(|count| count.set(0));
}

#[test]
fn process_exit_not_prepared_drop_releases_the_target() {
    if !crate::ossandbox::expect::require(
        "process_exit_not_prepared_drop_releases_the_target",
        crate::ossandbox::backend::backend().name(),
        &["bwrap"],
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let p = policy(dir.path());
    let prepared = Prepared::from_policy(p, dir.path()).unwrap();
    let wrapped = prepared.wrap_monitored("sh", &["-c".into(), "echo ready; read x".into()]);
    let monitor = prepared.start_monitor().unwrap().unwrap();
    let mut command = std::process::Command::new(wrapped.prog);
    command.args(wrapped.args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
    configure_lifecycle_fds(&mut command, wrapped.lifecycle_fds);
    let mut child = command.spawn().unwrap();
    monitor.attach(Some(child.id())).unwrap();
    use std::io::{BufRead, BufReader};
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    drop(prepared);
    assert!(dir.path().join(".mare").is_dir(), "a live process retains the target");
    child.kill().unwrap();
    child.wait().unwrap();
    wait_until(|| !dir.path().join(".mare").exists());
}

#[test]
fn finishing_last_holder_during_status_check_is_not_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let p = policy(dir.path());
    let target = dir.path().join(".mare");
    let held = prepare(&p).unwrap();
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let check_target = target.clone();
    let worker = std::thread::spawn(move || {
        AFTER_MARKER_CHECK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                arrived_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            }))
        });
        live_or_unmarked(&p, &check_target)
    });
    arrived_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    drop(held);
    assert!(!target.exists());
    resume_tx.send(()).unwrap();
    assert!(worker.join().unwrap(), "a normally removed target is safe to recreate");
}

#[test]
fn hardlinked_registry_files_are_refused_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("ordinary");
    std::fs::write(&original, "keep").unwrap();
    let mut mode = std::fs::metadata(&original).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    mode.set_mode(0o600);
    std::fs::set_permissions(&original, mode).unwrap();
    std::fs::hard_link(&original, dir.path().join("identity")).unwrap();
    assert!(write_identity(dir.path(), "replacement").is_err());
    assert_eq!(std::fs::read_to_string(original).unwrap(), "keep");
}

#[test]
fn stat_scanner_treats_comm_as_bytes_but_rejects_bad_numeric_fields() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("123");
    std::fs::create_dir(&pid).unwrap();
    let record = |group: &[u8], threads: &[u8]| {
        let mut fields = vec![b"0".as_slice(); 18];
        fields[0] = b"Z";
        fields[2] = group;
        fields[17] = threads;
        let mut data = b"123 (a) \xff name) ".to_vec();
        for field in fields {
            data.extend_from_slice(field);
            data.push(b' ');
        }
        data
    };
    std::fs::write(pid.join("stat"), record(b"123", b"1")).unwrap();
    assert!(!scan_process_group(dir.path(), 123).unwrap());
    std::fs::write(pid.join("stat"), record(b"123", b"2")).unwrap();
    assert!(scan_process_group(dir.path(), 123).unwrap());
    std::fs::write(pid.join("stat"), record(b"\xff", b"1")).unwrap();
    assert!(scan_process_group(dir.path(), 123).is_err());
    std::fs::write(pid.join("stat"), record(b"123", b"\xff")).unwrap();
    assert!(scan_process_group(dir.path(), 123).is_err());
}

#[test]
fn real_non_utf8_process_name_is_scanned_without_utf8_decoding() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    struct Cleanup(Child);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    if Command::new("python3").arg("--version").output().is_err() || !can_inspect_lifecycle() {
        eprintln!("skipped: python3 or complete procfs visibility is unavailable");
        return;
    }
    let code = r#"import ctypes,time; assert ctypes.CDLL(None).prctl(15,b'\xff\xfe',0,0,0)==0; print('ready',flush=True); time.sleep(30)"#;
    let mut command = Command::new("python3");
    command.args(["-c", code]).stdout(Stdio::piped()).process_group(0);
    let mut child = Cleanup(command.spawn().unwrap());
    let mut line = String::new();
    BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    let pid = child.0.id();
    let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
    assert!(std::str::from_utf8(&stat).is_err(), "fixture must use a non-UTF-8 process name");
    // Scan this real kernel stat record through an isolated proc root. Reading the entire
    // host /proc here made the assertion depend on unrelated processes not exiting mid-scan.
    let proc_root = tempfile::tempdir().unwrap();
    let proc_entry = proc_root.path().join(pid.to_string());
    std::fs::create_dir(&proc_entry).unwrap();
    std::fs::write(proc_entry.join("stat"), stat).unwrap();
    assert!(scan_process_group(proc_root.path(), pid).unwrap(), "the isolated child is live");
}

#[test]
fn zombie_leader_with_live_thread_keeps_group_live() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let code = "import ctypes,threading,time; threading.Thread(target=lambda:time.sleep(2)).start(); print('ready',flush=True); ctypes.CDLL(None).pthread_exit(None)";
    let mut command = Command::new("python3");
    command.args(["-c", code]).stdout(Stdio::piped()).process_group(0);
    let mut child = command.spawn().unwrap();
    let pid = child.id();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<_> = stat.rsplit_once(") ").unwrap().1.split_whitespace().collect();
        if fields[0] == "Z" {
            assert!(fields[17].parse::<u32>().unwrap() > 1);
            break;
        }
        assert!(Instant::now() < deadline, "leader did not enter zombie state");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(group_has_live_processes(pid).unwrap(), "a live sibling thread must keep the group live");
    child.kill().unwrap();
    child.wait().unwrap();
}

/// A vanished `/proc` entry restarts the observation (bounded), and exhausting the retries is an
/// error, never an empty group. The scan runs over an empty fixture tree, not the host's /proc,
/// so unrelated churn can't decide it (that churn is the separate stress repro below). The
/// watcher's handling of such an error is `unknown_group_observation_keeps_the_target…`.
#[test]
fn scan_retry_is_bounded_and_exhaustion_is_an_error() {
    let fixture = tempfile::tempdir().unwrap();
    TEST_PROC_ROOT.with(|root| *root.borrow_mut() = Some(fixture.path().to_path_buf()));
    FORCE_SCAN_ENOENT.with(|count| count.set(3));
    assert!(matches!(group_has_live_processes(u32::MAX), Ok(false)), "transient ENOENT is retried");
    assert_eq!(FORCE_SCAN_ENOENT.with(|count| count.get()), 0, "each retry rescans");
    FORCE_SCAN_ENOENT.with(|count| count.set(MAX_PROC_OBSERVATIONS));
    let result = group_has_live_processes(u32::MAX);
    assert!(result.is_err(), "exhaustion must never be treated as an empty group: {result:?}");
    FORCE_SCAN_ENOENT.with(|count| count.set(0));
    TEST_PROC_ROOT.with(|root| *root.borrow_mut() = None);
}

/// The fallback `/proc` watcher keeps the lease while the group's state is unknown and only a
/// clean "group empty" observation releases it.
#[test]
fn unknown_group_observation_keeps_the_target_until_the_group_is_seen_empty() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    let dir = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("99999")).unwrap();
    let lease = prepare(&policy(dir.path())).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let watch = ProcessWatch::open(pid, true).unwrap();

    let unknown = Arc::new(AtomicBool::new(true));
    let unknown_check = Arc::clone(&unknown);
    let (seen_tx, seen_rx) = mpsc::channel();
    let (done_rx, handle) = spawn_fake_proc_watch(fixture.path(), watch, lease, move |_| {
        let _ = seen_tx.send(());
        if unknown_check.load(Ordering::SeqCst) {
            Err(std::io::Error::other("injected: group state unknown"))
        } else {
            Ok(false)
        }
    });

    // Wait for two consecutive unknown-state observations so ProcessWatch::wait has processed at
    // least one Err, refused to release, slept 20ms, and re-entered the scan loop.
    seen_rx.recv_timeout(Duration::from_secs(60)).expect("the watcher took no observation");
    seen_rx.recv_timeout(Duration::from_secs(60)).expect("the watcher did not continue polling after unknown state");
    assert!(done_rx.try_recv().is_err(), "an unknown group state must keep the lease");
    assert!(dir.path().join(".mare").exists(), "the target must stay while the group state is unknown");

    unknown.store(false, Ordering::SeqCst);
    done_rx.recv_timeout(Duration::from_secs(60)).expect("watcher did not release after group became empty");
    handle.join().unwrap();
    assert!(!dir.path().join(".mare").exists());
}

/// Real-scan conflict end-to-end test: runs `ProcessWatch::wait` -> `group_has_live_processes`
/// -> `scan_process_group`. While a numeric `/proc/<pid>` entry has missing `stat` and
/// `pid_is_alive` reports the PID as still alive, `scan_process_group` fails, retries exhaust to
/// an explicit unknown error, and `.mare` MUST remain on disk. Once the conflict clears
/// (`pid_is_alive` confirms ESRCH), the next real scan returns `Ok(false)`, the watcher finishes,
/// and `.mare` is released.
#[test]
fn scan_conflict_keeps_target_via_real_watcher_until_resolved() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    let dir = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("99999")).unwrap();
    let lease = prepare(&policy(dir.path())).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let watch = ProcessWatch::open(pid, true).unwrap();

    let conflict = Arc::new(AtomicBool::new(true));
    let conflict_check = Arc::clone(&conflict);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_check = Arc::clone(&calls);
    let (exhausted_tx, exhausted_rx) = mpsc::channel();
    let (done_rx, handle) = spawn_fake_proc_watch(fixture.path(), watch, lease, move |_| {
        let n = calls_check.fetch_add(1, Ordering::SeqCst) + 1;
        if n == MAX_PROC_OBSERVATIONS + 1 {
            let _ = exhausted_tx.send(());
        }
        Ok(conflict_check.load(Ordering::SeqCst))
    });

    exhausted_rx.recv_timeout(Duration::from_secs(60)).expect("watcher never resumed polling after retry exhaustion");
    let calls_during_conflict = calls.load(Ordering::SeqCst);
    assert!(
        calls_during_conflict >= MAX_PROC_OBSERVATIONS,
        "pid_alive must be invoked across all {MAX_PROC_OBSERVATIONS} retries, got {calls_during_conflict}"
    );
    assert!(done_rx.try_recv().is_err(), "the lease must stay held while scan conflict persists");
    assert!(dir.path().join(".mare").exists(), "the target must stay while scan conflict persists");

    conflict.store(false, Ordering::SeqCst);
    done_rx.recv_timeout(Duration::from_secs(60)).expect("watcher never observed empty group after conflict resolved");
    handle.join().unwrap();
    assert!(calls.load(Ordering::SeqCst) > calls_during_conflict, "resolved ESRCH scan must also invoke pid_alive");
    assert!(!dir.path().join(".mare").exists());
}

/// Regression test for FLAKY-SANDBOX (BWRAP-SCAN): unrelated short-lived process churn must not
/// starve `/proc` process-group observation, `can_inspect_lifecycle`, or `.mare` target release.
#[test]
fn unrelated_process_churn_does_not_starve_group_observation() {
    use std::os::unix::process::CommandExt;
    if super::super::mount_inspection::visible_proc().is_err() {
        eprintln!("skipped: complete procfs visibility is unavailable");
        return;
    }
    struct Churn(Vec<std::process::Child>);
    impl Drop for Churn {
        fn drop(&mut self) {
            for child in &mut self.0 {
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
                let _ = child.wait();
            }
        }
    }
    let mut churn = Churn(Vec::new());
    for _ in 0..8 {
        let loop_ =
            std::process::Command::new("sh").args(["-c", "while :; do /bin/true; done"]).process_group(0).spawn();
        churn.0.push(loop_.unwrap());
    }
    for attempt in 0..3 {
        let started = Instant::now();
        let result = group_has_live_processes(u32::MAX);
        assert!(
            matches!(result, Ok(false)),
            "observation {attempt} of an absent group under unrelated churn: {result:?} after {:?}",
            started.elapsed()
        );
    }
    if super::super::mount_inspection::visible_proc().is_ok() {
        assert!(
            can_inspect_lifecycle(),
            "unrelated process churn must not make can_inspect_lifecycle report a false gap"
        );
    }
}

#[test]
fn unreadable_or_malformed_process_stat_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("123");
    std::fs::create_dir(&pid).unwrap();
    std::fs::write(pid.join("stat"), "malformed").unwrap();
    assert!(scan_process_group(dir.path(), 123).is_err());
    std::fs::remove_file(pid.join("stat")).unwrap();
    std::fs::create_dir(pid.join("stat")).unwrap();
    assert!(scan_process_group(dir.path(), 123).is_err());
    std::fs::remove_dir_all(&pid).unwrap();

    // Deterministic PID-liveness injection covering ESRCH, live PID, and EPERM.
    let dead = dir.path().join("99998");
    std::fs::create_dir(&dead).unwrap();
    TEST_PID_ALIVE.with(|cell| *cell.borrow_mut() = Some(std::sync::Arc::new(|_| Ok(false))));
    assert!(!scan_process_group(dir.path(), 123).unwrap(), "ENOENT with ESRCH must be tolerated");
    TEST_PID_ALIVE.with(|cell| *cell.borrow_mut() = Some(std::sync::Arc::new(|_| Ok(true))));
    assert!(scan_process_group(dir.path(), 123).is_err(), "ENOENT for a live PID must fail");
    TEST_PID_ALIVE.with(|cell| {
        *cell.borrow_mut() = Some(std::sync::Arc::new(|_| Err(std::io::Error::from_raw_os_error(libc::EPERM))));
    });
    assert!(scan_process_group(dir.path(), 123).is_err(), "ENOENT with EPERM must fail");
    TEST_PID_ALIVE.with(|cell| *cell.borrow_mut() = None);
    std::fs::remove_dir_all(&dead).unwrap();

    let live = dir.path().join(std::process::id().to_string());
    std::fs::create_dir(&live).unwrap();
    assert!(scan_process_group(dir.path(), 123).is_err(), "ENOENT for a live PID is not proof of exit");
}

#[test]
fn registry_visible_to_writable_root_is_a_capability_gap() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = policy(dir.path());
    p.extra.push(registry().unwrap().parent().unwrap().to_path_buf());
    assert!(!registry_safe(&p));
    assert!(p.bwrap_metadata_gap());
    assert!(prepare(&p).is_err());
}

#[test]
fn cleanup_preserves_replaced_target_and_new_contents() {
    let dir = tempfile::tempdir().unwrap();
    let p = policy(dir.path());
    let path = dir.path().join(".mare");
    let guard = prepare(&p).unwrap();
    std::fs::write(path.join("user-data"), "keep").unwrap();
    drop(guard);
    assert_eq!(std::fs::read_to_string(path.join("user-data")).unwrap(), "keep");
    std::fs::remove_dir_all(&path).unwrap();
    let guard = prepare(&p).unwrap();
    std::fs::rename(&path, dir.path().join("old-target")).unwrap();
    std::fs::create_dir(&path).unwrap();
    drop(guard);
    assert!(path.is_dir(), "a replacement must not be deleted");
    assert!(!synthetic(&p, &path), "a replacement must not be treated as synthetic");
}

#[test]
fn failed_preparation_rolls_back_earlier_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = policy(dir.path());
    p.extra.push(dir.path().join("missing-parent"));
    assert!(prepare(&p).is_err());
    assert!(!dir.path().join(".mare").exists());
}

#[test]
fn failed_child_start_releases_target() {
    if !crate::ossandbox::expect::require(
        "failed_child_start_releases_target",
        crate::ossandbox::backend::backend().name(),
        &["bwrap"],
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let prepared = Prepared::from_policy(policy(dir.path()), dir.path()).unwrap();
    let wrapped = prepared.wrap_monitored("/nonexistent/mare-test-command", &[]);
    let monitor = prepared.start_monitor().unwrap().unwrap();
    let mut command = std::process::Command::new(wrapped.prog);
    command.args(wrapped.args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    configure_lifecycle_fds(&mut command, wrapped.lifecycle_fds);
    let mut child = command.spawn().unwrap();
    monitor.attach(Some(child.id())).unwrap();
    assert!(!child.wait().unwrap().success());
    wait_until(|| !dir.path().join(".mare").exists());
}

/// Covers the `ProcessWatch::open(pid, true)` + `wait()` fallback path (`prepared.rs` fallback
/// when `bwrap_lifecycle` / `visible_proc` is not active): after the wrapper leader has exited
/// and been reaped, a live background member in the same process group must prevent
/// `ProcessWatch::wait` from releasing `.mare` until the member finishes.
#[test]
fn exited_wrapper_retains_target_while_group_member_is_live() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc;
    if super::super::mount_inspection::visible_proc().is_err() {
        eprintln!("skipped: complete procfs visibility is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let lease = prepare(&policy(dir.path())).unwrap();
    let release = dir.path().join("release-member");
    let mut cmd = std::process::Command::new("sh");
    cmd.args([
        "-c",
        r#"(while [ ! -e "$1" ]; do sleep 0.01; done) >/dev/null 2>&1 & echo ready"#,
        "sh",
        release.to_string_lossy().as_ref(),
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let pid = child.id();
    struct KillGroup(u32);
    impl Drop for KillGroup {
        fn drop(&mut self) {
            unsafe { libc::kill(-(self.0 as libc::pid_t), libc::SIGKILL) };
        }
    }
    let _group = KillGroup(pid);

    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    assert!(child.wait().unwrap().success(), "wrapper leader must exit cleanly while descendant runs");
    assert_eq!(unsafe { libc::kill(-(pid as libc::pid_t), 0) }, 0, "background member must stay alive in pgrp");
    assert!(group_has_live_processes(pid).unwrap(), "group scan must report live descendant after leader exits");

    let watch = ProcessWatch::open(pid, true).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        watch.wait();
        drop(lease);
        let _ = done_tx.send(());
    });

    // sleep-ok: let ProcessWatch::wait run multiple 20ms scan cycles while the descendant lives
    std::thread::sleep(Duration::from_millis(80));
    assert!(done_rx.try_recv().is_err(), "ProcessWatch::wait must not finish while group member lives");
    assert!(dir.path().join(".mare").exists(), "a live descendant keeps the target after wrapper shell exits");

    std::fs::write(&release, "").unwrap();
    done_rx.recv_timeout(Duration::from_secs(60)).expect("ProcessWatch::wait did not release after group exited");
    handle.join().unwrap();
    assert!(!dir.path().join(".mare").exists());
}

#[test]
fn waitid_reused_pid_case_keeps_a_new_live_child() {
    // If an old PID is reused by another child of this host, the observation is a live
    // child: keep waiting conservatively instead of declaring it exited.
    let mut newer = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let pid = newer.id();
    assert!(!child_wait_status(pid).unwrap());
    newer.kill().unwrap();
    newer.wait().unwrap();
    assert!(child_wait_status(pid).unwrap(), "a reaped or non-child PID is complete");
}

#[test]
fn waitid_fallback_handles_already_reaped_child() {
    let mut child = std::process::Command::new("sleep").arg("0.1").spawn().unwrap();
    let watch = ProcessWatch::open(child.id(), true).unwrap();
    assert!(matches!(watch, ProcessWatch::Child { .. }));
    child.wait().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        watch.wait();
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(2)).is_ok(),
        "ECHILD must finish monitoring after another waiter reaps"
    );
}

#[test]
fn stale_leases_preserve_targets_as_existing_paths() {
    use std::process::{Command, Stdio};
    if let Some(root) = std::env::var_os("MARE_TEST_S7_CRASH_ROOT") {
        let guard = prepare(&policy(&PathBuf::from(root))).unwrap();
        if let Ok(pid) = std::env::var("MARE_TEST_S7_CRASH_PID") {
            guard[0].mark_child(pid.parse().unwrap()).unwrap();
        }
        std::process::exit(0);
    }
    for record_child in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut worker = Command::new("sleep").arg("60").spawn().unwrap();
        let mut owner = Command::new(std::env::current_exe().unwrap());
        owner
            .args(["--exact", "ossandbox::bwrap_guard::tests::stale_leases_preserve_targets_as_existing_paths"])
            .env("MARE_TEST_S7_CRASH_ROOT", dir.path())
            .stdout(Stdio::null());
        if record_child {
            owner.env("MARE_TEST_S7_CRASH_PID", worker.id().to_string());
        }
        assert!(owner.status().unwrap().success());
        let p = policy(dir.path());
        let target = dir.path().join(".mare");
        if record_child {
            let live = prepare(&p).unwrap();
            assert_eq!(live.len(), 1, "live child keeps its target coordinated");
            drop(live);
            assert!(target.exists());
        }
        worker.kill().unwrap();
        worker.wait().unwrap();
        for _ in 0..2 {
            let recovered = prepare(&p).unwrap();
            assert!(recovered.is_empty(), "abandoned target becomes an ordinary existing path");
            assert!(target.exists());
            drop(recovered);
            assert!(target.exists(), "ordinary targets are never removed");
        }
        if super::super::backend() == Backend::Bwrap {
            let prepared = Prepared::from_policy(p, dir.path()).unwrap();
            let wrapped = prepared.wrap("sh", &["-c".into(), "echo denied > .mare/escape".into()]);
            assert!(!Command::new(wrapped.prog).args(wrapped.args).status().unwrap().success());
            assert!(!target.join("escape").exists());
            drop(prepared);
            assert!(target.exists());
        }
    }
}

#[test]
fn orphan_record_survives_other_live_holders() {
    let dir = tempfile::tempdir().unwrap();
    let p = policy(dir.path());
    let guard = prepare(&p).unwrap();
    let target = dir.path().join(".mare");
    let entry = entry(&target).unwrap();
    let abandoned = entry.join("lease-abandoned");
    std::fs::write(&abandoned, b"pid 2147483647\n").unwrap();
    std::fs::set_permissions(&abandoned, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    // Temporarily remove our live entry from the scan while keeping its lock alive, to
    // force the abandoned record to be observed before another live lease.
    std::fs::rename(&guard[0].lease_path, entry.join("held")).unwrap();
    {
        let _lock = RegistryLock::acquire().unwrap();
        assert!(state(&entry, &target).unwrap() == LeaseState::Orphan);
    }
    std::fs::rename(entry.join("held"), &guard[0].lease_path).unwrap();
    {
        let _lock = RegistryLock::acquire().unwrap();
        assert!(state(&entry, &target).unwrap() == LeaseState::Active);
    }
    drop(guard);
    assert!(target.exists(), "last live holder must preserve the abandoned target");
    assert!(prepare(&p).unwrap().is_empty());
    assert!(target.exists());
}

#[test]
fn independent_processes_share_and_release_owned_target() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    if let Some(root) = std::env::var_os("MARE_TEST_S7_LEASE_ROOT") {
        let root = PathBuf::from(root);
        let p = policy(&root);
        let guard = prepare(&p).unwrap();
        assert!(synthetic(&p, &root.join(".mare")));
        println!("S7_LEASE_READY");
        std::io::stdout().flush().unwrap();
        for _ in 0..500 {
            if root.join("release").exists() {
                drop(guard);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("parent did not release child");
    }
    let dir = tempfile::tempdir().unwrap();
    let p = policy(dir.path());
    let first = prepare(&p).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::bwrap_guard::tests::independent_processes_share_and_release_owned_target",
            "--nocapture",
        ])
        .env("MARE_TEST_S7_LEASE_ROOT", dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !sent && line.contains("S7_LEASE_READY") {
                let _ = tx.send(());
                sent = true;
            }
        }
    });
    if rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child failed to acquire lease");
    }
    drop(first);
    assert!(dir.path().join(".mare").exists());
    std::fs::write(dir.path().join("release"), "go").unwrap();
    assert!(child.wait().unwrap().success());
    wait_until(|| !dir.path().join(".mare").exists());
}
