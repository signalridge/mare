//! YAML for `mare.text.yaml` / `mare.text.frontmatter` (a real YAML parser, not a line
//! reader). yaml-rust2: YAML 1.2, pure safe Rust, passes the yaml-test-suite.
//!
//! Frontmatter splitting normalises BOM and CRLF; the block must open on the first line and close
//! with a `---` line. A block that isn't valid YAML still yields fields, read line by line
//! (`key: value`, quotes stripped) — matching the tolerant parsing expected for `~/.agents/skills` —
//! plus the parse error as a warning, so one stray colon never makes a skill disappear.

use mlua::{Lua, Table, Value};
use yaml_rust2::{Yaml, YamlLoader};

/// A YAML document as a JSON value (maps keep their order; non-string keys are stringified;
/// aliases and bad values become null). Only the first document of a stream is used.
pub fn parse(src: &str) -> Result<serde_json::Value, String> {
    let docs = YamlLoader::load_from_str(src).map_err(|e| e.to_string())?;
    Ok(docs.first().map(to_json).unwrap_or(serde_json::Value::Null))
}

fn key_string(k: &Yaml) -> String {
    match k {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Null => "null".into(),
        other => format!("{other:?}"),
    }
}

fn to_json(y: &Yaml) -> serde_json::Value {
    use serde_json::Value as J;
    match y {
        Yaml::String(s) => J::String(s.clone()),
        Yaml::Integer(i) => J::from(*i),
        // the lexical form is kept when it isn't a finite f64 (".inf", ".nan")
        Yaml::Real(s) => s
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(J::Number)
            .unwrap_or_else(|| J::String(s.clone())),
        Yaml::Boolean(b) => J::Bool(*b),
        Yaml::Array(a) => J::Array(a.iter().map(to_json).collect()),
        Yaml::Hash(h) => J::Object(h.iter().map(|(k, v)| (key_string(k), to_json(v))).collect()),
        Yaml::Alias(_) | Yaml::Null | Yaml::BadValue => J::Null,
    }
}

/// Split `text` into (frontmatter source, body). `None` when there is no frontmatter block.
pub fn split(text: &str) -> Option<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let rest = norm.strip_prefix("---\n").or_else(|| (norm == "---").then_some(""))?;
    // the closing fence is a line that is exactly `---` (an empty block closes at once)
    let (yaml, body) = if let Some(after) = rest.strip_prefix("---") {
        ("", after)
    } else {
        let end = rest.find("\n---")?;
        (&rest[..end], &rest[end + 4..])
    };
    let body = match body.split_once('\n') {
        Some((fence_tail, b)) if fence_tail.trim().is_empty() => b,
        None if body.trim().is_empty() => "",
        _ => return None, // `---x`: not a fence
    };
    Some((yaml.to_string(), body.to_string()))
}

/// The pre-YAML reader: one `key: value` per line, surrounding quotes stripped.
fn line_fields(src: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for line in src.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            continue;
        }
        let v = v.trim();
        let v = ['"', '\''].iter().find_map(|q| v.strip_prefix(*q).and_then(|s| s.strip_suffix(*q))).unwrap_or(v);
        out.insert(k.to_string(), serde_json::Value::String(v.to_string()));
    }
    out
}

/// (fields, body, warning): fields is an object (empty without frontmatter); the warning says
/// why the block was read line by line (invalid YAML, or not a mapping).
pub fn frontmatter(text: &str) -> (serde_json::Value, String, Option<String>) {
    let Some((src, body)) = split(text) else {
        return (serde_json::Value::Object(Default::default()), text.to_string(), None);
    };
    match parse(&src) {
        Ok(v @ serde_json::Value::Object(_)) => (v, body, None),
        Ok(serde_json::Value::Null) => (serde_json::Value::Object(Default::default()), body, None),
        Ok(_) => (serde_json::Value::Object(line_fields(&src)), body, Some("frontmatter is not a YAML mapping".into())),
        Err(e) => (
            serde_json::Value::Object(line_fields(&src)),
            body,
            Some(format!("frontmatter is not valid YAML ({e}); read line by line")),
        ),
    }
}

pub fn install(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // yaml(s) -> value | nil, err: the first document of a YAML 1.2 stream
    t.set(
        "yaml",
        lua.create_function(|lua, s: String| match parse(&s) {
            Ok(v) => Ok((mare_shared::lua::json_to_lua(lua, &v)?, Value::Nil)),
            Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e)?))),
        })?,
    )?;
    // frontmatter(text) -> fields, body, warning?: a leading `---` YAML block (SKILL.md, prompt
    // templates); invalid YAML falls back to `key: value` lines and says so
    t.set(
        "frontmatter",
        lua.create_function(|lua, s: mlua::LuaString| {
            let (fields, body, warn) = frontmatter(&s.to_string_lossy());
            let w = match warn {
                Some(w) => Value::String(lua.create_string(w)?),
                None => Value::Nil,
            };
            Ok((mare_shared::lua::json_to_lua(lua, &fields)?, body, w))
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yaml_block_scalars_quotes_maps_and_comments() {
        let (f, body, w) = frontmatter(
            "---\nname: my-skill # trailing comment\n# a comment line\ndescription: >\n  folded one\n  two\n\n  para\n\
             literal: |\n  keep\n  lines\nquoted: \"tab\\there \\\"q\\\" \\u00e9\"\nsingle: 'it''s'\n\
             metadata:\n  author: me\n  version: \"1.0\"\ntier: 2\n---\nBODY\n",
        );
        assert!(w.is_none(), "{w:?}");
        assert_eq!(body, "BODY\n");
        assert_eq!(f["name"], "my-skill");
        assert_eq!(f["description"], "folded one two\npara\n");
        assert_eq!(f["literal"], "keep\nlines\n");
        assert_eq!(f["quoted"], "tab\there \"q\" é");
        assert_eq!(f["single"], "it's");
        assert_eq!(f["metadata"], json!({ "author": "me", "version": "1.0" }));
        assert_eq!(f["tier"], 2, "unknown keys are kept");
    }

    #[test]
    fn invalid_yaml_falls_back_to_lines_with_a_warning() {
        let (f, body, w) = frontmatter("---\nname: x\ndescription: Use when: things happen\n---\nb");
        assert_eq!(f["description"], "Use when: things happen");
        assert_eq!(body, "b");
        assert!(w.unwrap().contains("not valid YAML"));
    }

    #[test]
    fn fences_bom_crlf_and_no_frontmatter() {
        let (f, body, _) = frontmatter("\u{feff}---\r\ndescription: d\r\n---\r\nbody\r\n");
        assert_eq!(f["description"], "d");
        assert_eq!(body, "body\n");
        let (f, body, w) = frontmatter("no frontmatter\n---\n");
        assert_eq!((f, body.as_str(), w), (json!({}), "no frontmatter\n---\n", None));
        let (f, body, _) = frontmatter("---\n---\nbody");
        assert_eq!((f, body.as_str()), (json!({}), "body"));
        assert!(split("---\nunclosed: 1\n").is_none());
    }
}
