//! Preserve console records while separating startup-probe responses.

use std::io;
use std::ops::Range;

#[cfg(windows)]
pub(super) const MAX_WINDOWS_PROBE_RECORDS: usize = 64 * 1024;
const READ_BATCH: usize = 64;
const MAX_RESPONSE_BYTES: usize = 1024;
// Windows' left/right Alt, left/right Ctrl and Shift bits; lock states are not modifiers.
const ACTIVE_MODIFIERS: u32 = 0x1f;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug)]
pub(super) struct ConsoleKey {
    pub character: u16,
    pub down: bool,
    pub repeat: u16,
    pub modifiers: u32,
}

pub(super) struct BufferedConsoleInput<R> {
    records: Vec<R>,
    bytes: Vec<u8>,
    indices: Vec<usize>,
    limit: usize,
}

impl<R: Clone> BufferedConsoleInput<R> {
    pub(super) fn new(limit: usize) -> Self {
        Self { records: Vec::new(), bytes: Vec::new(), indices: Vec::new(), limit }
    }

    pub(super) fn push(&mut self, record: R, key: Option<ConsoleKey>) -> Result<(), R> {
        if self.records.len() >= self.limit {
            return Err(record);
        }
        if let Some(key) = key
            && key.down
            && key.repeat == 1
            && key.modifiers & ACTIVE_MODIFIERS == 0
            && let Ok(byte) = u8::try_from(key.character)
            && byte.is_ascii()
            && byte != 0
        {
            self.bytes.push(byte);
            self.indices.push(self.records.len());
        }
        self.records.push(record);
        Ok(())
    }

    pub(super) fn partition(&self) -> (Vec<u8>, Vec<R>) {
        let mut omitted = vec![false; self.records.len()];
        let mut responses = Vec::new();
        for range in response_ranges(&self.bytes) {
            responses.extend_from_slice(&self.bytes[range.clone()]);
            for &index in &self.indices[range] {
                omitted[index] = true;
            }
        }
        let preserved =
            self.records.iter().zip(omitted).filter(|(_, omit)| !*omit).map(|(record, _)| record.clone()).collect();
        (responses, preserved)
    }

    fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.records.len())
    }

    pub(super) fn complete(&self) -> bool {
        response_ranges(&self.bytes).into_iter().any(|range| crate::term_input::probe_complete(&self.bytes[range]))
    }
}

/// The platform adapter retains the original records; only their character view is inspected.
pub(super) trait ConsoleQueue<R> {
    fn available(&mut self) -> io::Result<usize>;
    fn read(&mut self, max: usize) -> io::Result<Vec<(R, Option<ConsoleKey>)>>;
    fn write(&mut self, records: &[R]) -> io::Result<usize>;
}

/// Bounded capture and retryable FIFO replay, including input queued after the last probe read.
pub(super) struct ConsoleReplay<R> {
    pub(super) input: BufferedConsoleInput<R>,
    responses: Vec<u8>,
    preserved: Option<Vec<R>>,
    written: usize,
    rotate_remaining: usize,
    rotating: Vec<R>,
    rotated: usize,
    finished: bool,
}

impl<R: Clone> ConsoleReplay<R> {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            input: BufferedConsoleInput::new(limit),
            responses: Vec::new(),
            preserved: None,
            written: 0,
            rotate_remaining: 0,
            rotating: Vec::new(),
            rotated: 0,
            finished: false,
        }
    }

    pub(super) fn full(&self) -> bool {
        self.input.remaining() == 0
    }

    pub(super) fn capture<Q: ConsoleQueue<R>>(&mut self, queue: &mut Q, max: usize) -> io::Result<usize> {
        let max = max.min(READ_BATCH).min(self.input.remaining());
        if max == 0 {
            return Ok(0);
        }
        let records = queue.read(max)?;
        if records.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "console read exceeded requested records"));
        }
        let count = records.len();
        for (record, key) in records {
            if self.input.push(record, key).is_err() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "console capture exceeded its limit"));
            }
        }
        Ok(count)
    }

    /// The queue appends writes, so take a bounded snapshot before replaying older records. If
    /// the snapshot exceeds the capture limit, rotate its unread tail behind the saved prefix.
    pub(super) fn restore<Q: ConsoleQueue<R>>(&mut self, queue: &mut Q) -> io::Result<&[u8]> {
        if self.finished {
            return Ok(&self.responses);
        }
        if self.preserved.is_none() {
            while !self.full() {
                let mut snapshot = queue.available()?;
                if snapshot == 0 {
                    break;
                }
                while snapshot > 0 && !self.full() {
                    let n = self.capture(queue, snapshot)?;
                    if n == 0 {
                        break;
                    }
                    snapshot -= n;
                }
                if snapshot > 0 && !self.full() {
                    break;
                }
            }
            let pending = queue.available()?;
            let (responses, preserved) = self.input.partition();
            self.responses = responses;
            self.rotate_remaining = if preserved.is_empty() { 0 } else { pending };
            self.preserved = Some(preserved);
        }
        let preserved = self.preserved.as_ref().expect("replay prepared");
        while self.written < preserved.len() {
            let remaining = &preserved[self.written..];
            let n = queue.write(remaining)?;
            if n == 0 || n > remaining.len() {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "console replay did not write records"));
            }
            self.written += n;
        }
        while self.rotate_remaining > 0 || self.rotated < self.rotating.len() {
            if self.rotated == self.rotating.len() {
                let max = self.rotate_remaining.min(READ_BATCH);
                let records = queue.read(max)?;
                if records.is_empty() || records.len() > max {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "console replay lost its queued snapshot",
                    ));
                }
                self.rotate_remaining -= records.len();
                self.rotating = records.into_iter().map(|(record, _)| record).collect();
                self.rotated = 0;
            }
            let remaining = &self.rotating[self.rotated..];
            let n = queue.write(remaining)?;
            if n == 0 || n > remaining.len() {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "console rotation did not write records"));
            }
            self.rotated += n;
        }
        self.finished = true;
        Ok(&self.responses)
    }
}

fn numbers(bytes: &[u8]) -> Option<Vec<u32>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() {
        return None;
    }
    text.split(';')
        .map(|part| if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) { None } else { part.parse().ok() })
        .collect()
}

fn valid_csi(params: &[u8], final_byte: u8) -> bool {
    match final_byte {
        b'c' => params.strip_prefix(b"?").and_then(numbers).is_some(),
        b'u' => params.strip_prefix(b"?").and_then(numbers).is_some_and(|n| n.len() == 1),
        b'y' => params
            .strip_prefix(b"?")
            .and_then(|p| p.strip_suffix(b"$"))
            .and_then(numbers)
            .is_some_and(|n| matches!(n.as_slice(), [2026 | 2027, 0..=4])),
        b't' => numbers(params).is_some_and(|n| matches!(n.as_slice(), [6, h, w] if *h > 0 && *w > 0)),
        b'R' => numbers(params).is_some_and(|n| matches!(n.as_slice(), [row, col] if *row > 0 && *col > 0)),
        _ => false,
    }
}

fn valid_osc(payload: &[u8]) -> bool {
    let Some(rgb) = payload.strip_prefix(b"11;rgb:") else { return false };
    let mut parts = rgb.split(|b| *b == b'/');
    let valid = |p: &[u8]| !p.is_empty() && p.len() <= 4 && p.iter().all(u8::is_ascii_hexdigit);
    matches!((parts.next(), parts.next(), parts.next(), parts.next()), (Some(r), Some(g), Some(b), None) if valid(r) && valid(g) && valid(b))
}

fn valid_dcs(payload: &[u8]) -> bool {
    if let Some(version) = payload.strip_prefix(b">|") {
        return !version.is_empty() && version.iter().all(|b| (0x20..=0x7e).contains(b));
    }
    [b"1+r".as_slice(), b"0+r".as_slice()].iter().any(|prefix| {
        payload.strip_prefix(*prefix).is_some_and(|rest| {
            [b"524742".as_slice(), b"5463".as_slice()].iter().any(|name| {
                rest.strip_prefix(*name).is_some_and(|tail| {
                    tail.is_empty()
                        || tail
                            .strip_prefix(b"=")
                            .is_some_and(|value| !value.is_empty() && value.iter().all(u8::is_ascii_hexdigit))
                })
            })
        })
    })
}

/// Return payload end and sequence end only for complete bounded OSC/DCS strings.
fn string_end(bytes: &[u8], start: usize, osc: bool) -> Option<(usize, usize)> {
    let bound = bytes.len().min(start.saturating_add(MAX_RESPONSE_BYTES));
    let mut i = start + 2;
    while i < bound {
        match bytes[i] {
            0x07 if osc => return Some((i, i + 1)),
            0x1b if i + 1 < bound && bytes[i + 1] == b'\\' => return Some((i, i + 2)),
            0x00..=0x1f => return None,
            _ => i += 1,
        }
    }
    None
}

fn response_ranges(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(PASTE_START) {
            let start = i + PASTE_START.len();
            let Some(end) = bytes[start..].windows(PASTE_END.len()).position(|w| w == PASTE_END) else { break };
            i = start + end + PASTE_END.len();
            continue;
        }
        if bytes[i..].starts_with(b"\x1b]") || bytes[i..].starts_with(b"\x1bP") {
            let osc = bytes[i + 1] == b']';
            if let Some((payload_end, end)) = string_end(bytes, i, osc) {
                let payload = &bytes[i + 2..payload_end];
                if if osc { valid_osc(payload) } else { valid_dcs(payload) } {
                    ranges.push(i..end);
                }
                i = end;
                continue;
            }
        } else if bytes[i..].starts_with(b"\x1b[") {
            let end = (i + 2..bytes.len().min(i.saturating_add(MAX_RESPONSE_BYTES)))
                .find(|&j| !(0x20..=0x3f).contains(&bytes[j]));
            if let Some(end) = end
                && (0x40..=0x7e).contains(&bytes[end])
            {
                if valid_csi(&bytes[i + 2..end], bytes[end]) {
                    ranges.push(i..end + 1);
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Record {
        Key { character: u16, down: bool, repeat: u16, modifiers: u32 },
        Focus(bool),
        Resize(u16, u16),
        Mouse { x: u16, y: u16, buttons: u32 },
    }

    fn key_view(record: &Record) -> Option<ConsoleKey> {
        match *record {
            Record::Key { character, down, repeat, modifiers } => {
                Some(ConsoleKey { character, down, repeat, modifiers })
            }
            _ => None,
        }
    }

    fn push(buffer: &mut BufferedConsoleInput<Record>, record: Record) {
        let key = key_view(&record);
        buffer.push(record, key).unwrap();
    }

    fn key(character: u16, down: bool, modifiers: u32) -> Record {
        Record::Key { character, down, repeat: 1, modifiers }
    }

    fn push_bytes(buffer: &mut BufferedConsoleInput<Record>, bytes: &[u8]) {
        for &byte in bytes {
            push(buffer, key(u16::from(byte), true, 0));
        }
    }

    #[test]
    fn preserves_typeahead_modifiers_unicode_release_and_focus_around_replies() {
        let mut b = BufferedConsoleInput::new(256);
        let before = key(b'a'.into(), true, 0x8);
        let focus = Record::Focus(true);
        let release = key(b'x'.into(), false, 0x10);
        let unicode = key(0x00e9, true, 0);
        let high = key(0xd83d, true, 0);
        let low = key(0xde00, true, 0);
        let resize = Record::Resize(120, 40);
        let mouse = Record::Mouse { x: 7, y: 4, buttons: 1 };
        push(&mut b, before.clone());
        let reply = b"\x1b]11;rgb:1234/abcd/ffff\x1b\\";
        push_bytes(&mut b, &reply[..6]);
        push(&mut b, focus.clone());
        push(&mut b, release.clone());
        push(&mut b, unicode.clone());
        push_bytes(&mut b, &reply[6..]);
        push(&mut b, high.clone());
        push(&mut b, low.clone());
        push(&mut b, resize.clone());
        push(&mut b, mouse.clone());
        let fence = b"\x1b[?62;4c";
        push_bytes(&mut b, fence);
        let after = key(b'b'.into(), true, 0x2);
        push(&mut b, after.clone());
        assert!(b.complete());
        let (responses, preserved) = b.partition();
        assert_eq!(responses, [reply.as_slice(), fence.as_slice()].concat());
        assert!(crate::term_input::strip_replies(&responses).is_empty());
        assert_eq!(crate::term_input::parse_osc11(&String::from_utf8_lossy(&responses)), Some([18, 171, 255]));
        assert_eq!(preserved, [before, focus, release, unicode, high, low, resize, mouse, after]);
    }

    #[test]
    fn collects_only_complete_recognized_probe_responses() {
        let mut b = BufferedConsoleInput::new(512);
        let responses = b"\x1b[?2026;1$y\x1b[?2027;2$y\x1bP1+r524742=31\x1b\\\x1bP0+r5463\x1b\\\x1bP>|terminal\x1b\\\x1b[6;20;10t\x1b[?1u\x1b[12;1R\x1b[?62;4c";
        let typed = b"z\x1b[1;5C";
        push_bytes(&mut b, typed);
        push_bytes(&mut b, responses);
        let (reply, preserved) = b.partition();
        assert_eq!(reply, responses);
        assert_eq!(preserved, typed.iter().map(|&c| key(c.into(), true, 0)).collect::<Vec<_>>());
        assert!(crate::term_input::strip_replies(&reply).is_empty());
        let probe = crate::term_input::parse_probe(&reply);
        assert_eq!(
            (probe.sync, probe.grapheme, probe.kitty, probe.cursor_row, probe.cell_px),
            (Some(true), Some(true), Some(1), Some(12), Some((10, 20)))
        );
    }

    #[test]
    fn preserves_malformed_partial_and_bracketed_paste_records() {
        for typed in [
            b"\x1b]11;rgb:1234/abcd".as_slice(),
            b"\x1b]11;not-a-color\x07".as_slice(),
            b"\x1b]52;clipboard\x07".as_slice(),
            b"\x1bP1+rDEAD=31\x1b\\".as_slice(),
            b"\x1bP>|unterminated".as_slice(),
            b"\x1b[?2026;bad$y".as_slice(),
            b"\x1b[?2026;9$y".as_slice(),
            b"\x1b[200~\x1b]11;rgb:f/f/f\x07\x1b[?62c\x1b[201~".as_slice(),
            b"\x1b[200~\x1b]11;rgb:f/f/f\x07\x1b[?62c".as_slice(),
        ] {
            let mut b = BufferedConsoleInput::new(2048);
            push_bytes(&mut b, typed);
            let (reply, preserved) = b.partition();
            assert!(!b.complete(), "paste and malformed input must not complete the probe");
            assert!(reply.is_empty(), "input={typed:?}, reply={reply:?}");
            assert_eq!(preserved, typed.iter().map(|&c| key(c.into(), true, 0)).collect::<Vec<_>>());
        }
    }

    #[test]
    fn returns_overflow_record_to_the_caller_without_dropping_it() {
        let mut b = BufferedConsoleInput::new(1);
        let first = Record::Focus(true);
        let second = key('界' as u16, true, 0x10);
        push(&mut b, first.clone());
        assert_eq!(b.push(second.clone(), None), Err(second));
        assert_eq!(b.partition().1, [first]);
    }

    #[test]
    fn preserves_long_truncated_and_modified_input_before_a_later_reply() {
        let mut b = BufferedConsoleInput::new(4096);
        let mut typed = b"\x1b]11;rgb:".to_vec();
        typed.extend(std::iter::repeat_n(b'f', MAX_RESPONSE_BYTES));
        typed.extend_from_slice(b"\x07\x1b]11;incomplete");
        push_bytes(&mut b, &typed);
        let modified = key(b'x'.into(), true, 0x8);
        let repeated = Record::Key { character: b'y'.into(), down: true, repeat: 3, modifiers: 0 };
        push(&mut b, modified.clone());
        push(&mut b, repeated.clone());
        let fence = b"\x1b[?62c";
        push_bytes(&mut b, fence);
        let (reply, preserved) = b.partition();
        assert_eq!(reply, fence);
        let mut expected = typed.iter().map(|&c| key(c.into(), true, 0)).collect::<Vec<_>>();
        expected.extend([modified, repeated]);
        assert_eq!(preserved, expected);
    }

    #[test]
    fn preserves_modified_keys_even_when_they_resemble_a_probe_reply() {
        let mut b = BufferedConsoleInput::new(64);
        let modified_escape = key(0x1b, true, 0x8);
        push(&mut b, modified_escape.clone());
        push_bytes(&mut b, b"[?62c");
        let real_reply = b"\x1b[?62c";
        push_bytes(&mut b, real_reply);
        let (reply, preserved) = b.partition();
        assert_eq!(reply, real_reply);
        let mut expected = vec![modified_escape];
        expected.extend(b"[?62c".iter().map(|&c| key(c.into(), true, 0)));
        assert_eq!(preserved, expected);
    }

    struct FakeQueue {
        records: VecDeque<Record>,
        max_write: usize,
        fail_on_write: Option<usize>,
        writes: usize,
    }

    impl ConsoleQueue<Record> for FakeQueue {
        fn available(&mut self) -> io::Result<usize> {
            Ok(self.records.len())
        }

        fn read(&mut self, max: usize) -> io::Result<Vec<(Record, Option<ConsoleKey>)>> {
            let mut records = Vec::new();
            for _ in 0..max {
                let Some(record) = self.records.pop_front() else { break };
                let key = key_view(&record);
                records.push((record, key));
            }
            Ok(records)
        }

        fn write(&mut self, records: &[Record]) -> io::Result<usize> {
            self.writes += 1;
            if self.fail_on_write == Some(self.writes) {
                return Err(io::Error::other("injected write failure"));
            }
            let n = records.len().min(self.max_write);
            self.records.extend(records[..n].iter().cloned());
            Ok(n)
        }
    }

    #[test]
    fn replay_keeps_late_records_fifo_and_does_not_duplicate_partial_writes() {
        let mut replay = ConsoleReplay::new(256);
        let first = key(b'a'.into(), true, 0x10);
        let after = Record::Focus(true);
        push(&mut replay.input, first.clone());
        let response = b"\x1b[?62c";
        push_bytes(&mut replay.input, response);
        push(&mut replay.input, after.clone());
        let late = [key('界' as u16, true, 0), key(b'x'.into(), false, 0x8), Record::Resize(90, 30)];
        let mut queue = FakeQueue { records: late.clone().into(), max_write: 1, fail_on_write: Some(2), writes: 0 };
        assert!(replay.restore(&mut queue).is_err());
        assert_eq!(replay.restore(&mut queue).unwrap(), response);
        let expected = [vec![first, after], late.to_vec()].concat();
        assert_eq!(queue.records.iter().cloned().collect::<Vec<_>>(), expected);
        assert_eq!(replay.restore(&mut queue).unwrap(), response);
        assert_eq!(queue.records.iter().cloned().collect::<Vec<_>>(), expected, "second restore must not replay twice");
    }

    #[test]
    fn replay_rotates_a_bounded_snapshot_without_losing_fifo_on_retry() {
        let mut replay = ConsoleReplay::new(3);
        let early = [key(b'a'.into(), true, 0x8), Record::Focus(true)];
        for record in &early {
            push(&mut replay.input, record.clone());
        }
        let late = [
            key(b'b'.into(), true, 0),
            Record::Resize(80, 24),
            key('界' as u16, true, 0),
            key(b'x'.into(), false, 0x10),
            key(b'c'.into(), true, 0),
        ];
        let mut queue = FakeQueue { records: late.clone().into(), max_write: 1, fail_on_write: Some(5), writes: 0 };
        assert!(replay.restore(&mut queue).is_err(), "injected failure must interrupt rotation");
        assert!(replay.restore(&mut queue).unwrap().is_empty());
        assert_eq!(queue.records.iter().cloned().collect::<Vec<_>>(), [early.to_vec(), late.to_vec()].concat());
    }

    #[test]
    fn replay_filters_replies_that_arrive_while_capturing_late_input() {
        struct GrowingQueue {
            inner: FakeQueue,
            later: Option<Vec<Record>>,
        }
        impl ConsoleQueue<Record> for GrowingQueue {
            fn available(&mut self) -> io::Result<usize> {
                self.inner.available()
            }
            fn read(&mut self, max: usize) -> io::Result<Vec<(Record, Option<ConsoleKey>)>> {
                let out = self.inner.read(max)?;
                if let Some(later) = self.later.take() {
                    self.inner.records.extend(later);
                }
                Ok(out)
            }
            fn write(&mut self, records: &[Record]) -> io::Result<usize> {
                self.inner.write(records)
            }
        }
        let early = key(b'a'.into(), true, 0x8);
        let queued = Record::Focus(true);
        let last = key('界' as u16, true, 0);
        let response = b"\x1b[?62c";
        let mut later = response.iter().map(|&c| key(c.into(), true, 0)).collect::<Vec<_>>();
        later.push(last.clone());
        let mut queue = GrowingQueue {
            inner: FakeQueue { records: [queued.clone()].into(), max_write: 64, fail_on_write: None, writes: 0 },
            later: Some(later),
        };
        let mut replay = ConsoleReplay::new(64);
        push(&mut replay.input, early.clone());
        assert_eq!(replay.restore(&mut queue).unwrap(), response);
        assert_eq!(queue.inner.records.into_iter().collect::<Vec<_>>(), [early, queued, last]);
    }

    #[test]
    fn replay_preserves_a_reply_split_at_the_record_limit() {
        let mut replay = ConsoleReplay::new(5);
        let first = key(b'a'.into(), true, 0);
        push(&mut replay.input, first.clone());
        push_bytes(&mut replay.input, b"\x1b]11");
        let tail = b";rgb:f/f/f\x07";
        let mut later = tail.iter().map(|&c| key(c.into(), true, 0)).collect::<Vec<_>>();
        later.push(Record::Focus(false));
        let mut queue = FakeQueue { records: later.clone().into(), max_write: 2, fail_on_write: None, writes: 0 };
        assert!(replay.restore(&mut queue).unwrap().is_empty());
        let mut expected = vec![first];
        expected.extend(b"\x1b]11".iter().map(|&c| key(c.into(), true, 0)));
        expected.extend(later);
        assert_eq!(queue.records.into_iter().collect::<Vec<_>>(), expected);
    }
}
