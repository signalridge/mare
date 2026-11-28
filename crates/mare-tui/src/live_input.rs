//! Streaming byte decoder for the live Unix terminal reader.
//!
//! Wraps [`termina::Parser`] so standard key, mouse, focus, paste, and escape-reply decoding stays
//! identical to `termina`, while preserving the Kitty CSI-u `base_layout_key` (the third
//! colon-separated code point in `CSI code:shifted:base;mods u`) that `termina::event::KeyEvent`
//! does not carry.

use crate::tty::Event;

/// Maximum bytes buffered for one in-progress CSI sequence before passing through unchanged.
const MAX_CSI_LEN: usize = 64;

/// Extract the Kitty CSI-u `base_layout_key` (3rd colon-separated code point) from a complete
/// `CSI ... u` byte sequence, e.g. `\x1b[1089::99;5u` -> `Some('c')`.
pub(crate) fn extract_csi_u_base_layout_key(seq: &[u8]) -> Option<char> {
    if seq.len() < 4 || !seq.starts_with(b"\x1b[") || !seq.ends_with(b"u") {
        return None;
    }
    let body = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
    let code_field = body.split(';').next()?;
    let mut parts = code_field.split(':');
    let _primary = parts.next()?;
    let _shifted = parts.next()?;
    let base_str = parts.next()?;
    if base_str.is_empty() {
        return None;
    }
    let cp = base_str.parse::<u32>().ok()?;
    char::from_u32(cp).filter(|c| !c.is_control())
}

#[derive(Default)]
pub(crate) struct LiveDecoder {
    parser: termina::Parser,
    pending_csi: Vec<u8>,
    pending_esc: Option<std::time::Instant>,
    in_paste: bool,
}

impl LiveDecoder {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn esc_deadline(&self) -> Option<std::time::Instant> {
        self.pending_esc.map(|at| at + crate::term_input::EscJoin::ESC_TIMEOUT)
    }

    pub(crate) fn flush_esc(&mut self, out: &mut Vec<Event>) -> Option<std::time::Instant> {
        let at = self.pending_esc.take()?;
        self.parser.parse(b"\x1b", false);
        self.drain_parser(None, out);
        Some(at)
    }

    pub(crate) fn flush_expired_esc(
        &mut self,
        now: std::time::Instant,
        out: &mut Vec<Event>,
    ) -> Option<std::time::Instant> {
        if self.pending_esc.is_some_and(|at| now >= at + crate::term_input::EscJoin::ESC_TIMEOUT) {
            self.flush_esc(out)
        } else {
            None
        }
    }

    fn drain_parser(&mut self, base_key: Option<char>, out: &mut Vec<Event>) {
        while let Some(ev) = self.parser.pop() {
            if matches!(ev, termina::Event::Paste(_)) {
                self.in_paste = false;
            }
            let mut mapped = Event::from(ev);
            if let (Some(base), Event::Key(k)) = (base_key, &mut mapped) {
                k.base_layout_key = Some(base);
            }
            out.push(mapped);
        }
    }

    #[cfg(test)]
    pub(crate) fn feed(&mut self, bytes: &[u8], more: bool, out: &mut Vec<Event>) {
        self.feed_at(bytes, more, std::time::Instant::now(), out);
    }

    pub(crate) fn feed_at(&mut self, bytes: &[u8], more: bool, now: std::time::Instant, out: &mut Vec<Event>) {
        let mut i = 0;
        if !bytes.is_empty() && self.pending_esc.take().is_some() {
            match bytes[0] {
                b'[' => {
                    self.pending_csi.extend_from_slice(b"\x1b[");
                    i = 1;
                }
                b'\x1b' => {
                    self.parser.parse(b"\x1b", false);
                    self.drain_parser(None, out);
                }
                _ => {
                    self.parser.parse(b"\x1b", true);
                }
            }
        }

        while i < bytes.len() {
            if self.in_paste {
                if let Some(rel) = find_subslice(&bytes[i..], b"\x1b[201~") {
                    let end = i + rel + 6;
                    self.parser.parse(&bytes[i..end], end < bytes.len() || more);
                    self.in_paste = false;
                    self.drain_parser(None, out);
                    i = end;
                    continue;
                }
                self.parser.parse(&bytes[i..], true);
                self.drain_parser(None, out);
                break;
            }

            if !self.pending_csi.is_empty() {
                let b = bytes[i];
                if (0x20..=0x3f).contains(&b) && self.pending_csi.len() < MAX_CSI_LEN {
                    self.pending_csi.push(b);
                    i += 1;
                    continue;
                }
                if (0x40..=0x7e).contains(&b) {
                    self.pending_csi.push(b);
                    i += 1;
                    let seq = std::mem::take(&mut self.pending_csi);
                    if seq == b"\x1b[200~" {
                        self.in_paste = true;
                        self.parser.parse(&seq, true);
                        self.drain_parser(None, out);
                    } else if b == b'~'
                        && let Some(key) = crate::term_input::decode_xterm_modify_other_keys(&seq)
                    {
                        out.push(Event::Key(key));
                    } else {
                        let base_key = (b == b'u').then(|| extract_csi_u_base_layout_key(&seq)).flatten();
                        let seq_more = if base_key.is_some() { false } else { i < bytes.len() || more };
                        self.parser.parse(&seq, seq_more);
                        self.drain_parser(base_key, out);
                    }
                    continue;
                }
                // Unexpected byte or length cap: flush buffered bytes verbatim and re-process `b`.
                let seq = std::mem::take(&mut self.pending_csi);
                self.parser.parse(&seq, true);
                self.drain_parser(None, out);
                continue;
            }

            if let Some(rel) = find_subslice(&bytes[i..], b"\x1b[") {
                if rel > 0 {
                    // Precedes `\x1b[`, so any trailing `\x1b` here is a standalone Escape, not CSI.
                    self.parser.parse(&bytes[i..i + rel], false);
                    self.drain_parser(None, out);
                }
                self.pending_csi.extend_from_slice(b"\x1b[");
                i += rel + 2;
                continue;
            }

            let tail = &bytes[i..];
            if tail.ends_with(b"\x1b") {
                let prefix = &tail[..tail.len() - 1];
                if !prefix.is_empty() {
                    self.parser.parse(prefix, false);
                    self.drain_parser(None, out);
                }
                self.pending_esc = Some(now);
                break;
            }

            self.parser.parse(tail, more);
            self.drain_parser(None, out);
            break;
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tty::{KeyCode, KeyEventKind, Modifiers};

    #[test]
    fn csi_u_preserves_base_layout_key_in_single_and_split_reads() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        // Cyrillic `Ctrl+с`: primary 1089 ('с'), shifted empty, base layout 99 ('c'), Ctrl (5).
        dec.feed(b"\x1b[1089::99;5u", false, &mut out);
        assert_eq!(out.len(), 1);
        let Event::Key(k) = out[0] else { panic!("expected key event, got {:?}", out[0]) };
        assert_eq!(k.code, KeyCode::Char('с'));
        assert_eq!(k.modifiers, Modifiers::CONTROL);
        assert_eq!(k.kind, KeyEventKind::Press);
        assert_eq!(k.base_layout_key, Some('c'));
        assert!(crate::keys::is_ctrl_c(&k));

        // Same sequence split across two raw reads.
        out.clear();
        dec.feed(b"\x1b[1089:", true, &mut out);
        assert!(out.is_empty());
        dec.feed(b":99;5u", false, &mut out);
        assert_eq!(out.len(), 1);
        let Event::Key(k2) = out[0] else { panic!("expected key event") };
        assert_eq!(k2.base_layout_key, Some('c'));
        assert!(crate::keys::is_ctrl_c(&k2));
    }

    #[test]
    fn bracketed_paste_followed_by_csi_u_in_same_chunk() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        dec.feed(b"\x1b[200~hello\x1b[1089::99;5u\x1b[201~\x1b[1089::99;5u", false, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], Event::Paste(p) if p == "hello\x1b[1089::99;5u"));
        let Event::Key(k) = out[1] else { panic!("expected trailing key event, got {:?}", out[1]) };
        assert_eq!(k.base_layout_key, Some('c'));
        assert!(crate::keys::is_ctrl_c(&k));
    }

    #[test]
    fn split_esc_before_bracketed_paste_start_decodes_as_paste() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        dec.feed_at(b"abc\x1b", false, t0, &mut out);
        assert_eq!(out.len(), 3, "trailing lone ESC must wait for follow-up bytes: {out:?}");
        assert!(dec.esc_deadline().is_some());

        dec.feed_at(b"[200~text\x1b[201~", false, t0 + std::time::Duration::from_millis(2), &mut out);
        assert_eq!(out.len(), 4, "expected 3 chars + 1 paste event, got {out:?}");
        assert!(matches!(&out[3], Event::Paste(p) if p == "text"), "expected Paste(\"text\"), got {:?}", out[3]);
        assert!(dec.esc_deadline().is_none());

        // Split right after ESC before CSI-u still preserves base_layout_key, and double ESC does
        // not swallow or corrupt the following bracketed paste.
        out.clear();
        dec.feed_at(b"\x1b", false, t0, &mut out);
        dec.feed_at(b"[1089::99;5u", false, t0 + std::time::Duration::from_millis(1), &mut out);
        assert_eq!(out.len(), 1);
        let Event::Key(k) = out[0] else { panic!("expected key event, got {:?}", out[0]) };
        assert_eq!(k.base_layout_key, Some('c'));

        out.clear();
        dec.feed_at(b"\x1b", false, t0, &mut out);
        dec.feed_at(b"\x1b[200~pasted\x1b[201~", false, t0 + std::time::Duration::from_millis(1), &mut out);
        assert_eq!(out.len(), 2, "expected Esc + Paste, got {out:?}");
        assert!(matches!(out[0], Event::Key(k) if k.code == KeyCode::Escape));
        assert!(matches!(&out[1], Event::Paste(p) if p == "pasted"));
    }

    #[test]
    fn lone_esc_flushes_after_timeout_and_before_late_followup() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();

        dec.feed_at(b"\x1b", false, t0, &mut out);
        assert!(out.is_empty());
        assert!(dec.flush_expired_esc(t0 + std::time::Duration::from_millis(5), &mut out).is_none());
        assert!(out.is_empty());
        assert_eq!(dec.flush_expired_esc(t0 + crate::term_input::EscJoin::ESC_TIMEOUT, &mut out), Some(t0));
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Event::Key(k) if k.code == KeyCode::Escape));

        // Follow-up arriving after ESC_TIMEOUT flushes via `flush_expired_esc` with the original
        // `ESC` arrival timestamp before decoding the subsequent bytes.
        out.clear();
        let late = t0 + crate::term_input::EscJoin::ESC_TIMEOUT + std::time::Duration::from_millis(1);
        dec.feed_at(b"\x1b", false, t0, &mut out);
        assert_eq!(dec.flush_expired_esc(late, &mut out), Some(t0));
        dec.feed_at(b"[200~text\x1b[201~", false, late, &mut out);
        assert!(matches!(out[0], Event::Key(k) if k.code == KeyCode::Escape));
        assert!(!out.iter().any(|e| matches!(e, Event::Paste(_))), "late follow-up must not form paste: {out:?}");
    }

    #[test]
    fn same_read_esc_before_bracketed_paste_start_decodes_esc_and_paste() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        dec.feed_at(b"\x1b\x1b[200~x\x1b[201~", false, std::time::Instant::now(), &mut out);
        assert_eq!(out.len(), 2, "expected Esc + Paste, got {out:?}");
        assert!(matches!(out[0], Event::Key(k) if k.code == KeyCode::Escape));
        assert!(matches!(&out[1], Event::Paste(p) if p == "x"));
    }

    #[test]
    fn expired_esc_after_incomplete_osc_clears_deadline_without_emitting() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        dec.feed_at(b"a\x1b]11;rgb:0/0/0\x1b", false, t0, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Event::Key(k) if k.code == KeyCode::Char('a')));
        assert!(dec.esc_deadline().is_some());

        out.clear();
        let expired = t0 + crate::term_input::EscJoin::ESC_TIMEOUT;
        assert_eq!(dec.flush_expired_esc(expired, &mut out), Some(t0));
        assert!(out.is_empty(), "incomplete OSC trailing ESC must not emit key: {out:?}");
        assert!(dec.esc_deadline().is_none());
    }

    #[test]
    fn repro_242_xterm_modified_keys_in_live_decoder() {
        let mut dec = LiveDecoder::default();
        let mut out = Vec::new();
        // xterm modifyOtherKeys: Shift+Tab (`CSI 27 ; 2 ; 9 ~`) and Ctrl+Enter (`CSI 27 ; 5 ; 13 ~`)
        dec.feed(b"\x1b[27;2;9~\x1b[27;5;13~", false, &mut out);
        assert_eq!(out.len(), 2, "expected 2 key events for xterm Shift+Tab and Ctrl+Enter, got {out:?}");
        assert!(
            matches!(out[0], Event::Key(k) if (k.code == KeyCode::BackTab || (k.code == KeyCode::Tab && k.modifiers.contains(Modifiers::SHIFT)))),
            "expected Shift+Tab, got {:?}",
            out[0]
        );
        assert!(
            matches!(out[1], Event::Key(k) if k.code == KeyCode::Enter && k.modifiers.contains(Modifiers::CONTROL)),
            "expected Ctrl+Enter, got {:?}",
            out[1]
        );

        // Split across raw reads (right after ESC and mid-parameter).
        out.clear();
        let t0 = std::time::Instant::now();
        dec.feed_at(b"a\x1b", false, t0, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Event::Key(k) if k.code == KeyCode::Char('a')));
        dec.feed_at(b"[27;2;", true, t0 + std::time::Duration::from_millis(1), &mut out);
        assert_eq!(out.len(), 1);
        dec.feed_at(b"13~b", false, t0 + std::time::Duration::from_millis(2), &mut out);
        assert_eq!(out.len(), 3, "expected a + Shift+Enter + b, got {out:?}");
        assert!(
            matches!(out[1], Event::Key(k) if k.code == KeyCode::Enter && k.modifiers == Modifiers::SHIFT),
            "expected split Shift+Enter, got {:?}",
            out[1]
        );
        assert!(matches!(out[2], Event::Key(k) if k.code == KeyCode::Char('b')));
    }
}
