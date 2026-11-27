//! Release-only deflate-compressed archive of preset source files extracted by `runtime::ensure()`.
//!
//! Normal startup loads precompiled Lua bytecode directly and never decompresses this archive
//! (unless `~/.local/share/mare/runtime/VERSION` is missing or out of date).

use std::collections::HashMap;
use std::io::Read as _;

static ARCHIVE_DEFLATE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime_sources.deflate"));

fn decompress_raw() -> Vec<u8> {
    let mut decoder = flate2::read::DeflateDecoder::new(ARCHIVE_DEFLATE);
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf).expect("decompressing embedded runtime preset archive");
    buf
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> usize {
    let end = *pos + 4;
    let n = u32::from_le_bytes(bytes[*pos..end].try_into().expect("truncated runtime archive header"));
    *pos = end;
    n as usize
}

fn read_str<'a>(bytes: &'a [u8], pos: &mut usize) -> &'a str {
    let len = read_u32(bytes, pos);
    let end = *pos + len;
    let s = std::str::from_utf8(&bytes[*pos..end]).expect("invalid utf-8 in runtime archive");
    *pos = end;
    s
}

/// Decompress every preset source file into a map keyed by path relative to `preset/`
/// (e.g. `"plugins/agent.lua"`, `"plugins/lua/agent/loop.lua"`, `"types/mare.lua"`, `"NEWS.md"`).
/// Callers should drop the returned map promptly so the decompressed text does not stay resident.
pub(crate) fn extract_all() -> HashMap<String, String> {
    let raw = decompress_raw();
    let mut pos = 0;
    let mut out = HashMap::new();
    while pos < raw.len() {
        let key = read_str(&raw, &mut pos).to_owned();
        let val = read_str(&raw, &mut pos).to_owned();
        out.insert(key, val);
    }
    out
}

/// Look up a single preset source file by path relative to `preset/`.
pub(crate) fn get(target_key: &str) -> Option<String> {
    let raw = decompress_raw();
    let mut pos = 0;
    while pos < raw.len() {
        let key = read_str(&raw, &mut pos);
        let val_len = read_u32(&raw, &mut pos);
        let val_end = pos + val_len;
        if key == target_key {
            return Some(std::str::from_utf8(&raw[pos..val_end]).expect("invalid utf-8 in runtime archive").to_owned());
        }
        pos = val_end;
    }
    None
}

/// List all entries whose key starts with `prefix`, returning `(key_without_prefix, contents)`.
pub(crate) fn list_prefix(prefix: &str) -> Vec<(String, String)> {
    let raw = decompress_raw();
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < raw.len() {
        let key = read_str(&raw, &mut pos);
        let val_len = read_u32(&raw, &mut pos);
        let val_end = pos + val_len;
        if let Some(rel) = key.strip_prefix(prefix) {
            let val = std::str::from_utf8(&raw[pos..val_end]).expect("invalid utf-8 in runtime archive").to_owned();
            out.push((rel.to_owned(), val));
        }
        pos = val_end;
    }
    out
}
