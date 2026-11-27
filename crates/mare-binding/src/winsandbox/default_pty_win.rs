//! ConPTY launch and stdio bridging for the Windows default sandbox tier.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::token::restricted_token;
use super::{LocalSid, RunnerArgs, argv_to_command_line, last, launch_desktop, wide};

// winbase.h ProcThreadAttributeValue(22, FALSE, TRUE, FALSE)
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

fn pty_coord(cols: u16, rows: u16) -> COORD {
    COORD { X: cols.min(i16::MAX as u16) as i16, Y: rows.min(i16::MAX as u16) as i16 }
}

/// Owns a `PROC_THREAD_ATTRIBUTE_LIST` holding the pseudoconsole handle.
struct PtyAttrList {
    _buf: Vec<u8>,
    attrs: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl PtyAttrList {
    unsafe fn new(hpc: HPCON) -> Result<Self, String> {
        unsafe {
            let mut size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            let mut buf = vec![0u8; size];
            let attrs = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0 {
                return Err(last("InitializeProcThreadAttributeList"));
            }
            if UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                hpc as *const c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                DeleteProcThreadAttributeList(attrs);
                return Err(last("UpdateProcThreadAttribute(PSEUDOCONSOLE)"));
            }
            Ok(Self { _buf: buf, attrs })
        }
    }
}

impl Drop for PtyAttrList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.attrs) };
    }
}

unsafe fn create_pty_pipes_and_console(cols: u16, rows: u16) -> Result<(HPCON, HANDLE, HANDLE), String> {
    unsafe {
        // pipes: (pty input: we write, ConPTY reads) and (pty output: ConPTY writes, we read)
        let (mut in_rd, mut in_wr, mut out_rd, mut out_wr): (HANDLE, HANDLE, HANDLE, HANDLE) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        if CreatePipe(&mut in_rd, &mut in_wr, std::ptr::null(), 0) == 0 {
            return Err(last("CreatePipe (pty input)"));
        }
        if CreatePipe(&mut out_rd, &mut out_wr, std::ptr::null(), 0) == 0 {
            CloseHandle(in_rd);
            CloseHandle(in_wr);
            return Err(last("CreatePipe (pty output)"));
        }
        let mut hpc: HPCON = std::mem::zeroed();
        let hr = CreatePseudoConsole(pty_coord(cols, rows), in_rd, out_wr, 0, &mut hpc);
        // the ConPTY holds its own references now
        CloseHandle(in_rd);
        CloseHandle(out_wr);
        if hr < 0 {
            CloseHandle(in_wr);
            CloseHandle(out_rd);
            return Err(format!("CreatePseudoConsole failed: HRESULT {hr:#x}"));
        }
        Ok((hpc, in_wr, out_rd))
    }
}

unsafe fn spawn_pty_bridge_threads(
    hpc: HPCON,
    in_wr: HANDLE,
    out_rd: HANDLE,
) -> (std::sync::mpsc::Receiver<()>, std::sync::Arc<std::fs::File>) {
    unsafe {
        // bridge: terminal output → our stdout
        let out_pipe = std::fs::File::from_raw_handle(out_rd as _);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let mut out_pipe = out_pipe;
            let mut stdout = std::io::stdout();
            let mut buf = [0u8; 8192];
            loop {
                match out_pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = done_tx.send(());
        });
        // bridge: framed stdin → terminal input / resizes (the HPCON crosses as an integer)
        let in_pipe = std::sync::Arc::new(std::fs::File::from_raw_handle(in_wr as _));
        let in_pipe_thread = std::sync::Arc::clone(&in_pipe);
        let hpc_bits: usize = std::mem::transmute_copy(&hpc);
        std::thread::spawn(move || {
            let mut dec = crate::ptybridge::Decoder::default();
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let Ok(frames) = dec.push(&buf[..n]) else { break };
                for f in frames {
                    match f {
                        crate::ptybridge::Frame::Data(d) => {
                            if in_pipe_thread.as_ref().write_all(&d).is_err() {
                                return;
                            }
                        }
                        crate::ptybridge::Frame::Resize(c, r) => {
                            let h: HPCON = std::mem::transmute_copy(&hpc_bits);
                            ResizePseudoConsole(h, pty_coord(c, r));
                        }
                    }
                }
            }
            // Keep `in_pipe` alive in `run_pty` until the child exits so immediate stdin EOF
            // (`Stdio::null()`) does not trigger `conhost.exe` teardown (`0xC000013A`).
        });
        (done_rx, in_pipe)
    }
}

/// PTY mode: the confined
/// command can't attach to a console mare owns, so the runner creates the ConPTY, starts the
/// command on it under the confined token, and bridges it over its own stdio — framed input
/// and resizes in ([`crate::ptybridge`]), the terminal's raw output out.
pub(super) fn run_pty(
    a: &RunnerArgs,
    caps: &[LocalSid],
    (cols, rows): (u16, u16),
    environment: &[u16],
    ready: &mut dyn FnMut() -> Result<(), String>,
) -> Result<i32, String> {
    unsafe {
        SetConsoleCtrlHandler(None, 1);
        let (hpc, in_wr, out_rd) = create_pty_pipes_and_console(cols, rows)?;
        let close_pty_handles = || {
            ClosePseudoConsole(hpc);
            CloseHandle(in_wr);
            CloseHandle(out_rd);
        };
        let attr_list = match PtyAttrList::new(hpc) {
            Ok(list) => list,
            Err(e) => {
                close_pty_handles();
                return Err(e);
            }
        };
        let token = match restricted_token(caps, a.account.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                drop(attr_list);
                close_pty_handles();
                return Err(e);
            }
        };
        let (_desktop, mut desktop) = match launch_desktop(a.account.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                CloseHandle(token);
                drop(attr_list);
                close_pty_handles();
                return Err(e);
            }
        };
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
        // no inherited stdio: the console is the pseudoconsole
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        si.lpAttributeList = attr_list.attrs;
        let mut cmdline = wide(argv_to_command_line(&a.argv));
        let cwd = wide(&a.cwd);
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessAsUserW(
            token,
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        );
        let launch_error =
            (ok == 0).then(|| last(&format!("starting {:?} confined on a ConPTY (CreateProcessAsUserW)", a.argv[0])));
        CloseHandle(token);
        drop(attr_list);
        if let Some(e) = launch_error {
            close_pty_handles();
            return Err(e);
        }
        if let Err(error) = ready() {
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            close_pty_handles();
            return Err(error);
        }
        CloseHandle(pi.hThread);
        let (done_rx, in_pipe) = spawn_pty_bridge_threads(hpc, in_wr, out_rd);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        drop(in_pipe);
        // closing the pseudoconsole ends its output stream; let the tail drain (bounded)
        ClosePseudoConsole(hpc);
        let _ = done_rx.recv_timeout(std::time::Duration::from_secs(5));
        Ok(code as i32)
    }
}
