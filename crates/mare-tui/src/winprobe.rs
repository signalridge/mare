//! Windows startup probe: the Unix probe reads stdin directly, so on
//! Windows the same batched query (`term_input::probe_query`: OSC 11 background, DECRQM
//! 2026/2027 + XTGETTCAP on known terminals, XTVERSION, cell size, kitty keyboard flags, the
//! cursor position report, fenced by DA1) goes through the raw console handles:
//! virtual-terminal input is switched on for the query, console input records are buffered until
//! the DA1 fence or deadline, and non-reply records are restored before the event reader starts.
//! A console without OSC 11 falls back to the screen buffer's color table.
//!
//! Copyright 2025 OpenAI (Apache-2.0).

use std::time::Duration;

#[cfg(any(windows, test))]
mod records;

/// `(fg, bg)` default colors from a console color table and screen-buffer attributes (pure).
pub fn decode_console_default_colors(attributes: u16, table: &[u32; 16]) -> ([u8; 3], [u8; 3]) {
    let fg = (attributes & 0x0f) as usize;
    let bg = ((attributes >> 4) & 0x0f) as usize;
    (decode_color_ref(table[fg]), decode_color_ref(table[bg]))
}

fn decode_color_ref(c: u32) -> [u8; 3] {
    [(c & 0xff) as u8, ((c >> 8) & 0xff) as u8, ((c >> 16) & 0xff) as u8]
}

/// Send `query` (a `term_input::probe_query`) and collect what comes back within `timeout`,
/// stopping at the DA1 fence. Returns only probe-reply bytes; console input records typed during
/// the query are replayed to the console. Returns `None` if probing or replay cannot be started.
pub fn probe(query: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        imp::probe(query, timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = (query, timeout);
        None
    }
}

/// The console's default background from its color table (when OSC 11 got no answer).
pub fn console_background() -> Option<[u8; 3]> {
    #[cfg(windows)]
    {
        imp::console_background()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFOEX, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetConsoleScreenBufferInfoEx,
        GetNumberOfConsoleInputEvents, GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode, WriteConsoleInputW,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::records::{ConsoleKey, ConsoleQueue, ConsoleReplay, MAX_WINDOWS_PROBE_RECORDS};

    pub fn probe(query: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        let output = std_handle(STD_OUTPUT_HANDLE)?;
        let input = std_handle(STD_INPUT_HANDLE)?;
        query_replies(input, output, query, timeout)
    }

    pub fn console_background() -> Option<[u8; 3]> {
        console_default(std_handle(STD_OUTPUT_HANDLE)?)
    }

    fn std_handle(kind: u32) -> Option<HANDLE> {
        // SAFETY: plain handle lookup
        let h = unsafe { GetStdHandle(kind) };
        (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(h)
    }

    struct VtInput {
        handle: HANDLE,
        original: u32,
    }

    impl VtInput {
        fn enable(handle: HANDLE) -> Option<Self> {
            let mut original = 0;
            // SAFETY: console mode get/set on a console handle; restored on drop
            unsafe {
                if GetConsoleMode(handle, &mut original) == 0 {
                    return None;
                }
                if SetConsoleMode(handle, original | ENABLE_VIRTUAL_TERMINAL_INPUT) == 0 {
                    return None;
                }
            }
            Some(Self { handle, original })
        }
    }

    impl Drop for VtInput {
        fn drop(&mut self) {
            // SAFETY: restoring the mode we read
            unsafe {
                SetConsoleMode(self.handle, self.original);
            }
        }
    }

    struct WinQueue(HANDLE);

    impl ConsoleQueue<INPUT_RECORD> for WinQueue {
        fn available(&mut self) -> io::Result<usize> {
            let mut pending = 0;
            // SAFETY: `self.0` is a console input handle and `pending` is a valid out pointer.
            if unsafe { GetNumberOfConsoleInputEvents(self.0, &mut pending) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pending as usize)
        }

        fn read(&mut self, max: usize) -> io::Result<Vec<(INPUT_RECORD, Option<ConsoleKey>)>> {
            let mut records = [INPUT_RECORD::default(); 64];
            let count = max.min(records.len());
            if count == 0 {
                return Ok(Vec::new());
            }
            let mut read = 0;
            // SAFETY: `records` has room for `count` records and `read` is a valid out pointer.
            let ok = unsafe { ReadConsoleInputW(self.0, records.as_mut_ptr(), count as u32, &mut read) };
            if ok == 0 && read == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(records
                .into_iter()
                .take(read as usize)
                .map(|record| {
                    let key = if record.EventType == KEY_EVENT as u16 {
                        // SAFETY: EventType selects KeyEvent; ReadConsoleInputW fills UnicodeChar.
                        let key = unsafe { record.Event.KeyEvent };
                        Some(ConsoleKey {
                            character: unsafe { key.uChar.UnicodeChar },
                            down: key.bKeyDown != 0,
                            repeat: key.wRepeatCount,
                            modifiers: key.dwControlKeyState,
                        })
                    } else {
                        None
                    };
                    (record, key)
                })
                .collect())
        }

        fn write(&mut self, records: &[INPUT_RECORD]) -> io::Result<usize> {
            let mut written = 0;
            let count = records.len().min(u32::MAX as usize);
            // SAFETY: `records` contains `count` valid input records; `written` is an out pointer.
            let ok = unsafe { WriteConsoleInputW(self.0, records.as_ptr(), count as u32, &mut written) };
            if ok == 0 && written == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(written as usize)
        }
    }

    struct ProbeInput {
        queue: WinQueue,
        replay: ConsoleReplay<INPUT_RECORD>,
    }

    impl Drop for ProbeInput {
        fn drop(&mut self) {
            if let Err(error) = self.replay.restore(&mut self.queue) {
                tracing::warn!(%error, "failed to restore Windows terminal input after probing");
            }
        }
    }

    fn query_replies(input: HANDLE, output: HANDLE, query: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        let _vt = VtInput::enable(input)?;
        // frames queued before the probe go out first, so the replies follow them
        crate::out::wait_idle(Duration::from_millis(100));
        write_all(output, query)?;
        let deadline = Instant::now() + timeout;
        let mut input = ProbeInput { queue: WinQueue(input), replay: ConsoleReplay::new(MAX_WINDOWS_PROBE_RECORDS) };
        loop {
            if input.replay.input.complete() || input.replay.full() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let ms = (deadline.saturating_duration_since(now).as_millis().min(u32::MAX as u128) as u32).max(1);
            // SAFETY: waiting on the console input handle; input records are read by WinQueue.
            match unsafe { WaitForSingleObject(input.queue.0, ms) } {
                WAIT_OBJECT_0 => {
                    let pending = match input.queue.available() {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if input.replay.capture(&mut input.queue, pending).is_err() {
                        break;
                    }
                }
                WAIT_TIMEOUT => break,
                _ => break,
            }
        }
        input.replay.restore(&mut input.queue).ok().map(<[u8]>::to_vec)
    }

    fn console_default(output: HANDLE) -> Option<[u8; 3]> {
        // SAFETY: zeroed out-struct with its size set, as the API requires
        let mut info = unsafe { std::mem::zeroed::<CONSOLE_SCREEN_BUFFER_INFOEX>() };
        info.cbSize = std::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>() as u32;
        if unsafe { GetConsoleScreenBufferInfoEx(output, &mut info) } == 0 {
            return None;
        }
        Some(super::decode_console_default_colors(info.wAttributes, &info.ColorTable).1)
    }

    fn write_all(handle: HANDLE, mut bytes: &[u8]) -> Option<()> {
        while !bytes.is_empty() {
            let mut written = 0;
            // SAFETY: writing a byte buffer to the console output handle
            let ok = unsafe {
                WriteFile(
                    handle,
                    bytes.as_ptr().cast(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                return None;
            }
            bytes = &bytes[written as usize..];
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> [u32; 16] {
        [
            0x00000000, 0x00000080, 0x00008000, 0x00008080, 0x00800000, 0x00800080, 0x00808000, 0x00c0c0c0, 0x00808080,
            0x000000ff, 0x0000ff00, 0x0000ffff, 0x00ff0000, 0x00ff00ff, 0x00ffff00, 0x00ffffff,
        ]
    }

    #[test]
    fn console_colors_decode_attribute_indices_and_colorref_byte_order() {
        //
        assert_eq!(decode_console_default_colors(0x21, &table()), ([128, 0, 0], [0, 128, 0]));
        assert_eq!(decode_console_default_colors(0xe9, &table()), ([255, 0, 0], [0, 255, 255]));
        let mut t = table();
        t[3] = 0x00112233;
        t[4] = 0x00aabbcc;
        assert_eq!(decode_console_default_colors(0x43, &t), ([0x33, 0x22, 0x11], [0xcc, 0xbb, 0xaa]));
    }
}
