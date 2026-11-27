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
    in_paste: bool,
}

impl LiveDecoder {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
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

    pub(crate) fn feed(&mut self, bytes: &[u8], more: bool, out: &mut Vec<Event>) {
        let mut i = 0;
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
                    self.parser.parse(&bytes[i..i + rel], true);
                    self.drain_parser(None, out);
                }
                self.pending_csi.extend_from_slice(b"\x1b[");
                i += rel + 2;
                continue;
            }

            self.parser.parse(&bytes[i..], more);
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
}
