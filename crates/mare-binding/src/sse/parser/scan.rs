use std::str::Utf8Error;

// Ported from sse-stream 0.3.0 `src/stream/parser/scan.rs` (MIT OR Apache-2.0).
// Data values benefit from direct memchr2 on long slices; short comment
// lines need a scalar head.

/// # Errors
/// Returns the standard UTF-8 error, including its original byte position.
///
/// mare: std only (upstream optionally uses simdutf8 for values of 256+ bytes).
#[inline]
pub(super) fn validate_utf8(value: &[u8]) -> Result<&str, Utf8Error> {
    std::str::from_utf8(value)
}

#[inline]
pub(super) fn find_data_line_end(bytes: &[u8]) -> Option<usize> {
    crate::sse::note_scanned(bytes.len());
    const SCALAR_MAX_LEN: usize = 16;
    if bytes.len() > SCALAR_MAX_LEN {
        return memchr::memchr2(b'\n', b'\r', bytes);
    }
    bytes.iter().position(|byte| matches!(*byte, b'\n' | b'\r'))
}

/// Find the index of the first `\n` or `\r` in `bytes`.
///
/// The first few bytes are scanned scalar, the rest with
/// [`memchr2`](memchr::memchr2): SIMD dispatch does not pay off on short lines.
#[inline]
pub(super) fn find_line_end(bytes: &[u8]) -> Option<usize> {
    crate::sse::note_scanned(bytes.len());
    /// How many leading bytes are scanned scalar before delegating to `memchr2`.
    const SCALAR_HEAD: usize = 16;

    let head_len = bytes.len().min(SCALAR_HEAD);
    let head_hit = bytes[..head_len].iter().position(|byte| matches!(*byte, b'\n' | b'\r'));
    if head_len == bytes.len() || head_hit.is_some() {
        return head_hit;
    }
    memchr::memchr2(b'\n', b'\r', &bytes[head_len..]).map(|i| i + head_len)
}
