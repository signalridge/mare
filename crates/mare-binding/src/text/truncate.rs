//! Line/byte truncation, UTF-8-safe cutting, and bounded preview retention for `mare.text`.

use mlua::{Lua, Table};

use mare_shared::lua::rt_err;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Head,
    Tail,
    /// keep the beginning and the end, mark the omitted middle
    Middle,
}

pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    pub total_lines: usize,
    pub shown_lines: usize,
    pub omitted_lines: usize,
    /// lines kept from the start / the end (for "lines 1-H and T-N of N" notices)
    pub head_lines: usize,
    pub tail_lines: usize,
}

/// If a byte cut between `prev` (up to 3 trailing bytes before the cut) and `next` (leading bytes
/// after the cut) splits a single valid multi-byte UTF-8 character, returns `(prefix_len, cont_len)`
/// where `prefix_len` bytes are at the end of `prev` and `cont_len` continuation bytes are at the
/// start of `next`. Returns `None` for complete characters or genuinely invalid UTF-8 bytes.
pub(crate) fn split_utf8_boundary(prev: &[u8], next: &[u8]) -> Option<(usize, usize)> {
    let cont_max = next.iter().take(3).take_while(|&&b| (0x80..0xC0).contains(&b)).count();
    if cont_max == 0 || prev.is_empty() {
        return None;
    }
    let prev_max = prev.len().min(3);
    for pre in 1..=prev_max {
        let lead = prev[prev.len() - pre];
        if !(0xC2..=0xF4).contains(&lead) {
            continue;
        }
        for cont in 1..=cont_max {
            let total = pre + cont;
            if total > 4 {
                break;
            }
            let mut buf = [0u8; 4];
            buf[..pre].copy_from_slice(&prev[prev.len() - pre..]);
            buf[pre..total].copy_from_slice(&next[..cont]);
            if std::str::from_utf8(&buf[..total]).ok().is_some_and(|s| s.chars().count() == 1) {
                return Some((pre, cont));
            }
        }
    }
    None
}

/// Length of an incomplete valid UTF-8 prefix at the end of `bytes` (0 for complete characters or
/// genuinely invalid raw bytes).
pub(crate) fn trailing_incomplete_utf8_len(bytes: &[u8]) -> usize {
    let max_back = bytes.len().min(3);
    for len in 1..=max_back {
        let slice = &bytes[bytes.len() - len..];
        if let Err(e) = std::str::from_utf8(slice)
            && e.valid_up_to() == 0
            && e.error_len().is_none()
        {
            return len;
        }
    }
    0
}

/// Align a 0-based start byte cut `pos` (keeping `bytes[start..]`) forward past any continuation
/// bytes of a split valid UTF-8 character. Preserves genuinely invalid raw bytes untouched.
pub(crate) fn align_start_cut(bytes: &[u8], pos: usize) -> usize {
    let pos = pos.min(bytes.len());
    if pos == 0 || pos == bytes.len() {
        return pos;
    }
    if let Some((_, cont)) = split_utf8_boundary(&bytes[..pos], &bytes[pos..]) { pos + cont } else { pos }
}

/// Align a 0-based end byte cut `stop` (keeping `bytes[..end]`) backward before any lead byte of a
/// split valid UTF-8 character (may return `0` when `stop` is smaller than the first codepoint).
/// Preserves genuinely invalid raw bytes untouched.
pub(crate) fn align_end_cut(bytes: &[u8], stop: usize) -> usize {
    let stop = stop.min(bytes.len());
    if stop == 0 || stop == bytes.len() {
        return stop;
    }
    if let Some((pre, _)) = split_utf8_boundary(&bytes[..stop], &bytes[stop..]) { stop - pre } else { stop }
}

/// The first (`tail = false`) or last `n` units of `s`, never splitting a UTF-8 sequence — the
/// one boundary helper behind `mare.text.cut`. `chars = false`: `n` bytes, the cut moved inward
/// off continuation bytes (so the result may be shorter than `n`). `chars = true`: `n` code
/// points, or `n` raw bytes when `s` isn't valid UTF-8.
pub fn cut(s: &[u8], n: usize, tail: bool, chars: bool) -> &[u8] {
    let len = s.len();
    let cont = |b: u8| b & 0xC0 == 0x80;
    if chars {
        let Ok(st) = std::str::from_utf8(s) else {
            let n = n.min(len);
            return if tail { &s[len - n..] } else { &s[..n] };
        };
        if !tail {
            return st.char_indices().nth(n).map_or(s, |(i, _)| &s[..i]);
        }
        let count = st.chars().count();
        if n >= count {
            return s;
        }
        return st.char_indices().nth(count - n).map_or(&s[len..], |(i, _)| &s[i..]);
    }
    if len <= n {
        return s;
    }
    if tail {
        let mut i = len - n;
        while i < len && cont(s[i]) {
            i += 1;
        }
        &s[i..]
    } else {
        let mut i = n;
        while i > 0 && cont(s[i]) {
            i -= 1;
        }
        &s[..i]
    }
}

/// Cap a single line at `max` chars, returning `(line, was_capped)`.
fn cap_line(l: &str, max: usize) -> (std::borrow::Cow<'_, str>, bool) {
    if max == 0 || l.chars().count() <= max {
        return (std::borrow::Cow::Borrowed(l), false);
    }
    let head: String = l.chars().take(max).collect();
    (std::borrow::Cow::Owned(format!("{head}… [line truncated]")), true)
}

/// Truncate by lines and bytes (whichever limit hits first). Never returns partial lines.
pub fn truncate(s: &str, max_lines: usize, max_bytes: usize, keep: Keep, max_line_chars: usize) -> Truncated {
    // a single line longer than half the byte budget is cut by itself, so head/tail never
    // come back empty (e.g. minified files)
    let line_cap = match max_line_chars {
        0 => max_bytes / 2,
        n => n.min(max_bytes / 2),
    };
    let mut any_line_capped = false;
    let mut lines: Vec<std::borrow::Cow<str>> = s
        .split('\n')
        .map(|l| {
            let (line, capped) = cap_line(l, line_cap);
            any_line_capped |= capped;
            line
        })
        .collect();
    if s.ends_with('\n') {
        lines.pop();
    }
    let total = lines.len();
    let fits = |n: usize, b: usize| n <= max_lines && b <= max_bytes;
    let all_bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
    if fits(total, all_bytes) {
        let text = lines.join("\n");
        return Truncated {
            text,
            truncated: any_line_capped,
            total_lines: total,
            shown_lines: total,
            omitted_lines: 0,
            head_lines: total,
            tail_lines: 0,
        };
    }
    let take = |iter: &mut dyn Iterator<Item = usize>, budget_lines: usize, budget_bytes: usize| -> Vec<usize> {
        let mut out = vec![];
        let mut bytes = 0;
        for i in iter {
            let lb = lines[i].len() + 1;
            if out.len() >= budget_lines || bytes + lb > budget_bytes {
                break;
            }
            bytes += lb;
            out.push(i);
        }
        out
    };
    let (head, tail): (Vec<usize>, Vec<usize>) = match keep {
        Keep::Head => (take(&mut (0..total), max_lines, max_bytes), vec![]),
        Keep::Tail => (vec![], take(&mut (0..total).rev(), max_lines, max_bytes)),
        Keep::Middle => {
            let h = take(&mut (0..total), max_lines / 2, max_bytes / 2);
            let start = h.last().map(|x| x + 1).unwrap_or(0);
            let t = take(&mut (start..total).rev(), max_lines - h.len(), max_bytes / 2);
            (h, t)
        }
    };
    let mut tail = tail;
    tail.reverse();
    let shown = head.len() + tail.len();
    let omitted = total - shown;
    let mut parts: Vec<String> = head.iter().map(|&i| lines[i].to_string()).collect();
    let marker = format!("[… {omitted} lines omitted …]");
    match keep {
        Keep::Head => parts.push(marker),
        Keep::Tail => parts.insert(0, marker),
        Keep::Middle => parts.push(marker),
    }
    parts.extend(tail.iter().map(|&i| lines[i].to_string()));
    Truncated {
        text: parts.join("\n"),
        truncated: true,
        total_lines: total,
        shown_lines: shown,
        omitted_lines: omitted,
        head_lines: head.len(),
        tail_lines: tail.len(),
    }
}

/// Bounded UTF-8 head/tail preview. The suffix is included in the total budget.
pub fn retain(s: &str, bytes: usize, lines: usize, suffix: &str) -> Result<(String, bool), String> {
    let reserved = suffix.len() + usize::from(!suffix.is_empty());
    let suffix_lines = if suffix.is_empty() { 0 } else { suffix.lines().count() };
    if reserved > bytes || suffix_lines > lines {
        return Err("output budget cannot fit recovery notice".into());
    }
    let cap = bytes - reserved;
    let line_cap = lines - suffix_lines;
    let mut text = s.to_owned();
    let mut changed = false;
    if s.len() > cap || s.lines().count() > line_cap {
        let preview = truncate(s, line_cap.saturating_sub(1), cap.saturating_sub(64), Keep::Middle, 0);
        text = preview.text;
        // Existing line markers are descriptive; enforce the complete wire budget as well.
        text = String::from_utf8_lossy(cut(text.as_bytes(), cap, false, false)).into_owned();
        text = text.lines().take(line_cap).collect::<Vec<_>>().join("\n");
        changed = true;
    }
    if !suffix.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(suffix);
    }
    Ok((text, changed))
}

pub(super) fn register_truncate_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // truncate(s, {max_lines=2000, max_bytes=51200, keep="head"|"tail"|"middle", max_line_chars=0})
    //   -> text, {truncated, total_lines, shown_lines, omitted_lines}
    t.set(
        "truncate",
        lua.create_function(|lua, (s, o): (String, Option<Table>)| {
            let g = |k: &str, d: usize| o.as_ref().and_then(|o| o.get::<Option<usize>>(k).ok().flatten()).unwrap_or(d);
            let keep = match o.as_ref().and_then(|o| o.get::<Option<String>>("keep").ok().flatten()).as_deref() {
                Some("tail") => Keep::Tail,
                Some("middle") => Keep::Middle,
                _ => Keep::Head,
            };
            let r = truncate(&s, g("max_lines", 2000), g("max_bytes", 50 * 1024), keep, g("max_line_chars", 0));
            let info = lua.create_table()?;
            info.set("truncated", r.truncated)?;
            info.set("total_lines", r.total_lines)?;
            info.set("shown_lines", r.shown_lines)?;
            info.set("omitted_lines", r.omitted_lines)?;
            info.set("head_lines", r.head_lines)?;
            info.set("tail_lines", r.tail_lines)?;
            Ok((r.text, info))
        })?,
    )?;
    // A generic bounded preview with a reserved, intact recovery notice.
    t.set(
        "retain",
        lua.create_function(|_, (s, o): (String, Table)| {
            let bytes = o.get::<Option<usize>>("max_bytes")?.unwrap_or(51200);
            let lines = o.get::<Option<usize>>("max_lines")?.unwrap_or(2000);
            let suffix = o.get::<Option<String>>("suffix")?.unwrap_or_default();
            retain(&s, bytes, lines, &suffix).map_err(rt_err)
        })?,
    )?;
    // cut(s, n, {keep = "head"|"tail", unit = "bytes"|"chars"}) -> string: the first / last `n`
    //   bytes (never splitting a UTF-8 sequence) or code points (raw bytes when `s` isn't UTF-8)
    t.set(
        "cut",
        lua.create_function(|lua, (s, n, o): (mlua::LuaString, i64, Option<Table>)| {
            let opt = |k: &str| o.as_ref().and_then(|o| o.get::<Option<String>>(k).ok().flatten());
            let tail = opt("keep").as_deref() == Some("tail");
            let chars = opt("unit").as_deref() == Some("chars");
            let b = s.as_bytes();
            lua.create_string(cut(&b, n.max(0) as usize, tail, chars))
        })?,
    )?;
    // Shared UTF-8 byte-boundary helpers (same rules as Rust `job::Ring`): never split a valid
    // multi-byte UTF-8 sequence while leaving genuinely invalid raw bytes untouched.
    // Indices follow Lua 1-based byte conventions (`align_start` returns 1..=#s+1; `align_end`
    // returns 0..=#s so `string.sub(s, 1, stop)` returns `""` when budget < first codepoint).
    t.set(
        "utf8_align_start",
        lua.create_function(|_, (s, start): (mlua::LuaString, i64)| {
            let b = s.as_bytes();
            let pos = if start <= 1 { 0 } else { ((start - 1) as usize).min(b.len()) };
            Ok(align_start_cut(&b, pos) + 1)
        })?,
    )?;
    t.set(
        "utf8_align_end",
        lua.create_function(|_, (s, stop): (mlua::LuaString, i64)| {
            let b = s.as_bytes();
            let pos = if stop <= 0 { 0 } else { (stop as usize).min(b.len()) };
            Ok(align_end_cut(&b, pos))
        })?,
    )?;
    t.set(
        "utf8_trim_tail",
        lua.create_function(|lua, s: mlua::LuaString| {
            let b = s.as_bytes();
            let trim = trailing_incomplete_utf8_len(&b);
            lua.create_string(&b[..b.len() - trim])
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_utf8_boundary_helpers_handle_small_budgets_and_invalid_bytes() {
        // Budget smaller than a single 3-byte codepoint ("中" = 3 bytes).
        assert_eq!(align_end_cut("中a".as_bytes(), 2), 0);
        assert_eq!(align_start_cut("中".as_bytes(), 1), 3);
        assert_eq!(align_start_cut("中a".as_bytes(), 1), 3);
        assert_eq!(align_end_cut("a中b".as_bytes(), 3), 1);
        assert_eq!(trailing_incomplete_utf8_len(&"中".as_bytes()[..2]), 2);
        assert_eq!(trailing_incomplete_utf8_len("中".as_bytes()), 0);
        // Genuinely invalid raw bytes are preserved untouched.
        assert_eq!(align_start_cut(b"\xff\x80abcd", 1), 1);
        assert_eq!(align_end_cut(b"\xff\x80abcd", 1), 1);
        assert_eq!(trailing_incomplete_utf8_len(b"abc\xff"), 0);
    }

    #[test]
    fn lua_bash_output_and_persistent_shell_share_utf8_boundary_semantics() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        let text = lua.create_table().unwrap();
        register_truncate_ops(&lua, &text).unwrap();
        text.set("trunc_width", lua.create_function(|_, (s, _): (String, usize)| Ok(s)).unwrap()).unwrap();
        mare.set("text", text).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let bash_output_src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../mare-plugin/preset/plugins/lua/tools/bash_output.lua"),
        )
        .unwrap();
        let (out, dropped, tail, tail_small, end_small, invalid_start, split_start, split_end): (
            String,
            i64,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
        ) = lua
            .load(format!(
                r###"
                mare.paths = {{ tmp = "/tmp", state = "/tmp" }}
                mare.date = function() return "20260101-000000" end
                mare.crypto = {{ random_hex = function() return "abc" end }}
                mare.proc = {{ spill_write = function() end }}
                mare.fs = {{ list = function() return {{}} end }}
                local mod = (function() {bash_output_src} end)()({{
                    ctx = {{ on = function() end, spawn = function() end }},
                    cfg = {{ max_lines = 100, max_bytes = 1024 }},
                }})
                local acc = mod.new_output_acc(20)
                acc:push(string.rep("中", 12))
                local r = acc:result()
                assert(utf8.len(r.out) ~= nil, "acc:result produced invalid UTF-8")
                local t = acc:tail_of(10)
                assert(utf8.len(t) ~= nil, "acc:tail_of produced invalid UTF-8")
                local small = mod.new_output_acc(20)
                small:push("中")
                local ts = small:tail_of(2)
                assert(utf8.len(ts) ~= nil, "tail_of(2) on 中 produced invalid UTF-8")
                return r.out,
                    r.dropped,
                    t,
                    ts,
                    mod.utf8_align_end("中a", 2),
                    mod.utf8_align_start("\255\128abcd", 2),
                    mod.utf8_align_start("中a", 2),
                    mod.utf8_align_end("a中b", 3)
                "###
            ))
            .eval()
            .unwrap();
        assert!(out.starts_with("中中中"), "head must keep whole chars: {out:?}");
        assert!(out.ends_with("中中中"), "tail must keep whole chars: {out:?}");
        assert_eq!(dropped, 18, "dropped byte count must match trimmed head/tail");
        assert_eq!(tail, "中中中");
        assert_eq!(tail_small, "", "tail_of(2) on 中 must not start on continuation byte");
        assert_eq!(end_small, 0, "utf8_align_end on 中a with budget 2 must return 0 (no split UTF-8)");
        assert_eq!(invalid_start, 2, "genuinely invalid 0x80 after 0xFF must not be swallowed");
        assert_eq!(split_start, 4, "valid split UTF-8 start must advance past continuation bytes");
        assert_eq!(split_end, 1, "valid split UTF-8 end must back up before lead byte");
    }
}
