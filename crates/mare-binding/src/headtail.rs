//! Capped output buffer that keeps the beginning *and* the end of a command's output, dropping the
//! middle (Apache-2.0). The head matters: compilers and test runners often print the first
//! error at the top, and a tail-only buffer loses it.

use std::collections::VecDeque;

/// Room kept for the omission marker so `finish()` stays within `max` bytes.
const MARKER_RESERVE: usize = 48;

pub struct HeadTail {
    head_budget: usize,
    tail_budget: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: usize,
}

/// Marker inserted where the middle was dropped.
pub fn marker(omitted: usize) -> String {
    format!("\n... {omitted} bytes omitted ...\n")
}

impl HeadTail {
    /// Retain at most `max` bytes (marker included): half head, half tail.
    pub fn new(max: usize) -> Self {
        let usable = if max > MARKER_RESERVE * 4 { max - MARKER_RESERVE } else { max };
        let head_budget = usable / 2;
        Self { head_budget, tail_budget: usable - head_budget, head: Vec::new(), tail: VecDeque::new(), omitted: 0 }
    }

    pub fn push(&mut self, data: &[u8]) {
        let room = self.head_budget.saturating_sub(self.head.len());
        let (h, rest) = data.split_at(room.min(data.len()));
        self.head.extend_from_slice(h);
        if rest.is_empty() {
            return;
        }
        self.tail.extend(rest);
        if self.tail.len() > self.tail_budget {
            let over = self.tail.len() - self.tail_budget;
            self.tail.drain(..over);
            self.omitted += over;
        }
    }

    /// Bytes dropped from the middle so far.
    pub fn omitted(&self) -> usize {
        self.omitted
    }

    /// head + (marker if anything was dropped) + tail.
    pub fn finish(self) -> Vec<u8> {
        let mut out = self.head;
        let mut tail: Vec<u8> = self.tail.into();
        let mut omitted = self.omitted;
        if omitted > 0 {
            // the cut never splits a character or an escape sequence: the head ends at a whole
            // UTF-8 character and before an unterminated escape; the tail starts at a character
            // boundary
            let keep = whole_prefix(&out);
            omitted += out.len() - keep;
            out.truncate(keep);
            let skip = tail.iter().take(3).take_while(|&&b| (0x80..0xC0).contains(&b)).count();
            omitted += skip;
            tail.drain(..skip);
            out.extend_from_slice(marker(omitted).as_bytes());
        }
        out.extend(tail);
        out
    }
}

/// Length of the longest prefix that ends at a whole UTF-8 character and outside an unfinished
/// escape sequence (CSI `ESC [ … final`, OSC `ESC ] … BEL|ESC \\`, two-byte `ESC x`).
fn whole_prefix(b: &[u8]) -> usize {
    let mut end = match std::str::from_utf8(b) {
        Ok(_) => b.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => b.len(),
    };
    if let Some(esc) = b[..end].iter().rposition(|&c| c == 0x1b) {
        let rest = &b[esc + 1..end];
        let done = match rest.first() {
            None => false,
            Some(b'[') => rest[1..].iter().any(|c| (0x40..=0x7e).contains(c)),
            Some(b']') => rest.contains(&0x07) || rest.windows(2).any(|w| w == b"\x1b\\"),
            Some(_) => true,
        };
        if !done {
            end = esc;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_head_and_tail_and_marks_the_gap() {
        let mut b = HeadTail::new(1000);
        b.push(b"FIRST ERROR\n");
        for i in 0..10_000 {
            b.push(format!("line {i}\n").as_bytes());
        }
        b.push(b"LAST LINE\n");
        assert!(b.omitted() > 0);
        let out = String::from_utf8(b.finish()).unwrap();
        assert!(out.starts_with("FIRST ERROR\n"), "head lost");
        assert!(out.ends_with("LAST LINE\n"), "tail lost");
        assert!(out.contains("bytes omitted"));
        assert!(out.len() <= 1000, "{} > max", out.len());
    }

    #[test]
    fn the_cut_never_splits_a_character_or_an_escape() {
        // head ends inside "中" (3 bytes) and inside an SGR sequence
        let mut b = HeadTail::new(MARKER_RESERVE * 4 + 20);
        let head_budget = b.head_budget;
        let mut data = vec![b'a'; head_budget - 4];
        data.extend_from_slice(b"\x1b[3");
        data.extend_from_slice("中".as_bytes());
        data.extend(std::iter::repeat_n(b'z', 400));
        data.extend_from_slice("尾".as_bytes());
        b.push(&data);
        let out = b.finish();
        let text = String::from_utf8(out).expect("valid UTF-8 after the cut");
        assert!(!text.contains('\x1b'), "no half escape: {text:?}");
        assert!(text.ends_with("尾"));
    }

    #[test]
    fn small_output_is_untouched() {
        let mut b = HeadTail::new(1000);
        b.push(b"hello ");
        b.push(b"world");
        assert_eq!(b.omitted(), 0);
        assert_eq!(b.finish(), b"hello world");
    }
}
