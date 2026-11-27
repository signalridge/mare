//! Terminal detection and Windows Terminal settings parsing: pure functions over
//! environment lookups and settings text, so they are testable on every platform.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermKind {
    WindowsTerminal,
    /// VS Code, WezTerm, Alacritty, ConEmu, mintty… — modern enough, not checked further
    OtherModern,
    /// the built-in conhost window (known issues: `;0;rgb:` fragments in the input, a full
    /// redraw per key press)
    LegacyConhost,
}

pub fn detect(var: &dyn Fn(&str) -> Option<String>) -> TermKind {
    if var("WT_SESSION").is_some() {
        return TermKind::WindowsTerminal;
    }
    let modern = ["TERM_PROGRAM", "WEZTERM_PANE", "ALACRITTY_WINDOW_ID", "ConEmuPID", "MSYSTEM", "KITTY_WINDOW_ID"];
    if modern.iter().any(|k| var(k).is_some()) {
        return TermKind::OtherModern;
    }
    TermKind::LegacyConhost
}

/// Windows Terminal's settings.json locations (stable, preview, unpackaged).
pub fn settings_candidates(local_app_data: &Path) -> Vec<PathBuf> {
    vec![
        local_app_data.join(r"Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json"),
        local_app_data.join(r"Packages\Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe\LocalState\settings.json"),
        local_app_data.join(r"Microsoft\Windows Terminal\settings.json"),
    ]
}

/// Drop `//` and `/* */` comments outside strings (settings.json is JSONC).
fn strip_jsonc(s: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(s.len()), s.chars().peekable());
    let (mut in_str, mut esc) = (false, false);
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    // trailing commas are accepted by Windows Terminal too
    strip_trailing_commas(&out)
}

fn strip_trailing_commas(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut esc = false;
    for (i, &c) in b.iter().enumerate() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        if c == ',' {
            let next = b[i + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some(']' | '}')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Which of the keys mare needs are forwarded: a `sendInput` action, or `unbound` (the key then
/// reaches the application). Returns `(shift_enter, alt_enter)`; None if the file isn't JSON.
pub fn forwarded_keys(settings: &str) -> Option<(bool, bool)> {
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(settings)).ok()?;
    let mut found = (false, false);
    let mut by_id = std::collections::HashMap::new();
    for list in ["actions", "keybindings"] {
        for a in v.get(list).and_then(|x| x.as_array()).into_iter().flatten() {
            if let (Some(id), Some(cmd)) = (a.get("id").and_then(|x| x.as_str()), a.get("command")) {
                by_id.insert(id.to_string(), cmd.clone());
            }
        }
    }
    let forwards = |cmd: &serde_json::Value| match cmd {
        serde_json::Value::String(s) => s == "unbound",
        serde_json::Value::Object(o) => o.get("action").and_then(|x| x.as_str()) == Some("sendInput"),
        _ => false,
    };
    for list in ["actions", "keybindings"] {
        for a in v.get(list).and_then(|x| x.as_array()).into_iter().flatten() {
            let keys: Vec<String> = match a.get("keys") {
                Some(serde_json::Value::String(k)) => vec![k.to_ascii_lowercase()],
                Some(serde_json::Value::Array(ks)) => {
                    ks.iter().filter_map(|k| k.as_str().map(str::to_ascii_lowercase)).collect()
                }
                _ => continue,
            };
            // newer settings split actions ({id, command}) and keybindings ({id, keys})
            let cmd = match a.get("command") {
                Some(c) => c.clone(),
                None => a.get("id").and_then(|x| x.as_str()).and_then(|id| by_id.get(id).cloned()).unwrap_or_default(),
            };
            if forwards(&cmd) {
                found.0 |= keys.iter().any(|k| k == "shift+enter");
                found.1 |= keys.iter().any(|k| k == "alt+enter");
            }
        }
    }
    Some(found)
}

pub const SETUP: &str = r#"In Windows Terminal open settings.json (Ctrl+Shift+,) and add to "actions":
  { "command": { "action": "sendInput", "input": "\u001b[13;2u" }, "keys": "shift+enter" },
  { "command": { "action": "sendInput", "input": "\u001b\r" }, "keys": "alt+enter" }
then restart Windows Terminal. Without them: Ctrl+J inserts a newline, Ctrl+Q queues a follow-up."#;

/// One finding: (ok, detail).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_terminals() {
        assert_eq!(detect(&|k| (k == "WT_SESSION").then(|| "x".into())), TermKind::WindowsTerminal);
        assert_eq!(detect(&|k| (k == "TERM_PROGRAM").then(|| "vscode".into())), TermKind::OtherModern);
        assert_eq!(detect(&|_| None), TermKind::LegacyConhost);
    }

    #[test]
    fn reads_forwarded_keys_from_jsonc() {
        let s = r#"{
            // comment with "quotes" and a url http://x
            "actions": [
                { "command": { "action": "sendInput", "input": "\u001b[13;2u" }, "keys": "shift+enter" },
                /* block */ { "command": "toggleFullscreen", "keys": "alt+enter" },
            ],
        }"#;
        assert_eq!(forwarded_keys(s), Some((true, false)));
        let split = r#"{ "actions": [ { "id": "User.fwd", "command": { "action": "sendInput", "input": "\u001b\r" } } ],
                        "keybindings": [ { "id": "User.fwd", "keys": "alt+enter" }, { "command": "unbound", "keys": "shift+enter" } ] }"#;
        assert_eq!(forwarded_keys(split), Some((true, true)));
        assert_eq!(forwarded_keys("not json"), None);
    }
}
