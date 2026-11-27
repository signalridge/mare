//! Regression for lifecycle observations while unrelated processes start and exit.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::common::{ChildGroup, pid_alive};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

struct Churn {
    stop: Arc<AtomicBool>,
    count: Arc<AtomicUsize>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Churn {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let threads = (0..3)
            .map(|_| {
                let stop = stop.clone();
                let count = count.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let status = Command::new("/bin/true").stdout(Stdio::null()).stderr(Stdio::null()).status();
                        if !status.is_ok_and(|s| s.success()) {
                            break;
                        }
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        Self { stop, count, threads }
    }
}

impl Drop for Churn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[tokio::test]
async fn independent_hosts_clean_up_under_unrelated_process_churn() {
    let usable = Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--proc",
            "/proc",
            "--die-with-parent",
            "--",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        return;
    }
    let churn = Churn::start();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "suite::sandbox::protection::separate_hosts_keep_concurrent_sandboxes_protected",
            "--nocapture",
        ])
        .env_remove("MARE_TEST_SANDBOX_SHARED_WORKSPACE")
        .env_remove("MARE_TEST_SANDBOX_HOST_LABEL")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut child = ChildGroup(child);
    // the fixture's own 30s budget plus each of its two hosts' separately bounded initialization
    let deadline = Instant::now() + Duration::from_secs(30) + 2 * super::protection::SANDBOX_HOST_STARTUP_LIMIT;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the concurrent sandbox regression timed out under process churn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(status.success(), "the concurrent sandbox regression failed under process churn: {status}");
    assert!(churn.count.load(Ordering::Relaxed) >= 10, "the fixture did not generate process churn");
}

#[tokio::test]
async fn child_group_drop_kills_descendants_after_direct_exit() {
    let dir = tempfile::tempdir().unwrap();
    let pid_path = dir.path().join("descendant");
    let child = Command::new("sh")
        .args(["-c", "sleep 60 & echo $! > \"$1\""])
        .arg("fixture")
        .arg(&pid_path)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut group = ChildGroup(child);
    assert!(group.0.wait().unwrap().success());
    let pid: u32 = std::fs::read_to_string(pid_path).unwrap().trim().parse().unwrap();
    assert!(pid_alive(pid), "the descendant must outlive the direct child before cleanup");
    drop(group);
    let deadline = Instant::now() + Duration::from_secs(2);
    while pid_alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let survived = pid_alive(pid);
    if survived {
        // Keep the failure reproducer from leaving its own sleeper behind.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    assert!(!survived, "group cleanup must kill the surviving descendant");
}
