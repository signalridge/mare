//! A path as tools show it: relative to the working directory when it lies below it
//! (`mare.fs.relative`, behind `mare_lib.rel`). What the model reads in tool results and what
//! the transcript shows, so a file under the cwd reads `src/a.rs`, not its absolute path.

use std::path::Path;

/// `p` without the `cwd/` in front when it lies below the cwd, else `None` (shown as given).
///
/// Unix: a plain prefix test on the string, as the Lua helper always did. Windows: `\` and `/`
/// both separate, drive letters and names compare case-insensitively, and either side may carry
/// the verbatim `\\?\` prefix (`cwd.join("a.txt")` is `C:\w\a.txt` while the cwd ends in no
/// `/`); when that spelling still differs, the canonical forms of both are compared (8.3 short
/// names such as `YIXIAN~1`, links) by stripping path components.
pub fn relative_to(p: &str, cwd: &str) -> Option<String> {
    if !cfg!(windows) {
        return p.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')).map(str::to_owned);
    }
    relative_folded(p, cwd).or_else(|| {
        if !Path::new(p).is_absolute() {
            return None;
        }
        let (cp, cc) = (crate::realpath::resolve(Path::new(p)).ok()?, crate::realpath::resolve(Path::new(cwd)).ok()?);
        relative_folded(&cp.to_string_lossy(), &cc.to_string_lossy())
    })
}

/// The Windows comparison on the spelling: separators unified, ASCII case folded, `\\?\` (and
/// `\\?\UNC\`) dropped. Folding keeps every byte after the prefix in place, so the remainder is
/// cut from `p` itself and keeps the separators the caller wrote.
fn relative_folded(p: &str, cwd: &str) -> Option<String> {
    let (fp, fc) = (fold(p), fold(cwd));
    let base = format!("{}/", fc.trim_end_matches('/'));
    if !fp.starts_with(&base) {
        return None;
    }
    let keep = fp.len() - base.len();
    Some(p[p.len() - keep..].to_owned())
}

fn fold(s: &str) -> String {
    let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(s).to_owned()
    };
    s.chars().map(|c| if c == '\\' { '/' } else { c.to_ascii_lowercase() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_spellings_of_the_cwd_fold_to_one() {
        let cwd = r"C:\Users\me\AppData\Local\Temp\.tmpFgxT6r";
        for (p, want) in [
            (r"C:\Users\me\AppData\Local\Temp\.tmpFgxT6r\a.txt", Some("a.txt")),
            (r"c:/users/ME/appdata/local/temp/.TMPfgxt6r/sub/b.txt", Some("sub/b.txt")),
            (r"\\?\C:\Users\me\AppData\Local\Temp\.tmpFgxT6r\sub\c.txt", Some(r"sub\c.txt")),
            (r"C:\Users\me\AppData\Local\Temp\.tmpFgxT6r", None),
            (r"C:\Users\me\AppData\Local\Temp\.tmpFgxT6rX\a.txt", None),
            (r"D:\other\a.txt", None),
            ("a.txt", None),
        ] {
            assert_eq!(relative_folded(p, cwd).as_deref(), want, "{p}");
        }
        assert_eq!(relative_folded(r"C:\a.txt", r"C:\").as_deref(), Some("a.txt"));
        assert_eq!(relative_folded(r"\\?\UNC\srv\share\w\x.rs", r"\\srv\share\w").as_deref(), Some("x.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_is_the_plain_string_prefix() {
        assert_eq!(relative_to("/w/src/a.rs", "/w").as_deref(), Some("src/a.rs"));
        assert_eq!(relative_to("/w/", "/w").as_deref(), Some(""));
        assert_eq!(relative_to("/w", "/w"), None);
        assert_eq!(relative_to("/wx/a", "/w"), None);
        assert_eq!(relative_to("/W/a", "/w"), None);
        assert_eq!(relative_to("a.rs", "/w"), None);
    }
}
