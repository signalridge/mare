//! Content search in session files (`/resume search`): streamed line by line, one file at a
//! time so the caller can cancel between files; never reads a whole file into memory.

use std::fs::File;
use std::path::Path;

/// Case-insensitive substring search in one session file's message texts (streamed line by
/// line; used for `/resume search`, one file at a time so it can be cancelled).
pub fn file_contains(path: &Path, query: &str) -> bool {
    content_snippet(path, query).is_some()
}

/// First match of `query` in a session's *message text* (case-insensitive), as a short snippet
/// around it: JSON keys, tool arguments and metadata don't match.
/// Lines are pre-filtered on the JSON-escaped query (so `"` or newlines in it still match),
/// then only message entries are decoded.
pub fn content_snippet(path: &Path, query: &str) -> Option<String> {
    use std::io::BufRead;
    let q = query.to_lowercase();
    if q.is_empty() {
        return None;
    }
    let escaped = serde_json::to_string(&q).ok()?;
    let escaped = escaped.trim_matches('"').to_string();
    let f = File::open(path).ok()?;
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if !line.to_lowercase().contains(&escaped) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let content = &v["message"]["content"];
        let texts: Vec<&str> = match content {
            serde_json::Value::String(s) => vec![s.as_str()],
            serde_json::Value::Array(a) => a
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect(),
            _ => vec![],
        };
        for t in texts {
            if let Some(s) = snippet(t, &q) {
                return Some(s);
            }
        }
    }
    None
}

/// `…context match context…` (about 40 chars of context each side, on char boundaries)
fn snippet(text: &str, q_lower: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find(q_lower)?;
    // lowercasing can change byte lengths (non-ASCII): fall back to the lowercased text then
    let src = if lower.len() == text.len() { text } else { lower.as_str() };
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let ci = chars.iter().position(|(b, _)| *b >= at).unwrap_or(0);
    let qn = q_lower.chars().count();
    let from = ci.saturating_sub(40);
    let to = (ci + qn + 40).min(chars.len());
    let mut out: String = chars[from..to].iter().map(|(_, c)| if c.is_whitespace() { ' ' } else { *c }).collect();
    if from > 0 {
        out.insert(0, '…');
    }
    if to < chars.len() {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_matches_message_text_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let lines = [
            json!({"type":"session","session_id":"s","cwd":"/w"}),
            json!({"type":"message","entry_id":"a","message":{"role":"assistant","content":[
                {"type":"tool_call","name":"read","arguments":{"path":"needle.rs"}}]}}),
            json!({"type":"message","entry_id":"b","message":{"role":"user","content":[
                {"type":"text","text":"please fix the \"Needle\" parser\nin src"}]}}),
        ];
        std::fs::write(&p, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        // tool arguments and keys don't match; message text does, with quotes in the query
        assert_eq!(content_snippet(&p, "type"), None);
        assert!(content_snippet(&p, "needle.rs").is_none());
        let s = content_snippet(&p, "\"needle\" parser").unwrap();
        assert!(s.contains("\"Needle\" parser"), "{s}");
    }
}
