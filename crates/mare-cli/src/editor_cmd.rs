//! The external editor (Ctrl+G, the startup config error dialog): which program, and how to
//! open a file at a line with it.

use std::path::Path;
use std::process::Command;

use mare_host::Host;

/// `mare.o.editor`, then `$VISUAL` / `$EDITOR` (skipping blank values), then a platform default.
pub(crate) fn program(host: &Host) -> String {
    let configured = host.lua().load("local o = mare.o; return o and o.editor").eval::<Option<String>>().ok().flatten();
    resolve_program(configured, &|k| std::env::var(k).ok(), &command_on_path, cfg!(windows))
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn command_on_path_in(name: &str, path_env: Option<std::ffi::OsString>, cwd: &Path) -> bool {
    mare_binding::proc::which_path(name, path_env, cwd).is_some()
}

fn command_on_path(name: &str) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    command_on_path_in(name, std::env::var_os("PATH"), &cwd)
}

fn resolve_program(
    configured: Option<String>,
    var: &dyn Fn(&str) -> Option<String>,
    on_path: &dyn Fn(&str) -> bool,
    is_windows: bool,
) -> String {
    non_blank(configured)
        .or_else(|| non_blank(var("VISUAL")))
        .or_else(|| non_blank(var("EDITOR")))
        .unwrap_or_else(|| default_program(on_path, is_windows))
}

fn default_program(on_path: &dyn Fn(&str) -> bool, is_windows: bool) -> String {
    if is_windows {
        for cand in ["notepad", "nano", "vim", "nvim"] {
            if on_path(cand) {
                return cand.into();
            }
        }
        "notepad".into()
    } else {
        for cand in ["vi", "nano", "vim"] {
            if on_path(cand) {
                return cand.into();
            }
        }
        "vi".into()
    }
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

    #[test]
    fn resolve_program_skips_blank_overrides_and_uses_platform_defaults() {
        let none = |_: &str| None;
        let no_bins = |_: &str| false;

        // Blank configured/env values fall through to the next non-blank choice.
        let env = |k: &str| match k {
            "VISUAL" => Some("   \t ".into()),
            "EDITOR" => Some("code --wait".into()),
            _ => None,
        };
        assert_eq!(resolve_program(Some("  ".into()), &env, &no_bins, true), "code --wait");
        assert_eq!(resolve_program(Some("nvim -u NONE".into()), &env, &no_bins, true), "nvim -u NONE");

        // All blank on Windows falls back to an available Windows default (never `vi`).
        let blank_env = |k: &str| matches!(k, "VISUAL" | "EDITOR").then(|| "  ".into());
        assert_eq!(resolve_program(None, &blank_env, &no_bins, true), "notepad");
        assert_eq!(resolve_program(None, &none, &|c| c == "nano", true), "nano");

        // Unix keeps `vi` when available and falls back to `nano` when `vi` is absent.
        assert_eq!(resolve_program(None, &none, &|c| c == "vi", false), "vi");
        assert_eq!(resolve_program(None, &none, &|c| c == "nano", false), "nano");
        assert_eq!(resolve_program(None, &none, &no_bins, false), "vi");
    }

    #[cfg(unix)]
    #[test]
    fn command_on_path_ignores_non_executable_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir1 = root.path().join("dir1");
        let dir2 = root.path().join("dir2");
        std::fs::create_dir_all(&dir1).unwrap();
        std::fs::create_dir_all(&dir2).unwrap();

        // A non-executable `vi` in an earlier PATH entry must not match or shadow executable `nano`.
        let fake_vi = dir1.join("vi");
        std::fs::write(&fake_vi, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake_vi, std::fs::Permissions::from_mode(0o644)).unwrap();

        let fake_nano = dir2.join("nano");
        std::fs::write(&fake_nano, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake_nano, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = std::env::join_paths([&dir1, &dir2]).unwrap();
        assert!(!command_on_path_in("vi", Some(path_env.clone()), root.path()));
        assert!(command_on_path_in("nano", Some(path_env.clone()), root.path()));
        let chosen = default_program(&|cand| command_on_path_in(cand, Some(path_env.clone()), root.path()), false);
        assert_eq!(chosen, "nano");
    }
}
