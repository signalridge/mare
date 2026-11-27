//! Terminal compatibility on Windows ( and
//! terminal-setup.md): detect the legacy console host and whether Windows Terminal forwards
//! `Shift+Enter` (newline) and `Alt+Enter` (queue a follow-up; Windows Terminal binds it to
//! fullscreen by default). Used by `mare doctor` and as a one-time hint on first launch.
//! The checks are pure functions so they are tested on every platform.

use std::path::{Path, PathBuf};

pub use mare_tui::termdetect::{SETUP, TermKind, detect, forwarded_keys, settings_candidates};

pub fn check(var: &dyn Fn(&str) -> Option<String>) -> Vec<(bool, &'static str, String)> {
    let mut out = vec![];
    match detect(var) {
        TermKind::LegacyConhost => out.push((
            false,
            "terminal",
            "legacy console host: input may show ';0;rgb:' fragments and redraw on every key — \
             use Windows Terminal (winget install Microsoft.WindowsTerminal)"
                .into(),
        )),
        TermKind::OtherModern => out.push((true, "terminal", "modern terminal".into())),
        TermKind::WindowsTerminal => {
            out.push((true, "terminal", "Windows Terminal".into()));
            let settings = var("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|d| settings_candidates(&d))
                .unwrap_or_default()
                .into_iter()
                .find_map(|p| std::fs::read_to_string(p).ok());
            match settings.as_deref().and_then(forwarded_keys) {
                Some((true, true)) => out.push((true, "terminal keys", "Shift+Enter and Alt+Enter forwarded".into())),
                Some((s, a)) => {
                    let missing: Vec<&str> =
                        [(!s).then_some("Shift+Enter"), (!a).then_some("Alt+Enter")].into_iter().flatten().collect();
                    out.push((false, "terminal keys", format!("{} not forwarded.\n{SETUP}", missing.join(" and "))))
                }
                None => out.push((false, "terminal keys", format!("settings.json not found or unreadable.\n{SETUP}"))),
            }
        }
    }
    out
}

/// The first launch on Windows with a terminal problem shows one hint; never again after that.
/// It is one short line; `mare
/// doctor` has the details and the configuration steps.
pub fn first_launch_hint(state_dir: &Path) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let marker = state_dir.join("terminal-hint-shown");
    if marker.exists() {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok();
    let hint = short_hint(&check(&var));
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(&marker, "");
    hint
}

/// The one-line form of [`check`]'s problems: what is wrong, and where to read more.
fn short_hint(checks: &[(bool, &'static str, String)]) -> Option<String> {
    let problems: Vec<&str> = checks
        .iter()
        .filter(|(ok, ..)| !ok)
        .map(|(_, name, detail)| match *name {
            "terminal" => "legacy console: use Windows Terminal",
            _ if detail.starts_with("settings.json") => "Shift+Enter / Alt+Enter may not reach mare",
            _ => "Shift+Enter / Alt+Enter not forwarded",
        })
        .collect();
    (!problems.is_empty()).then(|| format!("{} (mare doctor)", problems.join(" · ")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_first_launch_hint_is_one_short_line() {
        let checks = vec![
            (false, "terminal", "legacy console host: input may show ';0;rgb:' fragments".to_string()),
            (true, "terminal keys", "Shift+Enter and Alt+Enter forwarded".to_string()),
        ];
        assert_eq!(super::short_hint(&checks).as_deref(), Some("legacy console: use Windows Terminal (mare doctor)"));
        let keys = vec![(false, "terminal keys", "Alt+Enter not forwarded.\nsetup".to_string())];
        assert_eq!(super::short_hint(&keys).as_deref(), Some("Shift+Enter / Alt+Enter not forwarded (mare doctor)"));
        assert_eq!(super::short_hint(&[(true, "terminal", "modern terminal".into())]), None);
    }
}
