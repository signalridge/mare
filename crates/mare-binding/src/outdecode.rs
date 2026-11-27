//! Ported output decoder (MIT — Copyright (c) 2025 Mario Zechner, (c) 2026 contributors).
//! On platforms other than Windows a span that is not valid UTF-8 is decoded with charset
//! detection (Apache-2.0: chardetng plus its IBM866 → Windows-1252 smart-punctuation fix)
//! instead of lossy UTF-8.
//!
//! Incremental text decoding for child-process output.
//!
//! Output arrives in arbitrary read-sized chunks, so a multi-byte character can
//! straddle two reads; decoding each chunk on its own turns it into `U+FFFD`.
//! [`StreamDecoder`] retains incomplete characters across reads.
//!
//! With a non-UTF-8 (or not yet resolved) fallback, only the ASCII prefix of
//! each segment is emitted immediately. From its first non-ASCII byte onward,
//! all bytes are held until `\n`, `\r`, end of stream, or the pending limit is
//! exceeded. At that point valid UTF-8 wins; otherwise the whole pending span
//! uses the fallback. ASCII after a non-ASCII byte must also be held: it may be
//! a legacy encoding's trailing byte. Both `\n` and `\r` are safe boundaries
//! for UTF-8 and legacy CJK encodings; `\r` also flushes progress displays.
//!
//! The limit bounds buffering for long lines without delimiters. Once exceeded,
//! a UTF-8 character cut by the limit is completed (at most 3 more bytes) before
//! deciding, and the decision applies until the next `\n` or `\r`, with
//! incomplete characters carried by the chosen decoder. Decisions use a fixed byte count,
//! independent of read boundaries. A known UTF-8 fallback (the non-Windows
//! default) has no ambiguity and streams immediately, retaining only incomplete
//! trailing characters.
//!
//! Windows resolves its console code page lazily, only when fallback is needed;
//! UTF-8 and console-encoded output can coexist in one command's output.

use encoding_rs::{Decoder, Encoding};

// Bound memory and latency for undelimited output, while allowing ordinary
// non-ASCII lines to be classified as a whole. Decide at LIMIT + 1 bytes, or up
// to 3 bytes later when that cuts a UTF-8 character.
const PENDING_LIMIT: usize = 64 * 1024;

/// Decodes one output stream chunk by chunk. Create one per stream.
pub struct StreamDecoder {
    /// None means the Windows console encoding has not been queried yet.
    fallback: Option<&'static Encoding>,
    /// Undecided bytes starting at the segment's first non-ASCII byte.
    pending: Vec<u8>,
    /// The committed decoder, including UTF-8 when selected at the limit.
    /// It carries incomplete characters until the next read or segment end.
    legacy: Option<Decoder>,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    /// Decoder whose fallback is the platform's console encoding.
    pub fn new() -> Self {
        Self {
            // undecided everywhere: Windows asks the console code page once; elsewhere every
            // non-UTF-8 span is guessed on its own
            fallback: None,
            pending: Vec::new(),
            legacy: None,
        }
    }

    /// Decoder that decodes lines which are not UTF-8 in `fallback`.
    pub fn with_fallback(fallback: &'static Encoding) -> Self {
        Self { fallback: Some(fallback), pending: Vec::new(), legacy: None }
    }

    /// Emit the unambiguous prefix, holding undecided non-ASCII spans as above.
    pub fn push(&mut self, mut bytes: &[u8]) -> String {
        let mut out = String::new();
        while !bytes.is_empty() {
            let (mut line, ends_line) = match bytes.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(end) => (&bytes[..=end], true),
                None => (bytes, false),
            };
            if self.legacy.is_none() && self.fallback == Some(encoding_rs::UTF_8) {
                self.legacy = Some(encoding_rs::UTF_8.new_decoder_without_bom_handling());
            }
            if let Some(decoder) = self.legacy.as_mut() {
                decode_legacy(decoder, line, ends_line, &mut out);
                bytes = &bytes[line.len()..];
                if ends_line {
                    self.legacy = None;
                }
                continue;
            }
            if self.pending.is_empty() {
                let ascii = line.iter().position(|b| !b.is_ascii()).unwrap_or(line.len());
                out.push_str(std::str::from_utf8(&line[..ascii]).unwrap_or_default());
                bytes = &bytes[ascii..];
                line = &line[ascii..];
                if line.is_empty() {
                    continue;
                }
            }
            // Past the limit, take one byte at a time until UTF-8 validity is
            // decided, so the decision point does not depend on reads.
            let room = (PENDING_LIMIT + 1).saturating_sub(self.pending.len()).max(1);
            let count = line.len().min(room);
            self.pending.extend_from_slice(&line[..count]);
            bytes = &bytes[count..];
            let last = ends_line && count == line.len();
            if last || (self.pending.len() > PENDING_LIMIT && utf8_decided(&self.pending)) {
                self.commit_pending(last, &mut out);
            }
        }
        out
    }

    fn commit_pending(&mut self, last: bool, out: &mut String) {
        if self.pending.is_empty() {
            return;
        }
        let encoding = if std::str::from_utf8(&self.pending).is_ok() {
            encoding_rs::UTF_8
        } else if let Some(f) = self.fallback {
            f
        } else if cfg!(windows) {
            *self.fallback.get_or_insert_with(console_encoding)
        } else {
            guess_encoding(&self.pending)
        };
        let mut decoder = encoding.new_decoder_without_bom_handling();
        decode_legacy(&mut decoder, &self.pending, last, out);
        self.pending.clear();
        if !last {
            self.legacy = Some(decoder);
        }
    }

    /// Decode whatever is still held back; call once at end of stream.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if let Some(mut decoder) = self.legacy.take() {
            decode_legacy(&mut decoder, &[], true, &mut out);
        } else {
            self.commit_pending(true, &mut out);
        }
        out
    }
}

/// Whether `bytes` is valid UTF-8 or contains an invalid sequence, as opposed
/// to valid UTF-8 cut inside its last character (at most 3 bytes).
fn utf8_decided(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).map_or_else(|err| err.error_len().is_some(), |_| true)
}

/// Decode a whole buffer at once (e.g. a probe's captured stdout).
pub fn decode_output(bytes: &[u8]) -> String {
    let mut decoder = StreamDecoder::new();
    let mut out = decoder.push(bytes);
    out.push_str(&decoder.finish());
    out
}

fn decode_legacy(decoder: &mut Decoder, bytes: &[u8], last: bool, out: &mut String) {
    let room = decoder.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len() * 3 + 16);
    out.reserve(room);
    let _ = decoder.decode_to_string(bytes, out, last);
}

/// The encoding console programs write in.
#[cfg(windows)]
pub fn console_encoding() -> &'static Encoding {
    static ENCODING: std::sync::OnceLock<&'static Encoding> = std::sync::OnceLock::new();
    ENCODING.get_or_init(|| console_code_page().and_then(codepage::to_encoding).unwrap_or(encoding_rs::UTF_8))
}

/// The encoding console programs write in.
#[cfg(not(windows))]
pub fn console_encoding() -> &'static Encoding {
    encoding_rs::UTF_8
}

/// The console output code page, read from `chcp` (a child inherits our
/// console, or gets a new one in the OEM code page — the same page our own
/// children write in). Spawning keeps this crate free of `unsafe` FFI, as
/// `taskkill` does for process trees; it runs once, when a pending span first
/// requires fallback (including an incomplete UTF-8 span at EOF or the limit).
#[cfg(windows)]
fn console_code_page() -> Option<u16> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let out = std::process::Command::new("cmd")
        .args(["/d", "/c", "chcp"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    parse_chcp(&out.stdout)
}

/// The number at the end of `chcp`'s localized "Active code page: 936".
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_chcp(stdout: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(stdout);
    let digits: String = text.trim_end().chars().rev().take_while(char::is_ascii_digit).collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

/// Guess output encoding via chardetng, with IBM866 coerced to Windows-1252 when the high bytes are
/// only CP1252 smart punctuation next to ASCII words (chardetng reads such snippets as Cyrillic).
pub fn guess_encoding(bytes: &[u8]) -> &'static Encoding {
    // chardetng 1.0: ISO-2022-JP and UTF-8 are opt-in; both allowed as 0.1 did (command output,
    // not script-running web content)
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    if encoding == encoding_rs::IBM866 && looks_like_windows_1252_punctuation(bytes) {
        return encoding_rs::WINDOWS_1252;
    }
    encoding
}

const WINDOWS_1252_PUNCT_BYTES: [u8; 8] = [0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x99];

fn looks_like_windows_1252_punctuation(bytes: &[u8]) -> bool {
    let mut saw_extended_punctuation = false;
    let mut saw_ascii_word = false;
    for &byte in bytes {
        if byte >= 0xA0 {
            return false;
        }
        if (0x80..=0x9F).contains(&byte) {
            if !WINDOWS_1252_PUNCT_BYTES.contains(&byte) {
                return false;
            }
            saw_extended_punctuation = true;
        }
        if byte.is_ascii_alphabetic() {
            saw_ascii_word = true;
        }
    }
    saw_extended_punctuation && saw_ascii_word
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_in_reads(decoder: &mut StreamDecoder, bytes: &[u8], read: usize) -> String {
        let mut out = String::new();
        for chunk in bytes.chunks(read) {
            out.push_str(&decoder.push(chunk));
        }
        out.push_str(&decoder.finish());
        out
    }

    #[test]
    fn utf8_character_split_across_reads_survives() {
        let text = "日本語 ünïcødé ✓\n";
        for read in 1..6 {
            assert_eq!(decode_in_reads(&mut StreamDecoder::new(), text.as_bytes(), read), text);
        }
    }

    #[test]
    fn gbk_character_split_across_reads_survives() {
        let (gbk, _, _) = encoding_rs::GBK.encode("中文输出\n");
        for read in 1..4 {
            let mut d = StreamDecoder::with_fallback(encoding_rs::GBK);
            assert_eq!(decode_in_reads(&mut d, &gbk, read), "中文输出\n");
        }
    }

    #[test]
    fn each_line_picks_its_own_encoding() {
        let mut bytes = "utf8 ✓\n".as_bytes().to_vec();
        bytes.extend_from_slice(&encoding_rs::GBK.encode("中文\n").0);
        let mut d = StreamDecoder::with_fallback(encoding_rs::GBK);
        assert_eq!(decode_in_reads(&mut d, &bytes, 3), "utf8 ✓\n中文\n");
    }

    #[test]
    fn smart_punctuation_is_not_read_as_cyrillic() {
        // “test” in Windows-1252
        let bytes = b"\x93test\x94\n";
        assert_eq!(guess_encoding(bytes), encoding_rs::WINDOWS_1252);
        // the default decoder guesses only off Windows: there a non-UTF-8 span is decoded in the
        // console code page (whatever the host's is: 65001 on CI runners), never guessed
        if !cfg!(windows) {
            assert_eq!(decode_in_reads(&mut StreamDecoder::new(), bytes, 2), "\u{201c}test\u{201d}\n");
        }
    }

    #[test]
    fn chcp_output_yields_the_code_page() {
        assert_eq!(parse_chcp(b"Active code page: 936\r\n"), Some(936));
    }
}
