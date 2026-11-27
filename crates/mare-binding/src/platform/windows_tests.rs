use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn job_object_kills_the_tree() {
    // the child starts a grandchild that outlives it unless the job takes the whole tree
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/c", "ping -n 30 127.0.0.1 >NUL"]);
    <Process as ProcessBackend>::isolate(&mut cmd);
    let mut child = spawn(&mut cmd).expect("spawn");
    let pid = child.id().unwrap();
    let job = win::job_of(pid).expect("job for the child");
    assert!(wait_until(|| active_job_processes(job) >= 2), "cmd and its ping child must both be running in the job");
    <Process as ProcessBackend>::signal_tree(pid, Signal::Kill).expect("terminate job");
    let st = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    assert!(st.is_ok(), "the job's root did not exit after TerminateJobObject");
    assert!(wait_until(|| active_job_processes(job) == 0), "a descendant survived termination of the job");
    assert!(!tree_alive(Some(pid)));
}

/// A Job that can't be set up fails the spawn, and the suspended process never runs
/// no output file appears.
#[tokio::test]
async fn a_job_setup_failure_fails_the_spawn_without_running_the_process() {
    let marker = std::env::temp_dir().join(format!("mare-job-fail-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/c", &format!("echo ran> \"{}\"", marker.display())]);
    <Process as ProcessBackend>::isolate(&mut cmd);
    win::FAIL_JOB_SETUP.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = spawn(&mut cmd);
    win::FAIL_JOB_SETUP.store(false, std::sync::atomic::Ordering::Relaxed);
    let Err(err) = r else { panic!("the spawn must fail") };
    assert!(err.to_string().contains("Job Object"), "{err}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!marker.exists(), "the process ran although it was never put in its job");
}

fn conpty(prog: &str, args: &[&str]) -> (Terminal, TerminalReader, TerminalWriter) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    Terminal::spawn(TerminalCommand {
        prog,
        args: &args,
        cwd: &cwd,
        env: std::env::vars_os().collect(),
        cols: 80,
        rows: 24,
    })
    .expect("spawn on a ConPTY")
}

fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !f() {
        if std::time::Instant::now() >= end {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    true
}

fn active_job_processes(job: isize) -> u32 {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation, QueryInformationJobObject,
    };
    unsafe {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(
            job as HANDLE,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        assert_ne!(ok, 0, "QueryInformationJobObject: {}", std::io::Error::last_os_error());
        info.ActiveProcesses
    }
}

/// The ConPTY child mare creates itself starts suspended inside its KILL_ON_JOB_CLOSE job
/// it and its grandchild (`ping` under `cmd`) are job members from the
/// start, and closing the job — what happens when mare exits, however it exits — ends both.
#[test]
fn conpty_child_runs_inside_its_job_and_dies_when_the_job_closes() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    let (mut t, mut r, _w) = conpty("cmd", &["/c", "ping -n 30 127.0.0.1 >NUL"]);
    // drain the console's output so it never blocks on a full pipe
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while matches!(std::io::Read::read(&mut r, &mut buf), Ok(n) if n > 0) {}
    });
    let pid = t.pid();
    assert!(tree_alive(Some(pid)), "the ConPTY child must be inside its Job Object");
    let job = win::job_of(pid).expect("a job for the ConPTY child");
    assert!(wait_until(|| active_job_processes(job) >= 2), "cmd and its ping should both be in the job");
    // close the job's last handle, as the OS does when mare exits
    unsafe { CloseHandle(win::take_job(pid).expect("job handle") as HANDLE) };
    assert!(wait_until(|| matches!(t.try_wait(), Ok(Some(_)))), "the child outlived its job");
}

/// Input, output, resize, exit code and kill keep working on the ConPTY mare creates.
#[test]
fn conpty_terminal_echoes_resizes_and_reports_exit() {
    let (mut t, mut r, mut w) = conpty("cmd", &["/q", "/k"]);
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = std::io::Read::read(&mut r, &mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    std::io::Write::write_all(&mut w, b"echo mare-conpty-ok\r\n").unwrap();
    let mut out = String::new();
    assert!(
        wait_until(|| {
            while let Ok(b) = rx.try_recv() {
                out.push_str(&String::from_utf8_lossy(&b));
            }
            out.contains("mare-conpty-ok")
        }),
        "{out:?}"
    );
    t.resize(100, 30).expect("resize");
    std::io::Write::write_all(&mut w, b"exit 7\r\n").unwrap();
    assert!(wait_until(|| matches!(t.try_wait(), Ok(Some(_)))));
    assert_eq!(t.try_wait().unwrap(), Some(7));
    t.kill().expect("killing an exited child is fine");
}

#[test]
fn conpty_env_block_and_path_search() {
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        vec![("b".into(), "1".into()), ("A".into(), "2".into()), ("B".into(), "3".into())];
    let block = String::from_utf16(&term::env_block(&env)).unwrap();
    assert_eq!(block, "A=2\0B=3\0\0", "sorted, case-insensitively unique, last wins");
    let sys = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let env = vec![
        ("PATH".into(), std::path::Path::new(&sys).join("System32").into_os_string()),
        ("PATHEXT".into(), ".COM;.EXE".into()),
    ];
    assert!(term::search_path("cmd", &env).to_ascii_lowercase().ends_with(r"\system32\cmd.exe"));
    assert_eq!(term::search_path(r"C:\x\y.exe", &env), r"C:\x\y.exe");
    assert_eq!(term::search_path("mare-no-such-program", &env), "mare-no-such-program");
}

#[tokio::test]
async fn named_pipe_roundtrip() {
    let dir = std::env::temp_dir().join(format!("mare-pipe-test-{}", std::process::id()));
    let path = dir.join("peer.sock");
    let l = <Ipc as IpcBackend>::bind(&path, 0o600).expect("bind");
    assert!(path.exists(), "marker file");
    assert!(<Ipc as IpcBackend>::bind(&path, 0o600).is_err(), "a live endpoint is refused");
    let server = tokio::spawn(async move {
        let s = <Ipc as IpcBackend>::accept(&l).await.unwrap();
        let (r, mut w) = tokio::io::split(s);
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        w.write_all(format!("echo:{line}").as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    });
    let c = <Ipc as IpcBackend>::connect(path.to_str().unwrap()).await.expect("connect");
    let (r, mut w) = tokio::io::split(c);
    w.write_all(b"hi\n").await.unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    assert_eq!(line, "echo:hi\n");
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}
