//! Windows ConPTY processes, created suspended and resumed inside their job.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// The pseudoconsole's size quirk flag (no win32-input-mode).
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;

pub struct Imp {
    hpc: HPCON,
    process: HANDLE,
    pid: u32,
}

// the handles are owned by this value alone and usable from any thread
unsafe impl Send for Imp {}

impl Drop for Imp {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.hpc);
            CloseHandle(self.process);
        }
    }
}

fn coord(cols: u16, rows: u16) -> COORD {
    COORD { X: cols.clamp(1, i16::MAX as u16) as i16, Y: rows.clamp(1, i16::MAX as u16) as i16 }
}

fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn os_err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

type Env = [(OsString, OsString)];

fn env_get<'a>(env: &'a Env, key: &str) -> Option<&'a OsStr> {
    env.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_os_str())
}

/// The program as `CreateProcessW` should start it, looked up on the *child's* `PATH` with
/// its `PATHEXT`; unchanged when it has a directory part or nothing matches.
pub(super) fn search_path(prog: &str, env: &Env) -> String {
    let p = std::path::Path::new(prog);
    if p.components().count() > 1 || p.is_absolute() {
        return prog.to_string();
    }
    let exts: Vec<String> = env_get(env, "PATHEXT")
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    let has_ext = p.extension().is_some();
    for dir in std::env::split_paths(env_get(env, "PATH").unwrap_or_default()) {
        if has_ext && dir.join(prog).is_file() {
            return dir.join(prog).to_string_lossy().into_owned();
        }
        for ext in &exts {
            let cand = dir.join(format!("{prog}{ext}"));
            if cand.is_file() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    prog.to_string()
}

/// `CREATE_UNICODE_ENVIRONMENT` block: `k=v\0…\0`, names case-insensitively unique (the
/// last one wins) and sorted, as Windows expects.
pub(super) fn env_block(env: &Env) -> Vec<u16> {
    let mut vars: Vec<&(OsString, OsString)> = vec![];
    for kv in env {
        vars.retain(|(k, _)| !k.eq_ignore_ascii_case(&kv.0));
        vars.push(kv);
    }
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block: Vec<u16> = vec![];
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

impl Imp {
    pub fn spawn(c: super::TerminalCommand<'_>) -> io::Result<(Self, super::TerminalReader, super::TerminalWriter)> {
        let argv: Vec<String> = std::iter::once(search_path(c.prog, &c.env)).chain(c.args.iter().cloned()).collect();
        let mut cmdline = wide(super::cmdline::argv_to_command_line(&argv));
        let cwd = wide(c.cwd);
        let mut env = env_block(&c.env);
        unsafe {
            // pipes: terminal input (we write, the console reads) and output (the reverse)
            let null = std::ptr::null_mut();
            let (mut in_rd, mut in_wr, mut out_rd, mut out_wr): (HANDLE, HANDLE, HANDLE, HANDLE) =
                (null, null, null, null);
            if CreatePipe(&mut in_rd, &mut in_wr, std::ptr::null(), 0) == 0 {
                return Err(os_err("CreatePipe (terminal input)"));
            }
            let input = File::from_raw_handle(in_wr as _);
            if CreatePipe(&mut out_rd, &mut out_wr, std::ptr::null(), 0) == 0 {
                CloseHandle(in_rd);
                return Err(os_err("CreatePipe (terminal output)"));
            }
            let output = File::from_raw_handle(out_rd as _);
            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(coord(c.cols, c.rows), in_rd, out_wr, PSEUDOCONSOLE_RESIZE_QUIRK, &mut hpc);
            // the console holds its own references now
            CloseHandle(in_rd);
            CloseHandle(out_wr);
            if hr < 0 {
                return Err(io::Error::other(format!("CreatePseudoConsole failed: HRESULT {hr:#x}")));
            }
            // from here on dropping `this` closes the console (and the process once set)
            let mut this = Self { hpc, process: null, pid: 0 };
            // the attribute list carrying the pseudoconsole
            let mut size = 0usize;
            InitializeProcThreadAttributeList(null, 1, 0, &mut size);
            // usize-aligned storage for the opaque list
            let mut attr_buf = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
            let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0 {
                return Err(os_err("InitializeProcThreadAttributeList"));
            }
            if UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const core::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                null,
                std::ptr::null(),
            ) == 0
            {
                let e = os_err("UpdateProcThreadAttribute(PSEUDOCONSOLE)");
                DeleteProcThreadAttributeList(attrs);
                return Err(e);
            }
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // no inherited stdio: the console is the pseudoconsole
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            si.lpAttributeList = attrs;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            // suspended: it runs only once it is inside its job (no grandchild escapes)
            let ok = CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                env.as_mut_ptr() as *const core::ffi::c_void,
                cwd.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            );
            DeleteProcThreadAttributeList(attrs);
            if ok == 0 {
                return Err(os_err(&format!("starting {:?} on a ConPTY (CreateProcessW)", argv[0])));
            }
            this.process = pi.hProcess;
            this.pid = pi.dwProcessId;
            let adopted = super::win::adopt(pi.dwProcessId, pi.hProcess, Some(pi.hThread));
            CloseHandle(pi.hThread);
            adopted?; // on failure the process was ended before it ran
            Ok((this, Box::new(output), Box::new(input)))
        }
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn exit_status(&mut self) -> io::Result<Option<super::TerminalExitStatus>> {
        Ok(self.try_wait()?.map(|code| (Some(code), None)))
    }
    pub fn wait_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Option<super::TerminalExitStatus>> {
        // Clamp below INFINITE (u32::MAX) so wait_timeout is always bounded.
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
        unsafe {
            match WaitForSingleObject(self.process, ms) {
                WAIT_OBJECT_0 => {
                    let mut code = 1u32;
                    if GetExitCodeProcess(self.process, &mut code) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(Some((Some(code), None)))
                }
                WAIT_TIMEOUT => Ok(None),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        unsafe {
            if WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 {
                return Ok(None);
            }
            let mut code = 1u32;
            if GetExitCodeProcess(self.process, &mut code) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(code))
        }
    }
    pub fn kill(&mut self) -> io::Result<()> {
        if unsafe { TerminateProcess(self.process, 1) } == 0 {
            let e = io::Error::last_os_error();
            // a process already on its way out (the job was just terminated: that is
            // asynchronous) refuses TerminateProcess with ERROR_ACCESS_DENIED; give the
            // exit a moment to land before calling it a failure
            if unsafe { WaitForSingleObject(self.process, 2000) } != WAIT_OBJECT_0 {
                return Err(e);
            }
        }
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let hr = unsafe { ResizePseudoConsole(self.hpc, coord(cols, rows)) };
        if hr < 0 { Err(io::Error::other(format!("ResizePseudoConsole failed: HRESULT {hr:#x}"))) } else { Ok(()) }
    }
}
