//! Windows Job Objects: children start suspended and run only once inside their job.

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
};

/// pid → job handle (as isize: HANDLE is a raw pointer). Jobs are created with
/// KILL_ON_JOB_CLOSE, so when mare exits — however it exits — the OS closes the handles and
/// every tree still running goes with them.
static JOBS: Mutex<Option<HashMap<u32, isize>>> = Mutex::new(None);

fn last() -> io::Error {
    io::Error::last_os_error()
}

/// Resume every thread of a process started with CREATE_SUSPENDED (tokio's `Child` doesn't
/// hand out the main-thread handle `CreateProcessW` returned).
fn resume(pid: u32) -> io::Result<()> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap as isize == -1 {
            return Err(last());
        }
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &mut e) != 0;
        let mut resumed = 0;
        while ok {
            if e.th32OwnerProcessID == pid {
                let t = OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID);
                if !t.is_null() {
                    ResumeThread(t);
                    CloseHandle(t);
                    resumed += 1;
                }
            }
            ok = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        if resumed == 0 { Err(io::Error::other("could not resume the new process")) } else { Ok(()) }
    }
}

/// Tests: the job handle of `pid`.
#[cfg(test)]
pub fn job_of(pid: u32) -> Option<isize> {
    JOBS.lock().unwrap().as_ref().and_then(|m| m.get(&pid).copied())
}

/// Tests: take the job handle of `pid` out of the registry (the caller closes it).
#[cfg(test)]
pub fn take_job(pid: u32) -> Option<isize> {
    JOBS.lock().unwrap().as_mut().and_then(|m| m.remove(&pid))
}

/// Tests: make Job setup fail (the error path can't be provoked from outside).
#[cfg(test)]
pub static FAIL_JOB_SETUP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn job_setup_fails_in_test() -> bool {
    #[cfg(test)]
    {
        FAIL_JOB_SETUP.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// A process still suspended because it couldn't be put in its job: end it before it runs
/// a single instruction — never resume it unconfined (a Job setup failure is a
/// start failure, nothing escapes into the background).
fn end_suspended(pid: u32, process: HANDLE) {
    unsafe {
        if TerminateProcess(process, 1) == 0 {
            let p = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !p.is_null() {
                TerminateProcess(p, 1);
                CloseHandle(p);
            }
        }
    }
}

/// Put `process`, started with CREATE_SUSPENDED, into a new KILL_ON_JOB_CLOSE job and only
/// then resume it: through `main_thread` (the handle `CreateProcessW` returned, ConPTY
/// children of [`super::Terminal`]) or, without one, every thread of the process. Any
/// failure ends the process.
pub fn adopt(pid: u32, process: HANDLE, main_thread: Option<HANDLE>) -> io::Result<()> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            let e = last();
            end_suspended(pid, process);
            return Err(io::Error::new(e.kind(), format!("creating the Job Object: {e}")));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // without KILL_ON_JOB_CLOSE the tree would outlive mare: a failure is a spawn failure
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
            || job_setup_fails_in_test()
        {
            let e = last();
            CloseHandle(job);
            // the new process is still suspended: end it rather than let it run unconfined
            end_suspended(pid, process);
            return Err(io::Error::new(e.kind(), format!("setting up the Job Object: {e}")));
        }
        if AssignProcessToJobObject(job, process) == 0 {
            let e = last();
            CloseHandle(job);
            end_suspended(pid, process);
            return Err(io::Error::new(e.kind(), format!("assigning the process to its Job Object: {e}")));
        }
        JOBS.lock().unwrap().get_or_insert_with(HashMap::new).insert(pid, job as isize);
    }
    // only now may the process run: anything it starts is already inside the job
    match main_thread {
        Some(t) => {
            if unsafe { ResumeThread(t) } == u32::MAX {
                let e = last();
                end_suspended(pid, process);
                return Err(io::Error::new(e.kind(), format!("resuming the new process: {e}")));
            }
            Ok(())
        }
        None => resume(pid),
    }
}

/// Whether the job of `pid` still has active processes. `terminate` keeps the job handle until
/// the tree is gone (TerminateJobObject is asynchronous), so this can confirm the stop.
pub fn alive(pid: u32) -> bool {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation, QueryInformationJobObject,
    };
    let job = JOBS.lock().unwrap().as_ref().and_then(|m| m.get(&pid).copied());
    let Some(j) = job else { return false };
    unsafe {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(
            j as HANDLE,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            return true; // can't tell: don't claim it stopped
        }
        if info.ActiveProcesses == 0 {
            if let Some(h) = JOBS.lock().unwrap().as_mut().and_then(|m| m.remove(&pid)) {
                CloseHandle(h as HANDLE);
            }
            return false;
        }
        true
    }
}

/// End the whole tree. Without a job (not started through `spawn`), the root only. The job
/// handle is kept (until [`alive`] sees it empty) so the stop can be confirmed.
pub fn terminate(pid: u32) -> io::Result<()> {
    let job = JOBS.lock().unwrap().as_ref().and_then(|m| m.get(&pid).copied());
    unsafe {
        match job {
            Some(j) => {
                if TerminateJobObject(j as HANDLE, 1) == 0 {
                    Err(last())
                } else {
                    Ok(())
                }
            }
            None => {
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if h.is_null() {
                    return Err(last());
                }
                let r = TerminateProcess(h, 1);
                CloseHandle(h);
                if r == 0 { Err(last()) } else { Ok(()) }
            }
        }
    }
}
