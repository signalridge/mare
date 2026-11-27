//! The terminal control-sequence trust boundary: one streaming filter for untrusted
//! text (model output, tool and process output, file names, plugin/MCP names, peer messages).
//! Combined rule set:
//!
//! - CSI, OSC, DCS, APC, PM, SOS (dropped up to their terminator), two-byte ESC sequences, C1
//!   controls (U+0080–U+009F, also as raw 8-bit bytes) and C0 controls other than `\n` / `\t`
//!   are removed; only SGR (`ESC [ … m`) may be kept, and it is parsed into styled spans
//!   ([`sgr_spans`]) rather than handed to the terminal;
//! - `\r\n` and a lone `\r` become `\n`;
//! - a sequence split across chunks is joined before it is judged; pending bytes are bounded,
//!   and an unterminated sequence is discarded up to its terminator.

use crate::style::{Color, Line, Span, Style};

/// Longest CSI parameter run we buffer; a longer one is discarded (no unbounded pending state).
const MAX_CSI: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    Ground,
    Esc,
    /// collecting a CSI (params + intermediates) to decide whether it is SGR
    Csi,
    /// discarding a CSI up to its final byte (too long, or not wanted)
    CsiSkip,
    /// OSC: discard up to BEL or ST
    Osc,
    /// DCS / APC / PM / SOS: discard up to ST
    Str,
    /// ESC seen inside an OSC/string: `\` ends it, anything else continues discarding
    StrEsc {
        osc: bool,
    },
    /// ESC + intermediate bytes (0x20–0x2F): discard up to the final byte
    EscInter,
}

/// Streaming filter. Feed chunks in order; [`StreamFilter::finish`] flushes a trailing `\r`.
#[derive(Clone, Debug)]
pub struct StreamFilter {
    keep_sgr: bool,
    st: St,
    csi: Vec<u8>,
    /// a `\r` waiting to see whether `\n` follows
    cr: bool,
    /// UTF-8: bytes of the current multi-byte character seen so far, and how many it needs
    utf8: Vec<u8>,
    need: usize,
}

impl StreamFilter {
    /// `keep_sgr`: keep `ESC [ … m` color sequences (for display, parsed by [`sgr_spans`]);
    /// otherwise every escape sequence is removed (text for the model, labels).
    pub fn new(keep_sgr: bool) -> Self {
        Self { keep_sgr, st: St::Ground, csi: vec![], cr: false, utf8: vec![], need: 0 }
    }

    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            self.byte(b, out);
        }
    }

    /// End of input: a pending `\r` becomes `\n`; incomplete sequences are dropped.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.cr {
            out.push(b'\n');
            self.cr = false;
        }
        self.st = St::Ground;
        self.csi.clear();
        self.utf8.clear();
        self.need = 0;
    }

    fn emit(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.cr {
            self.cr = false;
            out.push(b'\n');
            if b == b'\n' {
                return; // `\r\n` → `\n`
            }
        }
        if b == b'\r' {
            self.cr = true;
            return;
        }
        out.push(b);
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        // 8-bit string terminator / C1 controls arriving as UTF-8 (C2 80–C2 9F) are handled in
        // the ground-state UTF-8 decoder below; escape states work on ASCII bytes
        match self.st {
            St::Ground => self.ground(b, out),
            St::Esc => match b {
                b'[' => {
                    self.st = St::Csi;
                    self.csi.clear();
                }
                b']' => self.st = St::Osc,
                b'P' | b'_' | b'^' | b'X' => self.st = St::Str,
                0x20..=0x2F => self.st = St::EscInter,
                0x1B => {}                 // ESC ESC: restart
                _ => self.st = St::Ground, // two-byte sequence (or stray byte): dropped
            },
            St::EscInter => {
                if (0x30..=0x7E).contains(&b) {
                    self.st = St::Ground;
                } else if !(0x20..=0x2F).contains(&b) {
                    self.st = St::Ground;
                }
            }
            St::Csi => {
                if (0x40..=0x7E).contains(&b) {
                    // final byte
                    let sgr = b == b'm' && self.csi.iter().all(|c| c.is_ascii_digit() || *c == b';' || *c == b':');
                    if sgr && self.keep_sgr {
                        self.flush_cr(out);
                        out.extend_from_slice(b"\x1b[");
                        out.extend_from_slice(&self.csi);
                        out.push(b'm');
                    }
                    self.csi.clear();
                    self.st = St::Ground;
                } else if (0x20..=0x3F).contains(&b) && self.csi.len() < MAX_CSI {
                    self.csi.push(b);
                } else if (0x20..=0x3F).contains(&b) {
                    self.csi.clear();
                    self.st = St::CsiSkip;
                } else {
                    // a control byte or non-ASCII inside a CSI: malformed, drop the sequence
                    self.csi.clear();
                    self.st = St::Ground;
                    if b == 0x1B {
                        self.st = St::Esc;
                    }
                }
            }
            St::CsiSkip => {
                if (0x40..=0x7E).contains(&b) || !(0x20..=0x3F).contains(&b) {
                    self.st = if b == 0x1B { St::Esc } else { St::Ground };
                }
            }
            St::Osc => match b {
                0x07 => self.st = St::Ground,
                0x1B => self.st = St::StrEsc { osc: true },
                _ => {}
            },
            St::Str => {
                if b == 0x1B {
                    self.st = St::StrEsc { osc: false };
                }
            }
            St::StrEsc { osc } => {
                self.st = if b == b'\\' {
                    St::Ground
                } else if osc {
                    St::Osc
                } else {
                    St::Str
                };
            }
        }
    }

    fn flush_cr(&mut self, out: &mut Vec<u8>) {
        if self.cr {
            self.cr = false;
            out.push(b'\n');
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.need > 0 {
            if (0x80..=0xBF).contains(&b) {
                self.utf8.push(b);
                if self.utf8.len() == self.need {
                    let seq = std::mem::take(&mut self.utf8);
                    self.need = 0;
                    // C2 80–C2 9F = U+0080–U+009F: C1 controls (8-bit CSI, OSC, ST, …) — drop
                    if !(seq.len() == 2 && seq[0] == 0xC2 && seq[1] <= 0x9F) {
                        self.flush_cr(out);
                        out.extend_from_slice(&seq);
                    }
                }
                return;
            }
            // invalid continuation: drop the partial character and handle `b` afresh
            self.utf8.clear();
            self.need = 0;
        }
        match b {
            0x1B => self.st = St::Esc,
            b'\n' | b'\t' | b'\r' => self.emit(b, out),
            0x00..=0x1F | 0x7F => {} // other C0 controls and DEL
            0x80..=0xBF => {}        // lone continuation byte / raw 8-bit C1: drop
            0xC2..=0xDF => {
                self.utf8 = vec![b];
                self.need = 2;
            }
            0xE0..=0xEF => {
                self.utf8 = vec![b];
                self.need = 3;
            }
            0xF0..=0xF4 => {
                self.utf8 = vec![b];
                self.need = 4;
            }
            0xC0 | 0xC1 | 0xF5..=0xFF => {} // never valid in UTF-8
            _ => self.emit(b, out),
        }
    }
}

/// Filter a complete string (all escape sequences removed, or SGR kept when `keep_sgr`).
pub fn sanitize(s: &str, keep_sgr: bool) -> String {
    let mut f = StreamFilter::new(keep_sgr);
    let mut out = Vec::with_capacity(s.len());
    f.feed(s.as_bytes(), &mut out);
    f.finish(&mut out);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Labels (names, titles, paths shown in the UI or the terminal title): escape sequences,
/// control, bidi and invisible characters removed, whitespace collapsed, length capped.
pub fn label(s: &str, max_chars: usize) -> String {
    let clean = sanitize(s, false);
    let mut out = String::with_capacity(clean.len());
    let mut space = false;
    for c in clean.chars() {
        if is_invisible(c) {
            continue;
        }
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
    }
    out
}

/// Bidi controls, zero-width and other invisible format characters (spoofing in labels).
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}' // zero-width space/joiners, LRM, RLM
        | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
        | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
        | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
        | '\u{FEFF}' | '\u{061C}' | '\u{00AD}' | '\u{180E}')
}

/// A hyperlink target the UI may emit as OSC 8: http/https only, no control or whitespace
/// characters. Anything else is shown as plain text.
pub fn valid_link(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && url.len() <= 2048
        && url.len() > if lower.starts_with("https://") { 8 } else { 7 }
        && !url.chars().any(|c| c.is_control() || c.is_whitespace() || is_invisible(c))
}

/// Parse text that may contain SGR sequences (already filtered: no other escapes) into spans
/// layered on `base`. Any other escape that slipped through is
/// dropped, never passed on.
pub fn sgr_spans(text: &str, base: Style) -> Vec<Span> {
    if !text.contains('\x1b') {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut out = vec![];
    let mut st = base;
    let mut cur = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('\x1b') {
        cur.push_str(&rest[..i]);
        rest = &rest[i..];
        // ESC [ params m
        let end = rest.as_bytes().iter().skip(2).position(|b| (0x40..=0x7E).contains(b)).map(|p| p + 2);
        match end {
            Some(e) if rest.as_bytes().get(1) == Some(&b'[') && rest.as_bytes()[e] == b'm' => {
                if !cur.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut cur), st));
                }
                st = apply_sgr(st, base, &rest[2..e]);
                rest = &rest[e + 1..];
            }
            Some(e) => rest = &rest[e + 1..],
            None => rest = "",
        }
    }
    cur.push_str(rest);
    if !cur.is_empty() {
        out.push(Span::styled(cur, st));
    }
    out
}

/// A whole line of SGR-bearing text.
pub fn sgr_line(text: &str, base: Style) -> Line {
    Line::new(sgr_spans(text, base))
}

fn apply_sgr(mut st: Style, base: Style, params: &str) -> Style {
    let nums: Vec<u16> =
        if params.is_empty() { vec![0] } else { params.split([';', ':']).map(|p| p.parse().unwrap_or(0)).collect() };
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => st = Style { link: st.link, ..base },
            1 => st.bold = true,
            2 => st.dim = true,
            3 => st.italic = true,
            4 => st.underline = true,
            7 => st.reverse = true,
            9 => st.strike = true,
            22 => {
                st.bold = false;
                st.dim = false;
            }
            23 => st.italic = false,
            24 => st.underline = false,
            27 => st.reverse = false,
            29 => st.strike = false,
            n @ 30..=37 => st.fg = Some(Color::Ansi((n - 30) as u8)),
            39 => st.fg = base.fg,
            n @ 40..=47 => st.bg = Some(Color::Ansi((n - 40) as u8)),
            49 => st.bg = base.bg,
            n @ 90..=97 => st.fg = Some(Color::Ansi((n - 90 + 8) as u8)),
            n @ 100..=107 => st.bg = Some(Color::Ansi((n - 100 + 8) as u8)),
            n @ (38 | 48) => {
                let c = match nums.get(i + 1) {
                    Some(5) => {
                        i += 2;
                        nums.get(i).map(|v| Color::Ansi(*v as u8))
                    }
                    Some(2) => {
                        let rgb = (nums.get(i + 2), nums.get(i + 3), nums.get(i + 4));
                        i += 4;
                        match rgb {
                            (Some(r), Some(g), Some(b)) => Some(Color::Rgb(*r as u8, *g as u8, *b as u8)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if n == 38 {
                    st.fg = c.or(st.fg);
                } else {
                    st.bg = c.or(st.bg);
                }
            }
            _ => {}
        }
        i += 1;
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]], keep: bool) -> String {
        let mut f = StreamFilter::new(keep);
        let mut out = vec![];
        for c in chunks {
            f.feed(c, &mut out);
        }
        f.finish(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn every_sequence_class_is_removed() {
        let hostile = concat!(
            "a\x1b]52;c;ZXZpbA==\x07",       // OSC 52 clipboard (BEL)
            "b\x1b]0;pwned\x1b\\",           // OSC 0 title (ST)
            "c\x1b[2J\x1b[H\x1b[10;10H",     // CSI clear / cursor moves
            "d\x1bPq#0;2;0;0;0\x1b\\",       // DCS (sixel)
            "e\x1b_Gf=24;AAAA\x1b\\",        // APC (kitty graphics)
            "f\x1b^pm\x1b\\g\x1bXsos\x1b\\", // PM, SOS
            "h\x1b(0\x1bc\x1b7",             // charset, RIS, DECSC
            "i\u{9b}31m\u{9d}0;t\u{9c}",     // 8-bit C1 as UTF-8
            "j\x07\x08\x00\x7f",             // C0 + DEL
            "k"
        );
        assert_eq!(sanitize(hostile, true), "abcdefghi31m0;tjk");
    }

    #[test]
    fn sgr_is_kept_only_when_asked_and_split_chunks_join() {
        assert_eq!(run(&[b"x\x1b[3", b"1mred\x1b", b"[0m"], true), "x\x1b[31mred\x1b[0m");
        assert_eq!(run(&[b"x\x1b[3", b"1mred\x1b", b"[0m"], false), "xred");
        // an OSC split across chunks is still discarded whole
        assert_eq!(run(&[b"a\x1b]52;c;", b"AAAA", b"\x1b", b"\\b"], true), "ab");
        // a UTF-8 C1 split across chunks
        assert_eq!(run(&[b"a\xc2", b"\x9bb"], true), "ab");
    }

    #[test]
    fn carriage_returns_become_newlines() {
        assert_eq!(sanitize("a\r\nb\rc\r", false), "a\nb\nc\n");
        assert_eq!(run(&[b"a\r", b"\nb"], false), "a\nb");
    }

    #[test]
    fn unterminated_and_oversized_sequences_are_dropped() {
        assert_eq!(sanitize("ok\x1b]52;c;never-ends", false), "ok");
        assert_eq!(sanitize("ok\x1bPdcs-never-ends", false), "ok");
        let long = format!("a\x1b[{}mb", "1;".repeat(200));
        assert_eq!(sanitize(&long, true), "ab");
        assert_eq!(sanitize("ünï\u{1F600}", false), "ünï\u{1F600}");
    }

    #[test]
    fn labels_drop_bidi_and_collapse_whitespace() {
        assert_eq!(label("evil\u{202E}txt.exe  \t name\x1b[31m", 64), "eviltxt.exe name");
        assert_eq!(label("abcdef", 4), "abc…");
    }

    #[test]
    fn only_http_links_are_valid() {
        assert!(valid_link("https://example.com/a?b"));
        assert!(!valid_link("javascript:alert(1)"));
        assert!(!valid_link("file:///etc/passwd"));
        assert!(!valid_link("https://x.dev/\x1b]8;;"));
        assert!(!valid_link("https://"));
    }

    #[test]
    fn sgr_becomes_spans() {
        let s = sgr_spans("a\x1b[1;31mb\x1b[0mc\x1b[38;2;1;2;3md", Style::default());
        assert_eq!(s.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        assert!(s[1].style.bold && s[1].style.fg == Some(Color::Ansi(1)));
        assert_eq!(s[2].style, Style::default());
        assert_eq!(s[3].style.fg, Some(Color::Rgb(1, 2, 3)));
    }
}
