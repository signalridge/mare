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
    Ok(())
}
