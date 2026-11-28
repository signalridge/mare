//! `mare.text`: regex, truncation, token estimates, width, fuzzy match, and the heavy edit
//! engines (replace / apply_patch) plus diffs.

mod edit;
pub(crate) mod truncate;
mod web;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use mare_shared::lua::nil_err;

pub use truncate::{Keep, Truncated, cut, retain, truncate};

pub struct Regex(regex::Regex);

impl UserData for Regex {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("is_match", |_, this, s: String| Ok(this.0.is_match(&s)));
        // find(s, init?) -> start, end (1-based inclusive) | nil
        m.add_method("find", |_, this, (s, init): (String, Option<usize>)| {
            let start = init.unwrap_or(1).saturating_sub(1).min(s.len());
            Ok(match this.0.find_at(&s, start) {
                Some(m) => (Some(m.start() + 1), Some(m.end())),
                None => (None, None),
            })
        });
        // captures(s) -> {full, groups..., named=...} | nil
        m.add_method("captures", |lua, this, s: String| {
            let Some(c) = this.0.captures(&s) else { return Ok(None) };
            let t = lua.create_table()?;
            for (i, g) in c.iter().enumerate() {
                if let Some(g) = g {
                    t.set(i, g.as_str())?;
                }
            }
            for n in this.0.capture_names().flatten() {
                if let Some(g) = c.name(n) {
                    t.set(n, g.as_str())?;
                }
            }
            Ok(Some(t))
        });
        m.add_method("replace", |_, this, (s, rep, all): (String, String, Option<bool>)| {
            Ok(if all.unwrap_or(true) {
                this.0.replace_all(&s, rep.as_str()).into_owned()
            } else {
                this.0.replace(&s, rep.as_str()).into_owned()
            })
        });
        m.add_method("split", |_, this, s: String| Ok(this.0.split(&s).map(str::to_string).collect::<Vec<_>>()));
    }
}

fn fuzzy(query: &str, items: Vec<String>, limit: usize) -> Vec<String> {
    use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher};
    if query.is_empty() {
        return items.into_iter().take(limit).collect();
    }
    let mut m = Matcher::new(Config::DEFAULT.match_paths());
    let pat = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut res = pat.match_list(items, &mut m);
    res.truncate(limit);
    res.into_iter().map(|(s, _)| s).collect()
}

/// Shell metacharacters outside quotes that make a command more than one simple command
/// (`$(` and backticks also count inside double quotes, where the shell still expands them).
pub fn is_compound(cmd: &str) -> bool {
    let (mut single, mut double, mut esc) = (false, false, false);
    let b: Vec<char> = cmd.chars().collect();
    for (i, &c) in b.iter().enumerate() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if !single => esc = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return true,
            '$' if !single && b.get(i + 1) == Some(&'(') => return true,
            ';' | '&' | '|' | '<' | '>' | '\n' if !single && !double => return true,
            _ => {}
        }
    }
    false
}

/// Flat byte estimate for an inline image regardless of the size of its base64 payload.
pub const IMAGE_ESTIMATE_BYTES: usize = 4800;

/// Wire-declared hints for token estimation: provider-specific image shapes and opaque fields
/// capped at a flat byte budget. Core Rust only recognizes mare's canonical `{type="image", mime="image/...", data=...}`
/// shape and standard RFC 2397 `data:image/...;base64,...` data URIs; wire plugins declare their
/// own shapes via `mare.text.register_estimate_hint` (or `estimate_hints` in `llm:register_wire`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenEstimateHints {
    /// Additional object `type` values whose `data` string field holds an inline image payload.
    pub image_types: BTreeSet<String>,
    /// Additional object MIME key names (with `"image/..."` value) whose sibling `data` holds an inline image payload.
    pub image_mime_keys: BTreeSet<String>,
    /// Field names whose string values are capped at a flat byte budget (`key -> cap_bytes`).
    pub capped_fields: BTreeMap<String, usize>,
}

#[derive(Default)]
struct HintRegistry {
    next_gen: u64,
    entries: BTreeMap<String, (u64, TokenEstimateHints)>,
    merged: TokenEstimateHints,
}

impl HintRegistry {
    fn rebuild(&mut self) {
        let mut merged = TokenEstimateHints::default();
        for (_, hint) in self.entries.values() {
            merged.image_types.extend(hint.image_types.iter().cloned());
            merged.image_mime_keys.extend(hint.image_mime_keys.iter().cloned());
            for (k, &cap) in &hint.capped_fields {
                merged
                    .capped_fields
                    .entry(k.clone())
                    .and_modify(|existing| *existing = (*existing).min(cap))
                    .or_insert(cap);
            }
        }
        self.merged = merged;
    }

    fn insert(&mut self, name: String, hint: TokenEstimateHints) -> u64 {
        self.next_gen += 1;
        let generation = self.next_gen;
        self.entries.insert(name, (generation, hint));
        self.rebuild();
        generation
    }

    fn remove(&mut self, name: &str, generation: u64) {
        if self.entries.get(name).is_some_and(|(g, _)| *g == generation) {
            self.entries.remove(name);
            self.rebuild();
        }
    }
}

/// Approximate token count: 4 bytes per token, rounded up.
pub fn approx_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

fn is_image_mime(v: Option<&serde_json::Value>) -> bool {
    v.and_then(|v| v.as_str()).is_some_and(|m| m.as_bytes().get(..6).is_some_and(|b| b.eq_ignore_ascii_case(b"image/")))
}

/// Base64 bytes of inline images in `v` and how many there are: RFC 2397 `data:image/…;base64,`
/// URLs, mare's canonical `{type = "image", mime = "image/…", data = …}` shape, and any
/// additional shapes declared by registered wire plugins in `hints`.
fn image_payloads(v: &serde_json::Value, hints: &TokenEstimateHints) -> (usize, usize) {
    use serde_json::Value as J;
    match v {
        J::String(s) => {
            let lower = s.get(..11).map(|p| p.eq_ignore_ascii_case("data:image/")).unwrap_or(false);
            match s.find(";base64,") {
                Some(i) if lower => (s.len() - i - 8, 1),
                _ => (0, 0),
            }
        }
        J::Array(a) => a.iter().map(|item| image_payloads(item, hints)).fold((0, 0), |x, y| (x.0 + y.0, x.1 + y.1)),
        J::Object(o) => {
            let image = o
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|ty| ty == "image" || hints.image_types.contains(ty))
                || is_image_mime(o.get("mime"))
                || hints.image_mime_keys.iter().any(|k| is_image_mime(o.get(k)));
            let mut acc = (0, 0);
            for (k, val) in o {
                let p = match val {
                    J::String(s) if image && k == "data" => (s.len(), 1),
                    other => image_payloads(other, hints),
                };
                acc = (acc.0 + p.0, acc.1 + p.1);
            }
            acc
        }
        _ => (0, 0),
    }
}

fn capped_field_excess_bytes(v: &serde_json::Value, caps: &BTreeMap<String, usize>) -> usize {
    if caps.is_empty() {
        return 0;
    }
    use serde_json::Value as J;
    match v {
        J::Array(a) => a.iter().map(|item| capped_field_excess_bytes(item, caps)).sum(),
        J::Object(o) => o
            .iter()
            .map(|(k, val)| match val {
                J::String(s) => match caps.get(k) {
                    Some(&cap) => s.len().saturating_sub(cap),
                    None => 0,
                },
                other => capped_field_excess_bytes(other, caps),
            })
            .sum(),
        _ => 0,
    }
}

/// Model-visible size of a JSON value in bytes using `hints`: its serialization, with inline
/// images replaced by [`IMAGE_ESTIMATE_BYTES`] and any wire-declared capped fields bounded.
pub fn estimate_bytes_with_hints(v: &serde_json::Value, hints: &TokenEstimateHints) -> usize {
    let raw = serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    let (payload, images) = image_payloads(v, hints);
    let capped_excess = capped_field_excess_bytes(v, &hints.capped_fields);
    raw.saturating_sub(payload + capped_excess) + images * IMAGE_ESTIMATE_BYTES
}

/// Model-visible size of a JSON value in bytes using core mare defaults (no wire hints).
pub fn estimate_bytes(v: &serde_json::Value) -> usize {
    estimate_bytes_with_hints(v, &TokenEstimateHints::default())
}

fn register_shell_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // shell_split(cmd) -> argv | nil, compound: POSIX-shell word splitting (nil when the quoting
    // is malformed) plus whether the command uses compound syntax outside quotes (`;` `&` `|`
    // `<` `>` newline, `$(…)` or backticks) — callers matching command rules must not treat a
    // compound command as its first word
    t.set("shell_split", lua.create_function(|_, cmd: String| Ok((shlex::split(&cmd), is_compound(&cmd))))?)?;
    // redact(value) -> value: the unified redactor — strings
    // lose credential-looking parts, tables lose the values of sensitive keys (recursively)
    t.set(
        "redact",
        lua.create_function(|lua, v: mlua::Value| match v {
            mlua::Value::String(s) => {
                Ok(mlua::Value::String(lua.create_string(mare_shared::redact::redact_str(&s.to_string_lossy()))?))
            }
            mlua::Value::Table(_) => {
                let j = mare_shared::lua::lua_to_json(lua, v)?;
                mare_shared::lua::json_to_lua(lua, &mare_shared::redact::redact_json(&j))
            }
            other => Ok(other),
        })?,
    )?;
    // dangerous(cmd, {shell = "bash"|"powershell"}) -> rule name ("forced_rm" |
    // "windows_force_delete") | nil, literal commands {argv…} — built-in dangerous-command rules
    // the list of commands lets plugins add their own
    t.set(
        "dangerous",
        lua.create_function(|lua, (cmd, opts): (mlua::Value, Option<mlua::Table>)| {
            use mlua::FromLua;
            let ps = opts
                .as_ref()
                .and_then(|o| o.get::<Option<String>>("shell").ok().flatten())
                .is_some_and(|s| s == "powershell");
            let windows = ps || cfg!(windows);
            let (m, cmds) = match cmd {
                mlua::Value::String(s) => {
                    let s = s.to_str()?;
                    let m = crate::danger::dangerous(&s, ps, windows);
                    let cmds = if ps { vec![] } else { crate::danger::literal_commands(&s) };
                    (m, cmds)
                }
                mlua::Value::Table(tbl) => {
                    let argv = Vec::<String>::from_lua(mlua::Value::Table(tbl), lua)?;
                    let m = crate::danger::dangerous_argv(&argv, windows);
                    (m, if argv.is_empty() { vec![] } else { vec![argv] })
                }
                other => {
                    return Err(mlua::Error::FromLuaConversionError {
                        from: other.type_name(),
                        to: "string or string[]".to_string(),
                        message: Some("dangerous expects a command string or argv string list".to_string()),
                    });
                }
            };
            let list = lua.create_table()?;
            for c in cmds {
                list.push(c)?;
            }
            Ok((m.map(|m| m.name()), list))
        })?,
    )?;
    // shell_parse(cmd, {shell = "bash"|"powershell", budget_ms?, max_nodes?, safe_sinks?}) ->
    //   {analyzable, reason?, segments = {{argv, op?}}, script?}: plain-word segments joined by
    //   && ||; |; anything dynamic is not analyzable (see shellparse.rs)
    t.set(
        "shell_parse",
        lua.create_function(|lua, (cmd, opts): (Value, Option<mlua::Table>)| {
            use crate::shellparse::{
                Budget, Shell, literal_argv_candidates, literal_candidates, parse_argv, parse_with,
            };
            use mlua::FromLua;
            let get = |k: &str| opts.as_ref().and_then(|o| o.get::<Option<f64>>(k).ok().flatten());
            let shell = match opts.as_ref().and_then(|o| o.get::<Option<String>>("shell").ok().flatten()).as_deref() {
                Some("powershell" | "pwsh") => Shell::PowerShell,
                _ => Shell::Bash,
            };
            let mut budget = Budget::default();
            if let Some(ms) = get("budget_ms") {
                budget.time = std::time::Duration::from_millis(ms.max(1.0) as u64);
            }
            if let Some(n) = get("max_nodes") {
                budget.max_nodes = n.max(1.0) as usize;
            }
            // extra fixed-sink redirect targets
            let sinks: Vec<String> = opts
                .as_ref()
                .and_then(|o| o.get::<Option<Vec<String>>>("safe_sinks").ok().flatten())
                .unwrap_or_default();
            let (p, mut literals) = match cmd {
                Value::String(s) => {
                    let src = s.to_str()?;
                    (parse_with(&src, shell, budget, &sinks), literal_candidates(&src, shell, budget))
                }
                Value::Table(tbl) => {
                    let argv = Vec::<String>::from_lua(Value::Table(tbl), lua)?;
                    let literals = literal_argv_candidates(&argv);
                    (parse_argv(argv, shell, budget, &sinks), literals)
                }
                other => {
                    return Err(mlua::Error::FromLuaConversionError {
                        from: other.type_name(),
                        to: "string or string[]".to_string(),
                        message: Some("shell_parse expects a command string or argv string list".to_string()),
                    });
                }
            };
            if let Some(script) = p.script.as_deref() {
                for candidate in literal_candidates(script, shell, budget) {
                    if !literals.iter().any(|existing| existing == &candidate) {
                        literals.push(candidate);
                    }
                }
            }
            literals
                .retain(|s| !matches!(s.as_str(), "/dev/null" | "/dev/stdout" | "/dev/stderr" | "NUL" | "&1" | "&2"));
            let out = lua.create_table()?;
            out.set("analyzable", p.analyzable)?;
            out.set("reason", p.reason)?;
            out.set("script", p.script)?;
            out.set("literal_paths", literals)?;
            let segs = lua.create_table()?;
            for s in p.segments {
                let seg = lua.create_table()?;
                seg.set("unwrapped", crate::danger::unwrap_argv(&s.argv))?;
                seg.set("argv", s.argv)?;
                seg.set("op", s.op)?;
                segs.push(seg)?;
            }
            out.set("segments", segs)?;
            Ok(out)
        })?,
    )?;
    Ok(())
}

fn valid_hint_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let mut parts = name.split(':');
    let Some(first) = parts.next() else { return false };
    let mut count = 1usize;
    let valid_seg = |seg: &str| {
        let bytes = seg.as_bytes();
        !bytes.is_empty()
            && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
            && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(*b, b'.' | b'_' | b'-'))
    };
    if !valid_seg(first) {
        return false;
    }
    for seg in parts {
        count += 1;
        if !valid_seg(seg) {
            return false;
        }
    }
    count >= 2
}

fn parse_estimate_hint(spec: &Table) -> mlua::Result<TokenEstimateHints> {
    let mut hint = TokenEstimateHints::default();
    for pair in spec.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        let Value::String(ks) = k else {
            return Err(mlua::Error::runtime(format!(
                "register_estimate_hint: spec keys must be strings, got {}",
                k.type_name()
            )));
        };
        let key = ks.to_str()?;
        if !matches!(key.as_ref(), "image_types" | "image_mime_keys" | "capped_fields") {
            return Err(mlua::Error::runtime(format!(
                "register_estimate_hint: unknown field `{key}` (expected image_types, image_mime_keys, capped_fields)"
            )));
        }
    }
    for (field, dest) in [("image_types", &mut hint.image_types), ("image_mime_keys", &mut hint.image_mime_keys)] {
        match spec.get::<Value>(field)? {
            Value::Nil => {}
            Value::Table(tbl) => {
                let len = tbl.raw_len();
                let mut count = 0usize;
                for pair in tbl.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    let Value::Integer(idx) = k else {
                        return Err(mlua::Error::runtime(format!(
                            "register_estimate_hint: `{field}` must be a dense string list"
                        )));
                    };
                    if idx < 1 || (idx as usize) > len {
                        return Err(mlua::Error::runtime(format!(
                            "register_estimate_hint: `{field}` must be a dense string list"
                        )));
                    }
                    let Value::String(s) = v else {
                        return Err(mlua::Error::runtime(format!(
                            "register_estimate_hint: `{field}[{idx}]` must be a non-empty string, got {}",
                            v.type_name()
                        )));
                    };
                    let text = s.to_str()?.to_owned();
                    if text.is_empty() {
                        return Err(mlua::Error::runtime(format!(
                            "register_estimate_hint: `{field}[{idx}]` must be a non-empty string"
                        )));
                    }
                    count += 1;
                    dest.insert(text);
                }
                if count != len {
                    return Err(mlua::Error::runtime(format!(
                        "register_estimate_hint: `{field}` must be a dense string list"
                    )));
                }
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "register_estimate_hint: `{field}` must be a string list, got {}",
                    other.type_name()
                )));
            }
        }
    }
    match spec.get::<Value>("capped_fields")? {
        Value::Nil => {}
        Value::Table(tbl) => {
            for pair in tbl.pairs::<Value, Value>() {
                let (k, v) = pair?;
                let Value::String(ks) = k else {
                    return Err(mlua::Error::runtime(format!(
                        "register_estimate_hint: `capped_fields` keys must be non-empty strings, got {}",
                        k.type_name()
                    )));
                };
                let key = ks.to_str()?.to_owned();
                if key.is_empty() {
                    return Err(mlua::Error::runtime(
                        "register_estimate_hint: `capped_fields` keys must be non-empty strings",
                    ));
                }
                let Value::Integer(cap) = v else {
                    return Err(mlua::Error::runtime(format!(
                        "register_estimate_hint: `capped_fields.{key}` must be a non-negative integer, got {}",
                        v.type_name()
                    )));
                };
                if cap < 0 {
                    return Err(mlua::Error::runtime(format!(
                        "register_estimate_hint: `capped_fields.{key}` must be a non-negative integer"
                    )));
                }
                hint.capped_fields.insert(key, cap as usize);
            }
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "register_estimate_hint: `capped_fields` must be a table, got {}",
                other.type_name()
            )));
        }
    }
    Ok(hint)
}

fn register_token_and_style_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    let hints_reg = Rc::new(RefCell::new(HintRegistry::default()));
    // tokens(string | table) -> the one token estimate (compaction, /context and
    // providers all use it): text is bytes / 4, a table is its JSON
    // with each inline image counted as IMAGE_ESTIMATE_BYTES instead of its base64 payload,
    // plus any wire-declared estimation hints registered in this VM.
    let reg_for_tokens = Rc::clone(&hints_reg);
    t.set(
        "tokens",
        lua.create_function(move |lua, v: Value| {
            Ok(match v {
                Value::String(s) => approx_tokens(s.as_bytes().len()),
                Value::Nil => 0,
                v @ Value::Table(_) => {
                    let json = mare_shared::lua::lua_to_json(lua, v)?;
                    let reg = reg_for_tokens.borrow();
                    approx_tokens(estimate_bytes_with_hints(&json, &reg.merged))
                }
                other => approx_tokens(other.to_string()?.len()),
            })
        })?,
    )?;
    let reg_for_register = Rc::clone(&hints_reg);
    t.set(
        "register_estimate_hint",
        lua.create_function(move |lua, (name, spec): (String, Table)| {
            if !valid_hint_name(&name) {
                return Err(mlua::Error::runtime(format!(
                    "register_estimate_hint: name `{name}` must be namespaced (`<namespace>:<id>`, lowercase ASCII segments)"
                )));
            }
            let hint = parse_estimate_hint(&spec)?;
            let generation = reg_for_register.borrow_mut().insert(name.clone(), hint);
            let reg_for_dispose = Rc::clone(&reg_for_register);
            lua.create_function(move |_, ()| {
                reg_for_dispose.borrow_mut().remove(&name, generation);
                Ok(())
            })
        })?,
    )?;
    t.set("image_tokens", approx_tokens(IMAGE_ESTIMATE_BYTES))?;
    crate::yaml::install(lua, t)?;
    t.set("width", lua.create_function(|_, s: String| Ok(mare_shared::style::str_width(&s)))?)?;
    t.set(
        "trunc_width",
        lua.create_function(|_, (s, w): (String, usize)| Ok(mare_shared::style::truncate_str(&s, w, "…")))?,
    )?;
    t.set("sanitize", lua.create_function(|_, s: String| Ok(mare_shared::style::sanitize(&s)))?)?;
    t.set(
        "fuzzy",
        lua.create_function(|_, (q, items, limit): (String, Vec<String>, Option<usize>)| {
            Ok(fuzzy(&q, items, limit.unwrap_or(50)))
        })?,
    )?;
    t.set(
        "closest",
        lua.create_function(|_, (word, candidates): (String, Table)| {
            let mut owned = Vec::new();
            for pair in candidates.pairs::<Value, Value>() {
                let (k, v) = pair?;
                match (k, v) {
                    (Value::String(s), _) => owned.push(s.to_str()?.to_owned()),
                    (_, Value::String(s)) => owned.push(s.to_str()?.to_owned()),
                    _ => {}
                }
            }
            owned.sort_unstable();
            owned.dedup();
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            Ok(mare_shared::layout::closest_match(&word, &refs).map(str::to_owned))
        })?,
    )?;
    Ok(())
}

fn register_line_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // split_lines(s, from?, to?) -> lines, total. Lines as `str::lines` splits them; with `from`
    // only lines from..=to (1-based, `to` default the end) become Lua strings — a window of a
    // big file doesn't build one string per line. Bounds act like Lua's `for i = from, to` over
    // the full list (a fractional `from` selects nothing, `to` is floored); `total` counts all.
    t.set(
        "split_lines",
        lua.create_function(|_, (s, from, to): (mlua::LuaString, Option<f64>, Option<f64>)| {
            let s = s.to_str()?;
            let from = from.unwrap_or(1.0);
            // a NaN `to` is no bound, like Lua's math.min(total, NaN) = total
            let to = to.filter(|t| !t.is_nan()).map_or(f64::INFINITY, f64::floor);
            let mut lines = vec![];
            let mut total = 0usize;
            for l in s.lines() {
                total += 1;
                let i = total as f64;
                if from.fract() == 0.0 && i >= from && i <= to {
                    lines.push(l.to_string());
                }
            }
            Ok((lines, total))
        })?,
    )?;
    // is_utf8(s) -> whether s is valid UTF-8 (the same strings Lua's strict utf8.len accepts:
    // no overlongs, surrogates or code points past U+10FFFF), validated without a Lua loop
    t.set("is_utf8", lua.create_function(|_, s: mlua::LuaString| Ok(std::str::from_utf8(&s.as_bytes()).is_ok()))?)?;
    Ok(())
}

pub fn install(lua: &Lua, mare: &Table, cwd: PathBuf) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "regex",
        lua.create_function(|lua, p: String| match regex::Regex::new(&p) {
            Ok(r) => Ok(MultiValue::from_vec(vec![Value::UserData(lua.create_userdata(Regex(r))?)])),
            Err(e) => nil_err(lua, e),
        })?,
    )?;
    truncate::register_truncate_ops(lua, &t)?;
    register_shell_ops(lua, &t)?;
    register_token_and_style_ops(lua, &t)?;
    edit::register_edit_ops(lua, &t, cwd)?;
    register_line_ops(lua, &t)?;
    web::install(lua, &t)?;
    mare.set("text", t)?;
    Ok(())
}

#[cfg(test)]
mod shell_tests {
    use super::{
        IMAGE_ESTIMATE_BYTES, TokenEstimateHints, approx_tokens, cut, estimate_bytes, estimate_bytes_with_hints,
        is_compound,
    };

    #[test]
    fn token_estimate_counts_bytes_and_images_as_a_constant() {
        assert_eq!(approx_tokens("abcd".len()), 1);
        assert_eq!(approx_tokens("中文".len()), 2, "6 bytes, not 2 characters");
        let img = |data: &str| serde_json::json!([{ "type": "text", "text": "hi" }, { "type": "image", "mime": "image/png", "data": data }]);
        let small = estimate_bytes(&img("eA=="));
        let huge = estimate_bytes(&img(&"A".repeat(2_000_000)));
        assert_eq!(small, huge, "the payload size doesn't matter");
        assert!((IMAGE_ESTIMATE_BYTES..IMAGE_ESTIMATE_BYTES + 100).contains(&small), "{small}");
        let url = serde_json::json!({ "type": "input_image", "image_url": format!("data:image/png;base64,{}", "A".repeat(100_000)) });
        assert!(estimate_bytes(&url) < IMAGE_ESTIMATE_BYTES + 100);
        let mut anthropic_hints = TokenEstimateHints::default();
        anthropic_hints.image_mime_keys.insert("media_type".into());
        let anthropic = serde_json::json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "A".repeat(50_000) } });
        assert!(estimate_bytes_with_hints(&anthropic, &anthropic_hints) < 2 * IMAGE_ESTIMATE_BYTES);
    }

    #[test]
    fn cut_never_splits_utf8() {
        let s = "a日本🙂b".as_bytes(); // 1 + 3 + 3 + 4 + 1 bytes
        let c = |n, tail, chars| std::str::from_utf8(cut(s, n, tail, chars)).unwrap().to_string();
        assert_eq!(c(5, false, false), "a日"); // byte 5 is inside 本: back off
        assert_eq!(c(7, false, false), "a日本");
        assert_eq!(c(3, true, false), "b"); // the cut lands inside 🙂: move forward
        assert_eq!(c(5, true, false), "🙂b");
        assert_eq!(c(0, false, false), "");
        assert_eq!(c(99, true, false), "a日本🙂b");
        assert_eq!(c(3, false, true), "a日本");
        assert_eq!(c(2, true, true), "🙂b");
        assert_eq!(c(0, true, true), "");
        assert_eq!(c(9, true, true), "a日本🙂b");
        // not UTF-8: code-point units fall back to bytes
        assert_eq!(cut(b"ab\xffcd", 3, false, true), b"ab\xff");
        assert_eq!(cut(b"ab\xffcd", 3, true, true), b"\xffcd");
    }

    #[test]
    fn compound_detection() {
        for c in ["git push; rm -rf /", "a && b", "a | b", "echo $(id)", "echo `id`", "x > f", "a\nb", "echo \"$(id)\""]
        {
            assert!(is_compound(c), "{c}");
        }
        for c in ["git push origin main", "echo 'a; b | c'", "echo \"a && b\"", "grep -r \\; ."] {
            assert!(!is_compound(c), "{c}");
        }
    }
}

#[cfg(test)]
mod lua_tests {
    #[test]
    fn patch_paths_parse_without_reading_files() {
        let dir = tempfile::tempdir().unwrap();
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            r#"
            local paths, err = mare.text.patch_paths([[*** Begin Patch
*** Add File: missing/add
+new
*** Delete File: missing/delete
*** Update File: missing/source
*** Move to: missing/target
@@
-old
+new
*** End Patch]])
            assert(paths, err)
            assert(#paths == 3)
            assert(paths[1].kind == "add" and paths[1].path == "missing/add")
            assert(paths[2].kind == "delete" and paths[2].path == "missing/delete")
            assert(paths[3].kind == "update" and paths[3].path == "missing/source"
                and paths[3].move_to == "missing/target")
            local bad, why = mare.text.patch_paths("not a patch")
            assert(bad == nil and type(why) == "string")
            local empty = mare.text.patch_paths("*** Begin Patch\n*** End Patch")
            assert(empty == nil)
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "parsing must not touch files");
    }

    /// The read / edit tools swapped Lua's `utf8.len` and a full `split_lines` + slice for
    /// these two: they must answer exactly the same.
    #[test]
    fn is_utf8_and_windowed_split_lines_match_the_lua_they_replace() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare, std::env::temp_dir()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        let utf8_len_ok: mlua::Function = lua.load("return function(s) return utf8.len(s) ~= nil end").eval().unwrap();
        let is_utf8: mlua::Function = lua.load("return mare.text.is_utf8").eval().unwrap();
        // random byte strings biased towards UTF-8 lead/continuation bytes and edge values
        let pool = [0x00, 0x0a, 0x41, 0x7f, 0x80, 0xbf, 0xc0, 0xc2, 0xdf, 0xe0, 0xed, 0xef, 0xf0, 0xf4, 0xf5, 0xff];
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        for n in 0..20_000 {
            let mut s = vec![];
            for _ in 0..n % 9 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push(if x.is_multiple_of(3) { (x >> 8) as u8 } else { pool[(x >> 20) as usize % pool.len()] });
            }
            let s = lua.create_string(&s).unwrap();
            let want: bool = utf8_len_ok.call(&s).unwrap();
            assert_eq!(is_utf8.call::<bool>(&s).unwrap(), want, "{:?}", s.as_bytes());
        }
        let same: mlua::Function = lua
            .load(
                r#"return function(s, from, to)
                    local all = mare.text.split_lines(s)
                    local want = {}
                    for i = from, math.min(#all, to) do want[#want + 1] = all[i] end
                    local got, total = mare.text.split_lines(s, from, to)
                    if total ~= #all or #got ~= #want then return false end
                    for i = 1, #want do if got[i] ~= want[i] then return false end end
                    return true
                end"#,
            )
            .eval()
            .unwrap();
        for s in ["", "\n", "a", "a\n", "a\r\nb", "a\r\nb\r\n", "\r", "a\r", "x\n\ny\n\n", "a\rb\nc"] {
            for (from, to) in [(1.0, 1.0), (1.0, 2000.0), (2.0, 3.0), (3.0, 2.0), (5.0, 9.0), (2.0, 2.5), (1.5, 9.0)] {
                assert!(same.call::<bool>((s, from, to)).unwrap(), "{s:?} {from}..{to}");
            }
        }
    }

    #[tokio::test]
    async fn apply_patch_literal_tilde_dir_and_lock_cleanup_on_rejected_patches() {
        let dir = tempfile::tempdir().unwrap();
        let tilde_dir = dir.path().join("~");
        std::fs::create_dir_all(&tilde_dir).unwrap();
        std::fs::write(tilde_dir.join("src.txt"), "before\n").unwrap();

        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
        crate::fs::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
        crate::exec_ctx::install(&lua, &mare).unwrap();
        crate::task::install(&lua, &mare).unwrap();
        crate::codec::install(&lua, &mare).unwrap();
        mare.set("cwd", dir.path().to_string_lossy().into_owned()).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let guard_src = include_str!("../../../mare-plugin/preset/plugins/lua/tools/file_guard.lua");
        let guard_loader: mlua::Function = lua.load(guard_src).set_name("@preset/tools/file_guard.lua").eval().unwrap();
        lua.globals().set("LOAD_GUARD", guard_loader).unwrap();

        let patch_src = include_str!("../../../mare-plugin/preset/plugins/tools-apply-patch.lua");
        let patch_def: mlua::Table = lua.load(patch_src).set_name("@preset/tools-apply-patch.lua").eval().unwrap();
        lua.globals().set("PATCH_DEF", patch_def).unwrap();

        lua.globals().set("EXPECTED_TILDE_SRC", tilde_dir.join("src.txt").to_string_lossy().into_owned()).unwrap();
        lua.globals().set("EXPECTED_TILDE_DST", tilde_dir.join("dst.txt").to_string_lossy().into_owned()).unwrap();

        tokio::task::LocalSet::new()
            .run_until(
                lua.load(
                    r#"
                    package.loaded.mare_lib = {
                        rel = function(p) return p end,
                    }
                    local registered = {}
                    local tools = {
                        register = function(_, name, def) registered[name] = def end,
                        edit_mode = function() return "apply_patch" end,
                    }
                    local fg = LOAD_GUARD({
                        ctx = { on = function() end, bus = { emit = function() end } },
                        cfg = { read_before_edit = true, edit_max_bytes = 1048576 },
                        registry = { tools = tools },
                        policy = { decide = function() return { decision = "allow" } end },
                    })
                    local raw_observe = tools.observe
                    local raw_guard = tools.fs_guard
                    local raw_commit = tools.fs_commit
                    local raw_lock = tools.with_file_lock
                    tools.observe = function(self, path, data) return raw_observe(self, nil, path, data) end
                    tools.fs_guard = function(self, path, verb, tool) return raw_guard(self, nil, path, verb, tool) end
                    tools.fs_commit = function(self, path, content, cur, tool, opts) return raw_commit(self, nil, path, content, cur, tool, opts) end
                    local locked_paths = nil
                    tools.with_file_lock = function(self, paths, fn)
                        locked_paths = paths
                        return raw_lock(self, nil, paths, fn)
                    end
                    PATCH_DEF.apply({
                        tools = tools,
                        bus = { emit = function() end },
                        inject = function() end,
                    })
                    local apply = registered.apply_patch

                    -- 1. patch_paths({ resolve = true }) resolves ~/ literally against cwd (matching apply_patch::resolve).
                    local patch = table.concat({
                        "*** Begin Patch",
                        "*** Update File: ~/src.txt",
                        "*** Move to: ~/dst.txt",
                        "@@",
                        "-before",
                        "+after",
                        "*** End Patch",
                    }, "\n")
                    local resolved = assert(mare.text.patch_paths(patch, { resolve = true }))
                    assert(resolved[1].path == EXPECTED_TILDE_SRC, "got " .. tostring(resolved[1].path))
                    assert(resolved[1].move_to == EXPECTED_TILDE_DST, "got " .. tostring(resolved[1].move_to))

                    -- Read ~/src.txt first (required by fs_guard) and record seen content.
                    local content = assert(mare.fs.read(EXPECTED_TILDE_SRC))
                    assert(content == "before\n")
                    fg.record(EXPECTED_TILDE_SRC, fg.version_of(EXPECTED_TILDE_SRC, content))

                    -- Verify literal ~/ paths are locked (not expanded $HOME).
                    local out = apply.execute({ input = patch })
                    assert(not out.is_error, tostring(out.content))
                    assert(locked_paths[1] == EXPECTED_TILDE_SRC and locked_paths[2] == EXPECTED_TILDE_DST)
                    assert(fg.lock_count() == 0, "lock table must be empty after patch succeeds")

                    -- 2. Repeated rejected patches on distinct nonexistent paths must not leak file_locks entries.
                    for i = 1, 25 do
                        local bad_patch = "*** Begin Patch\n*** Update File: missing_" .. i .. ".txt\n@@\n-a\n+b\n*** End Patch"
                        local rej = apply.execute({ input = bad_patch })
                        assert(rej.is_error)
                    end
                    assert(fg.lock_count() == 0, "expected 0 leaked locks, got " .. fg.lock_count())
                    "#,
                )
                .exec_async(),
            )
            .await
            .unwrap();

        assert!(!tilde_dir.join("src.txt").exists());
        assert_eq!(std::fs::read_to_string(tilde_dir.join("dst.txt")).unwrap(), "after\n");
    }

    #[test]
    fn estimate_bytes_caps_encrypted_content_with_flat_budget() {
        use super::{IMAGE_ESTIMATE_BYTES, TokenEstimateHints, estimate_bytes_with_hints};
        let mut hints = TokenEstimateHints::default();
        hints.capped_fields.insert("encrypted_content".into(), IMAGE_ESTIMATE_BYTES);
        let small = serde_json::json!([{ "type": "compaction", "encrypted_content": "ENC-COMPACTED" }]);
        let huge_blob = "X".repeat(5_000_000);
        let huge = serde_json::json!([{ "type": "compaction", "encrypted_content": huge_blob }]);
        assert!(
            estimate_bytes_with_hints(&small, &hints) < 200,
            "short test blobs remain exact: {}",
            estimate_bytes_with_hints(&small, &hints)
        );
        let est = estimate_bytes_with_hints(&huge, &hints);
        assert!(
            (IMAGE_ESTIMATE_BYTES..=IMAGE_ESTIMATE_BYTES + 200).contains(&est),
            "5MB encrypted_content must use flat conservative budget (~{IMAGE_ESTIMATE_BYTES}B), got {est}"
        );
    }

    #[test]
    fn text_closest_and_patch_paths_resolve_option_are_public() {
        let dir = tempfile::tempdir().unwrap();
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.globals()
            .set("EXPECTED_TILDE_SRC", dir.path().join("~").join("src.txt").to_string_lossy().into_owned())
            .unwrap();
        lua.globals()
            .set("EXPECTED_TILDE_DST", dir.path().join("~").join("dst.txt").to_string_lossy().into_owned())
            .unwrap();
        lua.load(
            r#"
            assert(mare.text._patch_paths_resolved == nil, "_patch_paths_resolved must be removed")
            assert(type(mare.text.closest) == "function", "mare.text.closest must be public")

            -- List and set candidates + deterministic tie-breaking + miss.
            assert(mare.text.closest("modle", { "model", "effort", "theme" }) == "model")
            assert(mare.text.closest("modle", { model = true, effort = true, theme = true }) == "model")
            assert(mare.text.closest("ab", { bb = true, aa = true }) == "aa")
            assert(mare.text.closest("zzzzzz", { "model", "effort" }) == nil)
            assert(mare.text.closest("modle", {}) == nil)

            -- patch_paths({ resolve = true }) resolves literally against cwd, matching apply_patch.
            local patch = table.concat({
                "*** Begin Patch",
                "*** Update File: ~/src.txt",
                "*** Move to: ~/dst.txt",
                "@@",
                "-before",
                "+after",
                "*** End Patch",
            }, "\n")
            local unresolved = assert(mare.text.patch_paths(patch))
            assert(unresolved[1].path == "~/src.txt" and unresolved[1].move_to == "~/dst.txt")
            local unresolved_explicit = assert(mare.text.patch_paths(patch, { resolve = false }))
            assert(unresolved_explicit[1].path == "~/src.txt" and unresolved_explicit[1].move_to == "~/dst.txt")
            local resolved = assert(mare.text.patch_paths(patch, { resolve = true }))
            assert(resolved[1].path == EXPECTED_TILDE_SRC, "got " .. tostring(resolved[1].path))
            assert(resolved[1].move_to == EXPECTED_TILDE_DST, "got " .. tostring(resolved[1].move_to))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn token_estimate_requires_wire_hints_for_provider_shapes_and_capped_fields() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare, std::env::temp_dir()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            r#"
            assert(type(mare.text.register_estimate_hint) == "function", "register_estimate_hint must exist")
            local payload = string.rep("A", 50000)
            local anthropic = { type = "image", source = { type = "base64", media_type = "image/png", data = payload } }
            local gemini = { inlineData = { mimeType = "image/png", data = payload } }
            local enc = { { type = "compaction", encrypted_content = payload } }

            -- Without wire hints, provider-specific image shapes and opaque ciphertext fields are NOT hardcoded in core.
            assert(mare.text.tokens(anthropic) > 10000, "core must not hardcode Anthropic media_type shape")
            assert(mare.text.tokens(gemini) > 10000, "core must not hardcode Gemini mimeType shape")
            assert(mare.text.tokens(enc) > 10000, "core must not hardcode encrypted_content field")

            -- Wire plugins declare their shapes and capped fields via register_estimate_hint (namespaced).
            local dispose = mare.text.register_estimate_hint("test:wire", {
                image_types = { "input_image" },
                image_mime_keys = { "media_type", "mimeType" },
                capped_fields = { encrypted_content = 4800 },
            })
            assert(type(dispose) == "function", "register_estimate_hint must return a disposer")
            assert(mare.text.tokens(anthropic) < 2500, "Anthropic shape should use flat image budget with hint")
            assert(mare.text.tokens(gemini) < 2500, "Gemini shape should use flat image budget with hint")
            assert(mare.text.tokens(enc) <= 1300, "encrypted_content should use flat cap with hint")

            -- Namespacing and structural validation reject malformed registrations.
            for _, bad_name in ipairs({ "", "bare", ":foo", "foo:", "a::b", "Bad:Caps", "has space:x" }) do
                local ok = pcall(mare.text.register_estimate_hint, bad_name, {})
                assert(not ok, "must reject un-namespaced/invalid hint name: " .. bad_name)
            end
            for _, bad_spec in ipairs({
                { unknown_key = {} },
                { image_types = { "" } },
                { image_types = { 123 } },
                { image_types = { foo = "bar" } },
                { image_mime_keys = "media_type" },
                { capped_fields = { foo = -1 } },
                { capped_fields = { foo = 1.5 } },
                { capped_fields = { [""] = 10 } },
                { capped_fields = { [1] = 10 } },
            }) do
                local ok = pcall(mare.text.register_estimate_hint, "test:invalid", bad_spec)
                assert(not ok, "must reject malformed estimate_hints spec")
            end

            -- Disposing the wire hint restores unhinted estimation.
            dispose()
            assert(mare.text.tokens(anthropic) > 10000, "disposed Anthropic hint must no longer apply")
            assert(mare.text.tokens(gemini) > 10000, "disposed Gemini hint must no longer apply")
            assert(mare.text.tokens(enc) > 10000, "disposed encrypted_content hint must no longer apply")
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn non_ascii_mime_does_not_panic_and_single_line_cap_marks_truncated() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare, std::env::temp_dir()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            r#"
            -- Non-ASCII mime values whose UTF-8 char spans byte 6 must not panic during token estimation.
            for _, bad_mime in ipairs({ "a中文", "ab中文", "éééé", "图片/png", "image" }) do
                local n = mare.text.tokens({ mime = bad_mime, data = string.rep("x", 100) })
                assert(type(n) == "number" and n > 0)
            end
            -- Case-insensitive ASCII "IMAGE/png" still uses the flat image budget.
            local flat = mare.text.tokens({ mime = "IMAGE/png", data = string.rep("A", 50000) })
            assert(flat < 2500, "expected flat image budget for IMAGE/png, got " .. tostring(flat))
            "#,
        )
        .exec()
        .unwrap();

        // Single line between max_bytes/2 and max_bytes gets capped by cap_line and must report truncated = true.
        let single_line = "a".repeat(30_000);
        let res = super::truncate::truncate(&single_line, 2000, 50 * 1024, super::truncate::Keep::Head, 0);
        assert!(res.text.contains("[line truncated]"), "expected single-line cap marker");
        assert!(res.truncated, "single-line cap must set truncated = true so callers spill full output");

        // Short line within both limits stays untruncated.
        let short = super::truncate::truncate("hello world", 2000, 50 * 1024, super::truncate::Keep::Head, 0);
        assert!(!short.truncated);
    }
}
