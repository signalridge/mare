//! Account-runner transport; modified for mare stdio and job ownership.
//! Copyright 2025 OpenAI (Apache-2.0).

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[path = "ipc_io_win.rs"]
mod pipe_io;
use pipe_io::Pipe;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

use super::{last, wide};
use crate::winsandbox::RunnerArgs;
use crate::winsandbox::runner_ipc::{SpawnRequest, VERSION, read_frame, write_frame};

const TIMEOUT: Duration = Duration::from_secs(15);
const PREFIX: &str = r"\\.\pipe\mare-runner-";

fn file(handle: HANDLE) -> Result<File, String> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(last("opening runner pipe"))
    } else {
        Ok(unsafe { File::from_raw_handle(handle.cast()) })
    }
}

fn duplicate(handle: HANDLE) -> Result<File, String> {
    let mut copied = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(GetCurrentProcess(), handle, GetCurrentProcess(), &mut copied, 0, 0, DUPLICATE_SAME_ACCESS)
    } == 0
    {
        return Err(last("DuplicateHandle(runner stdio)"));
    }
    file(copied)
}

/// Validate and own every relay endpoint before account creation or SpawnRequest delivery.
pub(super) fn prepare_stdio(handles: [HANDLE; 3]) -> Result<[File; 3], String> {
    let mut files = Vec::new();
    for handle in handles {
        let file = duplicate(handle)?;
        if unsafe { GetFileType(file.as_raw_handle().cast()) } == FILE_TYPE_UNKNOWN {
            return Err("runner stdio is not a file, pipe, or console stream".into());
        }
        files.push(file);
    }
    Ok(files.try_into().unwrap())
}

pub(super) fn eof_input() -> Result<File, String> {
    let (read, write) = anonymous_pipe()?;
    drop(write);
    Ok(read)
}

fn io(error: std::io::Error) -> String {
    format!("runner IPC: {error}")
}

/// A nonce ties the two local pipe names to one invocation; process IDs authenticate peers.
fn pipe_names() -> [String; 2] {
    let base = format!("{PREFIX}{}-{:032x}", unsafe { GetCurrentProcessId() }, rand::random::<u128>());
    [format!("{base}-in"), format!("{base}-out")]
}

fn validate_names(names: &[String]) -> Result<u32, String> {
    if names.len() != 2 {
        return Err("runner IPC requires two pipe names".into());
    }
    let stem = names[0].strip_suffix("-in").ok_or("bad runner input pipe")?;
    if names[1] != format!("{stem}-out") {
        return Err("runner IPC pipe pair mismatch".into());
    }
    let suffix = stem.strip_prefix(PREFIX).ok_or("bad runner pipe prefix")?;
    let (pid, nonce) = suffix.split_once('-').ok_or("bad runner pipe identity")?;
    if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("bad runner pipe nonce".into());
    }
    pid.parse().map_err(|_| "bad runner server PID".into())
}

pub(super) struct Server {
    names: [String; 2],
    input: Pipe,
    output: Pipe,
}

impl Server {
    pub fn new(account_sid: &str) -> Result<Self, String> {
        let names = pipe_names();
        // Only the chosen sandbox account may connect.
        // The creating server retains its own handles; no extra user/SYSTEM ACE is needed.
        let sddl = wide(format!("D:P(A;;GA;;;{account_sid})"));
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last("runner pipe security descriptor"));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let make = |name: &str, access| {
            file(unsafe {
                CreateNamedPipeW(
                    wide(name).as_ptr(),
                    access | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    65536,
                    65536,
                    0,
                    &attributes,
                )
            })
        };
        let input = make(&names[0], PIPE_ACCESS_OUTBOUND);
        let output =
            if input.is_ok() { make(&names[1], PIPE_ACCESS_INBOUND) } else { Err("input pipe creation failed".into()) };
        unsafe { LocalFree(descriptor) };
        let deadline = Instant::now() + TIMEOUT;
        Ok(Self { names, input: Pipe::new(input?, deadline), output: Pipe::new(output?, deadline) })
    }

    pub fn args(&self) -> Vec<String> {
        vec![
            crate::winsandbox::RUNNER_ARG.into(),
            "--pipe-in".into(),
            self.names[0].clone(),
            "--pipe-out".into(),
            self.names[1].clone(),
        ]
    }

    fn connect(pipe: &Pipe, pid: u32, _process: HANDLE) -> Result<(), String> {
        pipe.connect().map_err(io)?;
        let mut actual = 0;
        if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle().cast(), &mut actual) } == 0 || actual != pid {
            return Err("runner pipe client PID mismatch".into());
        }
        Ok(())
    }

    pub fn relay(mut self, pid: u32, process: HANDLE, args: &[String], stdio: [File; 3]) -> Result<i32, String> {
        Self::connect(&self.input, pid, process)?;
        Self::connect(&self.output, pid, process)?;
        let request = SpawnRequest {
            version: VERSION,
            args: args.to_vec(),
            env: std::env::vars_os().map(|(k, v)| (k.encode_wide().collect(), v.encode_wide().collect())).collect(),
        };
        let payload = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        write_frame(&mut self.input, 0, &payload).map_err(io)?;
        // The same deadline covers connections, the entire request write, and SpawnReady.
        let [mut stdin, mut stdout, mut stderr] = stdio;
        let (kind, data) = read_frame(&mut self.output).map_err(io)?;
        if kind == 4 {
            return Err(String::from_utf8_lossy(&data).into_owned());
        }
        if kind != 5 || !data.is_empty() {
            return Err("expected runner SpawnReady".into());
        }
        self.input.running();
        self.output.running();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stopped);
        let cancel_input = self.input.cancellation();
        let input = std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while !worker_stop.load(Ordering::SeqCst) {
                let Ok(size) = stdin.read(&mut buffer) else { break };
                if size == 0 || worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                if self.input.write_all(&buffer[..size]).is_err() {
                    break;
                }
            }
        });
        let result = (|| {
            loop {
                let (kind, data) = read_frame(&mut self.output).map_err(io)?;
                match kind {
                    1 => stdout.write_all(&data).map_err(io)?,
                    2 => stderr.write_all(&data).map_err(io)?,
                    3 if data.len() == 4 => return Ok(i32::from_le_bytes(data.try_into().unwrap())),
                    4 => return Err(String::from_utf8_lossy(&data).into_owned()),
                    _ => return Err("invalid runner response frame".into()),
                }
            }
        })();
        // Stop before cancellation so the worker cannot issue a subsequent read/write.
        // Overlapped pipe operations are canceled/drained; synchronous host reads are canceled
        // across their start race and joined. No detached thread retains a pipe or stdio handle.
        stopped.store(true, Ordering::SeqCst);
        cancel_input.cancel();
        while !input.is_finished() {
            unsafe { CancelSynchronousIo(input.as_raw_handle().cast()) };
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = input.join();
        result
    }
}

fn client_pipe(name: &str, access: u32, expected_pid: u32) -> Result<Pipe, String> {
    let pipe = file(unsafe {
        CreateFileW(
            wide(name).as_ptr(),
            access,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    })?;
    let mut actual = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle().cast(), &mut actual) } == 0 || actual != expected_pid {
        return Err("runner pipe server PID mismatch".into());
    }
    Ok(Pipe::new(pipe, Instant::now() + TIMEOUT))
}

fn anonymous_pipe() -> Result<(File, File), String> {
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(last("CreatePipe(runner bridge)"));
    }
    Ok((file(read)?, file(write)?))
}

fn output_worker(mut read: File, output: Arc<Mutex<Pipe>>, kind: u8) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(size) = read.read(&mut buffer) {
            if size == 0 {
                break;
            }
            if write_frame(&mut *output.lock().unwrap(), kind, &buffer[..size]).is_err() {
                break;
            }
        }
    })
}

pub(super) fn run_client(raw: &[String]) -> Result<i32, String> {
    if raw.len() != 4 || raw[0] != "--pipe-in" || raw[2] != "--pipe-out" {
        return Err("invalid account runner pipe arguments".into());
    }
    let names = [raw[1].clone(), raw[3].clone()];
    let pid = validate_names(&names)?;
    let input = client_pipe(&names[0], GENERIC_READ, pid)?;
    let output = client_pipe(&names[1], GENERIC_WRITE, pid)?;
    let output = Arc::new(Mutex::new(output));
    let result = client_request(input, Arc::clone(&output));
    let mut output = output.lock().unwrap();
    match result {
        Ok(code) => {
            write_frame(&mut *output, 3, &code.to_le_bytes()).map_err(io)?;
            Ok(code)
        }
        Err(error) => {
            let _ = write_frame(&mut *output, 4, error.as_bytes());
            Err(error)
        }
    }
}

fn client_request(mut input: Pipe, output: Arc<Mutex<Pipe>>) -> Result<i32, String> {
    let (kind, data) = read_frame(&mut input).map_err(io)?;
    if kind != 0 {
        return Err("expected runner spawn request".into());
    }
    let request: SpawnRequest = serde_json::from_slice(&data).map_err(|error| error.to_string())?;
    if request.version != VERSION {
        return Err("unsupported runner protocol".into());
    }
    let args = RunnerArgs::parse(&request.args)?;
    if args.as_user.is_some() || args.account.is_none() {
        return Err("IPC request must target an account inner runner".into());
    }
    // Pass the request environment directly to child creation, including hidden =Drive
    // entries and non-Unicode UTF-16. Never mutate process-global environment around threads.
    let (stdin, mut stdin_write) = anonymous_pipe()?;
    let (stdout_read, stdout) = anonymous_pipe()?;
    let (stderr_read, stderr) = anonymous_pipe()?;
    for (which, handle) in [(STD_INPUT_HANDLE, &stdin), (STD_OUTPUT_HANDLE, &stdout), (STD_ERROR_HANDLE, &stderr)] {
        if unsafe { SetStdHandle(which, handle.as_raw_handle().cast()) } == 0 {
            return Err(last("SetStdHandle(runner bridge)"));
        }
    }
    input.running();
    let mut stdout_read = Some(stdout_read);
    let mut stderr_read = Some(stderr_read);
    let mut output_workers = None;
    let cancel_input = input.cancellation();
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stopped);
    let input_worker = std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while !worker_stop.load(Ordering::SeqCst) {
            let Ok(size) = input.read(&mut buffer) else { break };
            if size == 0 || worker_stop.load(Ordering::SeqCst) {
                break;
            }
            let mut written = 0;
            while written < size && !worker_stop.load(Ordering::SeqCst) {
                match stdin_write.write(&buffer[written..size]) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => written += count,
                }
            }
        }
    });
    let result = crate::winsandbox::win::run_with_env_ready(&args, &request.env, &mut || {
        let mut sender = output.lock().unwrap();
        write_frame(&mut *sender, 5, &[]).map_err(io)?;
        sender.running();
        drop(sender);
        output_workers = Some((
            output_worker(stdout_read.take().unwrap(), Arc::clone(&output), 1),
            output_worker(stderr_read.take().unwrap(), Arc::clone(&output), 2),
        ));
        Ok(())
    });
    stopped.store(true, Ordering::SeqCst);
    cancel_input.cancel();
    while !input_worker.is_finished() {
        unsafe { CancelSynchronousIo(input_worker.as_raw_handle().cast()) };
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = input_worker.join();
    drop((stdin, stdout, stderr));
    if let Some((stdout_worker, stderr_worker)) = output_workers {
        let _ = stdout_worker.join();
        let _ = stderr_worker.join();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_matching_nonce_pipe_pairs_are_accepted() {
        let names = pipe_names();
        assert_eq!(validate_names(&names).unwrap(), unsafe { GetCurrentProcessId() });
        assert!(validate_names(&[names[0].clone(), format!("{}x", names[1])]).is_err());
        assert!(validate_names(&[r"\\remote\pipe\x-in".into(), r"\\remote\pipe\x-out".into()]).is_err());
    }
    #[test]
    fn pipe_server_rejects_a_client_with_an_unexpected_process_id() {
        if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
            return;
        }
        let (user, _) = crate::winsandbox::win::current_sids().unwrap();
        let sid = crate::winsandbox::win::sid_string(user.ptr()).unwrap();
        let server = Server::new(&sid).unwrap();
        let name = server.names[0].clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (close_tx, close_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let pipe = client_pipe(&name, GENERIC_READ, unsafe { GetCurrentProcessId() }).unwrap();
            ready_tx.send(()).unwrap();
            close_rx.recv().unwrap();
            drop(pipe);
        });
        ready_rx.recv_timeout(TIMEOUT).unwrap();
        let error = Server::connect(&server.input, 0, unsafe { GetCurrentProcess() }).unwrap_err();
        assert!(error.contains("PID mismatch"), "{error}");
        close_tx.send(()).unwrap();
        client.join().unwrap();
    }
    fn local_pair() -> (Server, Pipe, Pipe) {
        let (user, _) = crate::winsandbox::win::current_sids().unwrap();
        let sid = crate::winsandbox::win::sid_string(user.ptr()).unwrap();
        let server = Server::new(&sid).unwrap();
        let pid = unsafe { GetCurrentProcessId() };
        let input = client_pipe(&server.names[0], GENERIC_READ, pid).unwrap();
        let output = client_pipe(&server.names[1], GENERIC_WRITE, pid).unwrap();
        Server::connect(&server.input, pid, unsafe { GetCurrentProcess() }).unwrap();
        Server::connect(&server.output, pid, unsafe { GetCurrentProcess() }).unwrap();
        (server, input, output)
    }

    fn enabled() -> bool {
        std::env::var("CI").is_ok() || std::env::var("MARE_WIN_SANDBOX_TEST").is_ok()
    }

    #[test]
    fn startup_deadline_covers_connected_peer_that_does_not_read_and_partial_frame() {
        if !enabled() {
            return;
        }
        let (mut server, _input, mut output) = local_pair();
        server.input.set_deadline(Instant::now() + Duration::from_millis(40));
        let error = write_frame(&mut server.input, 0, &vec![1; 262144]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        output.write_all(&[0, 100, 0, 0, 0, 1]).unwrap();
        server.output.set_deadline(Instant::now() + Duration::from_millis(40));
        assert_eq!(read_frame(&mut server.output).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn complete_request_larger_than_pipe_buffer_and_cancellation_races_are_reclaimed() {
        if !enabled() {
            return;
        }
        let (mut server, mut input, _output) = local_pair();
        let (user, _) = crate::winsandbox::win::current_sids().unwrap();
        let account = crate::winsandbox::win::sid_string(user.ptr()).unwrap();
        let request = SpawnRequest {
            version: VERSION,
            args: RunnerArgs {
                mode: "read-only".into(),
                sids: vec!["S-1-4-1-2-2".into()],
                cwd: "C:/".into(),
                argv: vec!["cmd.exe".into(), "/c".into(), "echo ready".into()],
                as_user: None,
                account: Some(account),
                pty: None,
            }
            .to_args(),
            env: (0..16).map(|index| (format!("ENV_{index}").encode_utf16().collect(), vec![0x78; 4096])).collect(),
        };
        RunnerArgs::parse(&request.args).unwrap();
        let payload = serde_json::to_vec(&request).unwrap();
        assert!(payload.len() > 65536);
        let reader = std::thread::spawn(move || read_frame(&mut input).unwrap());
        write_frame(&mut server.input, 0, &payload).unwrap();
        assert_eq!(reader.join().unwrap(), (0, payload));
        for _ in 0..16 {
            let (server, _input, _output) = local_pair();
            let mut pipe = server.output;
            let cancellation = pipe.cancellation();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let reader = std::thread::spawn(move || {
                worker_barrier.wait();
                pipe.read(&mut [0]).unwrap_err()
            });
            barrier.wait();
            cancellation.cancel();
            assert_eq!(reader.join().unwrap().kind(), std::io::ErrorKind::ConnectionAborted);
        }
    }

    #[test]
    fn invalid_outer_stdio_and_explicit_eof_are_checked_without_host_input() {
        if !enabled() {
            return;
        }
        let stdin = eof_input().unwrap();
        let (mut read, mut write) = anonymous_pipe().unwrap();
        write.write_all(b"unread-host-input").unwrap();
        for index in 0..3 {
            let mut handles =
                [stdin.as_raw_handle().cast(), write.as_raw_handle().cast(), write.as_raw_handle().cast()];
            handles[index] = std::ptr::null_mut();
            assert!(prepare_stdio(handles).is_err());
        }
        let mut eof = eof_input().unwrap();
        assert_eq!(eof.read(&mut [0]).unwrap(), 0);
        let mut marker = [0; 17];
        read.read_exact(&mut marker).unwrap();
        assert_eq!(&marker, b"unread-host-input");
    }
    #[test]
    fn pipe_dacl_contains_only_the_target_account() {
        if !enabled() {
            return;
        }
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce};
        let (user, _) = crate::winsandbox::win::current_sids().unwrap();
        let sid = crate::winsandbox::win::sid_string(user.ptr()).unwrap();
        let server = Server::new(&sid).unwrap();
        unsafe {
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            assert_eq!(
                GetSecurityInfo(
                    server.input.as_raw_handle().cast(),
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor
                ),
                0
            );
            assert!(!dacl.is_null());
            assert_eq!((*dacl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            assert_ne!(GetAce(dacl, 0, &mut ace), 0);
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            assert_ne!(EqualSid((&ace.SidStart as *const u32).cast_mut().cast(), user.ptr()), 0);
            LocalFree(descriptor);
        }
    }
    #[test]
    fn relay_cancellation_controller_does_not_suppress_input_eof() {
        if !enabled() {
            return;
        }
        let (server, mut input, _output) = local_pair();
        let cancellation = server.input.cancellation();
        drop(server.input);
        assert_eq!(input.read(&mut [0]).unwrap(), 0);
        cancellation.cancel();
    }
}
