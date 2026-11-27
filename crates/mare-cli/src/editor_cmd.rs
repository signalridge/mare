//! The external editor (Ctrl+G, the startup config error dialog): which program, and how to
//! open a file at a line with it.

use std::path::Path;
use std::process::Command;

use mare_host::Host;

/// `mare.o.editor`, then $VISUAL / $EDITOR, then vi.
pub(crate) fn program(host: &Host) -> String {
    let configured = host
        .lua()
        .load("local o = mare.o; return o and o.editor")
        .eval::<Option<String>>()
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty());
    configured
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".into())
}

/// Split an editor command string into `(program, args)` using shell lexical rules.
/// Returns `Err` when the command has unclosed quotes/escapes or is empty.
pub(crate) fn split_program(program: &str) -> Result<(String, Vec<String>), String> {
    #[cfg(windows)]
    let parts = split_windows_program(program);
    #[cfg(not(windows))]
    let parts = shlex::split(program);
    let parts = parts.ok_or_else(|| format!("Invalid editor command `{program}` (unclosed quote or escape)"))?;
    let mut it = parts.into_iter();
    let exe = it.next().ok_or_else(|| "No editor command configured (set $VISUAL or $EDITOR)".to_string())?;
    Ok((exe, it.collect()))
}

#[cfg(any(windows, test))]
fn split_windows_program(program: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = program.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some('"') if c == '\\' && chars.peek() == Some(&'"') => {
                current.push(chars.next().unwrap());
            }
            Some(_) => {
                current.push(c);
            }
            None if c.is_whitespace() => {
                if in_token {
                    parts.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_token = true;
            }
            None => {
                current.push(c);
                in_token = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_token {
        parts.push(current);
    }
    Some(parts)
}

/// The command that opens `path` in `program` (words split with shell lexical rules, so
/// `"/path with spaces/code" --wait` works), at `line` when the editor has a known way to say it:
/// `+N file` (vi, Vim, Neovim, nano, Emacs, micro, Kakoune, joe, ne), `-g file:N` (VS Code and its
/// forks), `file:N` (Sublime Text, Zed, Helix). Any other editor gets just the file.
pub(crate) fn command(program: &str, path: &Path, line: Option<u32>) -> Option<Command> {
    let (exe, args) = split_program(program).ok()?;
    let mut cmd = Command::new(&exe);
    cmd.args(args);
    let name = Path::new(&exe).file_stem().and_then(|s| s.to_str()).unwrap_or(&exe).to_ascii_lowercase();
    let file = path.to_string_lossy();
    match (line, name.as_str()) {
        (
            Some(n),
            "vi" | "vim" | "nvim" | "gvim" | "mvim" | "nano" | "emacs" | "emacsclient" | "micro" | "kak" | "joe" | "ne",
        ) => {
            cmd.arg(format!("+{n}")).arg(path);
        }
        (Some(n), "code" | "code-insiders" | "codium" | "cursor" | "windsurf") => {
            cmd.arg("-g").arg(format!("{file}:{n}"));
        }
        (Some(n), "subl" | "zed" | "hx" | "helix") => {
            cmd.arg(format!("{file}:{n}"));
        }
        _ => {
            cmd.arg(path);
        }
    }
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(program: &str, line: Option<u32>) -> Vec<String> {
        let cmd = command(program, Path::new("/c/init.lua"), line).unwrap();
        std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_line_goes_the_way_each_editor_takes_it() {
        assert_eq!(args("nvim", Some(3)), ["nvim", "+3", "/c/init.lua"]);
        assert_eq!(args("/usr/bin/vim -u NONE", Some(3)), ["/usr/bin/vim", "-u", "NONE", "+3", "/c/init.lua"]);
        assert_eq!(args("code --wait", Some(3)), ["code", "--wait", "-g", "/c/init.lua:3"]);
        assert_eq!(args("hx", Some(3)), ["hx", "/c/init.lua:3"]);
        assert_eq!(args("my-editor", Some(3)), ["my-editor", "/c/init.lua"]);
        assert_eq!(args("nvim", None), ["nvim", "/c/init.lua"]);
        assert_eq!(args(r#""/My Apps/code" --wait"#, Some(3)), ["/My Apps/code", "--wait", "-g", "/c/init.lua:3"]);
        assert_eq!(
            args("'/path with spaces/nvim' -u NONE", Some(5)),
            ["/path with spaces/nvim", "-u", "NONE", "+5", "/c/init.lua"]
        );
        assert!(command("  ", Path::new("x"), None).is_none());
        assert!(command("nvim \"unclosed", Path::new("x"), None).is_none());
        assert!(split_program("nvim \"unclosed").is_err());
    }

    #[test]
    fn windows_program_splitting_preserves_backslashes() {
        assert_eq!(
            split_windows_program(r"C:\Users\alice\AppData\Local\Temp\fix-config.cmd").unwrap(),
            [r"C:\Users\alice\AppData\Local\Temp\fix-config.cmd"]
        );
        assert_eq!(
            split_windows_program(r#""C:\Program Files\VS Code\code.exe" --wait"#).unwrap(),
            [r"C:\Program Files\VS Code\code.exe", "--wait"]
        );
        assert!(split_windows_program(r#""C:\Program Files\unclosed"#).is_none());
    }
}
