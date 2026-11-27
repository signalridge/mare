//! Key-conflict hints:
//! chords that a terminal or the OS is known to take before they reach mare. A binding that
//! hits the table is reported once per distinct set of conflicts at startup and marked in the
//! `?` key menu, with how to fix it. mare doesn't scan system or terminal configs: the
//! table is static, and `key_conflicts` in init.lua appends to it.

/// One known conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// chord in mare's key notation ("ctrl+tab")
    pub key: String,
    /// where it applies: a terminal id from [`terminal_id`] or "any"
    pub terminal: String,
    /// who takes it ("Ghostty: next tab")
    pub by: String,
    /// how to fix it
    pub fix: String,
}

fn e(key: &str, terminal: &str, by: &str, fix: &str) -> Entry {
    Entry { key: key.into(), terminal: terminal.into(), by: by.into(), fix: fix.into() }
}

/// The built-in table (defaults of common terminals and multiplexers).
pub fn builtin() -> Vec<Entry> {
    vec![
        e(
            "ctrl+tab",
            "ghostty",
            "Ghostty: next tab",
            "rebind it in mare, or `keybind = ctrl+tab=unbind` in Ghostty's config",
        ),
        e("ctrl+shift+tab", "ghostty", "Ghostty: previous tab", "rebind it in mare, or unbind it in Ghostty's config"),
        e(
            "ctrl+tab",
            "windows-terminal",
            "Windows Terminal: next tab",
            "rebind it in mare, or remove the action in settings.json",
        ),
        e(
            "ctrl+shift+tab",
            "windows-terminal",
            "Windows Terminal: previous tab",
            "rebind it in mare, or remove the action in settings.json",
        ),
        e(
            "alt+enter",
            "windows-terminal",
            "Windows Terminal: toggle fullscreen",
            "unbind `toggleFullscreen` in settings.json (`mare doctor` shows the setting)",
        ),
        e(
            "ctrl+p",
            "vscode",
            "VS Code: Quick Open",
            "add the command to `terminal.integrated.commandsToSkipShell` exclusions, or rebind it in mare",
        ),
        e("ctrl+shift+p", "vscode", "VS Code: Command Palette", "rebind it in mare"),
        e("ctrl+b", "tmux", "tmux: prefix key", "rebind it in mare, or change tmux's `prefix`"),
        e("ctrl+a", "screen", "GNU screen: command key", "rebind it in mare, or change screen's `escape`"),
        e(
            "ctrl+shift+t",
            "kitty",
            "kitty: new tab",
            "rebind it in mare, or `map ctrl+shift+t` to something else in kitty.conf",
        ),
        e("ctrl+shift+enter", "kitty", "kitty: new window", "rebind it in mare, or remap it in kitty.conf"),
    ]
}

/// The terminals (and multiplexers) mare is running inside, from the environment.
pub fn terminal_id(var: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    let prog = var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
    let mut out = vec![];
    if prog == "ghostty" || var("GHOSTTY_RESOURCES_DIR").is_some() {
        out.push("ghostty");
    }
    if var("WT_SESSION").is_some() {
        out.push("windows-terminal");
    }
    if prog == "vscode" {
        out.push("vscode");
    }
    if var("KITTY_WINDOW_ID").is_some() {
        out.push("kitty");
    }
    if var("TMUX").is_some() {
        out.push("tmux");
    }
    if var("STY").is_some() {
        out.push("screen");
    }
    out
}

/// A user binding that the table says won't arrive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub key: String,
    pub action: String,
    pub by: String,
    pub fix: String,
}

/// Bindings (`(key, action)`) that collide with the table for these terminals.
pub fn find(bindings: &[(String, String)], table: &[Entry], terms: &[&str]) -> Vec<Conflict> {
    let mut out = vec![];
    for (key, action) in bindings {
        let k = key.to_ascii_lowercase();
        for t in table {
            if t.key.eq_ignore_ascii_case(&k) && (t.terminal == "any" || terms.contains(&t.terminal.as_str())) {
                out.push(Conflict { key: key.clone(), action: action.clone(), by: t.by.clone(), fix: t.fix.clone() });
            }
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

/// Ambiguous bindings ("配置里的快捷键有歧义时加载阶段就报错"
/// `tui/src/keymap.rs` `validate_conflicts`): a key bound on its own that is also the first
/// key(s) of a longer sequence — the shorter binding always fires first, so the sequence can
/// never be typed. One message per clash, sorted.
pub fn ambiguous(bindings: &[(String, String)]) -> Vec<String> {
    let mut out = vec![];
    for (seq, seq_action) in bindings {
        let tokens: Vec<&str> = seq.split(' ').collect();
        for n in 1..tokens.len() {
            let prefix = tokens[..n].join(" ");
            if let Some((_, action)) = bindings.iter().find(|(k, _)| *k == prefix) {
                out.push(format!(
                    "ambiguous key binding: `{prefix}` ({action}) is also the start of `{seq}` ({seq_action}), \
                     which can never be typed — rebind one of them"
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A stable signature of a conflict set (warn once per distinct set).
pub fn signature(conflicts: &[Conflict]) -> String {
    conflicts.iter().map(|c| format!("{}={}", c.key, c.by)).collect::<Vec<_>>().join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(k: &str, a: &str) -> (String, String) {
        (k.into(), a.into())
    }

    #[test]
    fn conflicts_depend_on_the_terminal_and_the_table_is_appendable() {
        let binds = vec![b("ctrl+tab", "model.next"), b("ctrl+t", "transcript"), b("ctrl+b", "jobs")];
        let table = builtin();
        assert!(find(&binds, &table, &[]).is_empty(), "nothing known to clash outside those terminals");
        let c = find(&binds, &table, &["ghostty", "tmux"]);
        assert_eq!(c.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["ctrl+b", "ctrl+tab"]);
        assert!(c[1].by.contains("Ghostty"));
        // init.lua `key_conflicts` adds entries (any terminal here)
        let mut table = table;
        table.push(e("ctrl+t", "any", "my window manager", "rebind"));
        assert!(find(&binds, &table, &[]).iter().any(|c| c.key == "ctrl+t"));
        assert_ne!(signature(&c), signature(&c[..1]), "a changed set warns again");
    }

    #[test]
    fn a_binding_that_starts_a_sequence_is_ambiguous() {
        let ok = vec![b("ctrl+x g", "git"), b("ctrl+x l", "log"), b("ctrl+t", "transcript")];
        assert!(ambiguous(&ok).is_empty());
        let bad = vec![b("ctrl+x", "cut"), b("ctrl+x g", "git"), b("ctrl+t", "transcript")];
        let m = ambiguous(&bad);
        assert_eq!(m.len(), 1);
        assert!(m[0].contains("`ctrl+x` (cut)") && m[0].contains("`ctrl+x g` (git)"), "{m:?}");
    }

    #[test]
    fn terminals_are_read_from_the_environment() {
        let env = |k: &str| match k {
            "TERM_PROGRAM" => Some("ghostty".to_string()),
            "TMUX" => Some("/tmp/tmux".to_string()),
            _ => None,
        };
        assert_eq!(terminal_id(&env), ["ghostty", "tmux"]);
    }
}
