//! Owned, cancellable named-pipe I/O for the bounded startup handshake,
//! using Win32 overlapped operations so cancellation drains each operation before releasing buffers and handles.
//! Copyright 2025 OpenAI (Apache-2.0).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

fn win_error(code: u32) -> io::Error {
    if code == ERROR_OPERATION_ABORTED {
        // read_exact/write_all retry Interrupted; cancellation must terminate those loops.
        io::Error::new(io::ErrorKind::ConnectionAborted, "runner pipe operation cancelled (Windows 995)")
    } else {
        io::Error::from_raw_os_error(code as i32)
    }
}

pub(super) struct Pipe {
    handle: Arc<File>,
    stopped: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

pub(super) struct Cancellation {
    // A controller must not keep a finished write endpoint alive: that would suppress EOF.
    // Upgrade only during cancellation so the HANDLE cannot be closed/reused under CancelIoEx.
    handle: Weak<File>,
    stopped: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.upgrade() {
            unsafe { CancelIoEx(handle.as_raw_handle().cast(), std::ptr::null()) };
        }
    }
}

impl AsRawHandle for Pipe {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }
}

impl Pipe {
    pub fn new(file: File, deadline: Instant) -> Self {
        Self { handle: Arc::new(file), stopped: Arc::new(AtomicBool::new(false)), deadline: Some(deadline) }
    }
    #[cfg(test)]
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }
    pub fn running(&mut self) {
        self.deadline = None;
    }
    pub fn cancellation(&self) -> Cancellation {
        Cancellation { handle: Arc::downgrade(&self.handle), stopped: Arc::clone(&self.stopped) }
    }
    fn operation(&self, start: impl FnOnce(*mut OVERLAPPED, *mut u32) -> i32, connect: bool) -> io::Result<u32> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(win_error(ERROR_OPERATION_ABORTED));
        }
        if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "runner startup deadline expired"));
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(win_error(unsafe { GetLastError() }));
        }
        // Each operation owns its event and OVERLAPPED until completion or drained cancellation.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event;
        let mut bytes = 0;
        let started = start(&mut overlapped, &mut bytes);
        let error = unsafe { GetLastError() };
        let result = if started != 0 {
            if connect || unsafe { GetOverlappedResult(self.as_raw_handle().cast(), &overlapped, &mut bytes, 0) } != 0 {
                Ok(bytes)
            } else {
                Err(win_error(unsafe { GetLastError() }))
            }
        } else if connect && error == ERROR_PIPE_CONNECTED {
            Ok(0)
        } else if error != ERROR_IO_PENDING {
            Err(win_error(error))
        } else {
            // Cancellation may race the start call. Recheck after registration, before waiting.
            if self.stopped.load(Ordering::SeqCst) {
                unsafe { CancelIoEx(self.as_raw_handle().cast(), &overlapped) };
            }
            let timeout = self.deadline.map_or(u32::MAX, |deadline| {
                deadline.saturating_duration_since(Instant::now()).as_millis().min((u32::MAX - 1) as u128) as u32
            });
            let wait = unsafe { WaitForSingleObject(event, timeout) };
            if wait != WAIT_OBJECT_0 {
                unsafe { CancelIoEx(self.as_raw_handle().cast(), &overlapped) };
                // Named-pipe cancellation must complete before the stack-owned buffer/event
                // can be freed. No detached worker or outstanding operation survives return.
                unsafe { GetOverlappedResult(self.as_raw_handle().cast(), &overlapped, &mut bytes, 1) };
                Err(io::Error::new(
                    if wait == WAIT_TIMEOUT { io::ErrorKind::TimedOut } else { io::ErrorKind::Other },
                    "runner pipe operation did not complete",
                ))
            } else if unsafe { GetOverlappedResult(self.as_raw_handle().cast(), &overlapped, &mut bytes, 0) } == 0 {
                Err(win_error(unsafe { GetLastError() }))
            } else {
                Ok(bytes)
            }
        };
        unsafe { CloseHandle(event) };
        result
    }
    pub fn connect(&self) -> io::Result<()> {
        self.operation(|overlapped, _| unsafe { ConnectNamedPipe(self.as_raw_handle().cast(), overlapped) }, true)
            .map(|_| ())
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match self.operation(
            |overlapped, bytes| unsafe {
                ReadFile(
                    self.as_raw_handle().cast(),
                    buffer.as_mut_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    bytes,
                    overlapped,
                )
            },
            false,
        ) {
            Ok(bytes) => Ok(bytes as usize),
            Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            Err(error) => Err(error),
        }
    }
}
impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.operation(
            |overlapped, bytes| unsafe {
                WriteFile(
                    self.as_raw_handle().cast(),
                    buffer.as_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    bytes,
                    overlapped,
                )
            },
            false,
        )
        .map(|bytes| bytes as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
