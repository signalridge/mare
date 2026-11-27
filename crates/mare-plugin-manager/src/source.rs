//! Package sources: short names expand through
//! `mare.o.plugins.url_format` (lazy.nvim `git.url_format`); full addresses are recognized by git's
//! own rules (`transport.c` remote helpers, `connect.c` `url_is_local_not_ssh` for scp syntax);
//! only https, ssh and file transports are used (plain http and git:// behind `allow_insecure`);
//! checkouts are named `<host>/<path>` so the same path on two hosts never collides.

use std::path::PathBuf;

/// lazy.nvim `core/config.lua` `git.url_format`.
pub const DEFAULT_URL_FORMAT: &str = "https://github.com/%s.git";

/// A git transport mare may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Https,
    Ssh,
    File,
    /// plain `http://` (only with `allow_insecure`)
    Http,
    /// `git://` (only with `allow_insecure`)
    Git,
}

/// What a declared source string is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `owner/repo` (any depth: GitLab `group/sub/repo`), expanded through `url_format`
    Short,
    Url(Transport),
    /// `<transport>::<address>` (`ext::`, `fd::`, …): a remote helper — never allowed
    Helper(String),
    /// `scheme://…` with a scheme mare does not allow
    Unsupported(String),
}

fn is_scheme_char(first: bool, c: char) -> bool {
    c.is_ascii_alphabetic() || (!first && (c.is_ascii_digit() || c == '+' || c == '-' || c == '.'))
}

/// The scheme-like prefix of `s` (git `is_urlschemechar`) and what follows it.
fn scheme_prefix(s: &str) -> (&str, &str) {
    let end = s.char_indices().find(|&(i, c)| !is_scheme_char(i == 0, c)).map(|(i, _)| i).unwrap_or(s.len());
    s.split_at(end)
}

/// `C:\…` / `C:/…` (git `has_dos_drive_prefix`).
fn has_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'/' || b[2] == b'\\')
}

fn is_local_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with('\\') || has_drive_prefix(s)
}

/// scp syntax `[user@]host:path`: a colon before the first `/` that is not a drive letter
/// (git `url_is_local_not_ssh`, inverted). `[::1]:path` keeps its brackets.
fn scp_split(s: &str) -> Option<(&str, &str)> {
    if has_drive_prefix(s) {
        return None;
    }
    // a bracketed host may contain colons: look for the colon after the closing bracket
    let search_from = match (s.find('['), s.find(']')) {
        (Some(o), Some(c)) if o < c && !s[..o].contains('/') => c,
        _ => 0,
    };
    let colon = search_from + s[search_from..].find(':')?;
    if s.find('/').is_some_and(|slash| slash < colon) || colon == 0 {
        return None;
    }
    let auth = &s[..colon];
    let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    if auth.starts_with('-') || host.is_empty() || host.starts_with('-') {
        return None;
    }
    Some((auth, &s[colon + 1..]))
}

/// Classify a declared source by git's rules.
pub fn classify(s: &str) -> Source {
    let (scheme, rest) = scheme_prefix(s);
    if !scheme.is_empty() && rest.starts_with("::") {
        return Source::Helper(scheme.to_string());
    }
    if !scheme.is_empty() && rest.starts_with("://") {
        return match scheme.to_ascii_lowercase().as_str() {
            "https" => Source::Url(Transport::Https),
            "ssh" | "git+ssh" | "ssh+git" => Source::Url(Transport::Ssh),
            "file" => Source::Url(Transport::File),
            "http" => Source::Url(Transport::Http),
            "git" => Source::Url(Transport::Git),
            other => Source::Unsupported(other.to_string()),
        };
    }
    if is_local_path(s) {
        return Source::Url(Transport::File);
    }
    if scp_split(s).is_some() {
        return Source::Url(Transport::Ssh);
    }
    Source::Short
}

/// A short name through `url_format` (lazy.nvim `string.format(url_format, name)`); anything
/// else is already an address.
pub fn expand(name: &str, url_format: &str) -> String {
    match classify(name) {
        Source::Short => url_format.replacen("%s", name.trim_matches('/'), 1),
        _ => name.to_string(),
    }
}

/// Refuse transports outside the allowlist, with what to do about it.
pub fn check(url: &str, allow_insecure: bool) -> anyhow::Result<()> {
    match classify(url) {
        Source::Url(Transport::Https | Transport::Ssh | Transport::File) => {
            if url.contains("://") {
                let (host, _) = host_and_path(url);
                if host.starts_with('-') {
                    anyhow::bail!("{url}: host name must not start with `-`");
                }
            }
            Ok(())
        }
        Source::Url(t @ (Transport::Http | Transport::Git)) => {
            let (host, _) = host_and_path(url);
            if host.starts_with('-') {
                anyhow::bail!("{url}: host name must not start with `-`");
            }
            if allow_insecure {
                Ok(())
            } else {
                let what = if t == Transport::Http { "http://" } else { "git://" };
                anyhow::bail!(
                    "{url}: {what} is unencrypted; use https or ssh, or set mare.o.plugins.allow_insecure = true"
                )
            }
        }
        Source::Helper(h) => anyhow::bail!(
            "{url}: the git remote helper `{h}::` can run arbitrary commands and is never allowed (https, ssh, file only)"
        ),
        Source::Unsupported(s) => anyhow::bail!("{url}: transport `{s}://` is not allowed (https, ssh, file only)"),
        Source::Short => anyhow::bail!("{url}: not a git address"),
    }
}

/// `-c` flags every git call carries: everything off, then the allowed transports back on.
/// `protocol.file.allow=user` blocks `.gitmodules` `file://` submodule clones (CVE-2022-39253).
pub fn protocol_flags(allow_insecure: bool) -> Vec<&'static str> {
    let mut v = vec![
        "-c",
        "protocol.allow=never",
        "-c",
        "protocol.https.allow=always",
        "-c",
        "protocol.ssh.allow=always",
        "-c",
        "protocol.file.allow=user",
    ];
    if allow_insecure {
        v.extend(["-c", "protocol.http.allow=always", "-c", "protocol.git.allow=always"]);
    }
    v
}

/// Split `scheme://authority[tail]` when present.
fn split_scheme_authority(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || !scheme.chars().enumerate().all(|(i, c)| is_scheme_char(i == 0, c)) {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some((scheme, &rest[..end], &rest[end..]))
}

/// Whether `url` embeds credentials in URL userinfo (`https://user:tok@host/...` or `ssh://user:pass@host/...`).
pub fn has_url_credentials(url: &str) -> bool {
    let Some((scheme, authority, _)) = split_scheme_authority(url) else {
        return false;
    };
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    if matches!(scheme.to_ascii_lowercase().as_str(), "ssh" | "git+ssh" | "ssh+git") {
        userinfo.contains(':')
    } else {
        true
    }
}

/// Strip URL userinfo credentials before persisting to `mare-lock.json` (preserving SSH login username without password).
pub fn strip_url_credentials(url: &str) -> String {
    let Some((scheme, authority, tail)) = split_scheme_authority(url) else {
        return url.to_string();
    };
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    if matches!(scheme.to_ascii_lowercase().as_str(), "ssh" | "git+ssh" | "ssh+git") {
        if let Some((user, _)) = userinfo.split_once(':') {
            if user.is_empty() { format!("{scheme}://{host}{tail}") } else { format!("{scheme}://{user}@{host}{tail}") }
        } else {
            url.to_string()
        }
    } else {
        format!("{scheme}://{host}{tail}")
    }
}

/// One path component made of `[alnum - _ .]` only, never empty, `.` or `..`: joined under the
/// packages root it can't climb out or become absolute (`C:` turns into `C_`).
pub(crate) fn safe_segment(seg: &str) -> String {
    let s: String =
        seg.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
    if s.is_empty() || s.chars().all(|c| c == '.') { "_".into() } else { s }
}

/// `p` is strictly inside the packages root, with no `..`, root or drive components after it.
pub fn under_packages(p: &std::path::Path) -> bool {
    use std::path::Component;
    let root = mare_shared::paths::packages_dir();
    match p.strip_prefix(&root) {
        Ok(rel) => rel.components().next().is_some() && rel.components().all(|c| matches!(c, Component::Normal(_))),
        Err(_) => false,
    }
}

/// `(host, path)` of an address: `local` for file:// and local paths, `other` for what is never
/// cloned anyway.
fn host_and_path(url: &str) -> (String, String) {
    match classify(url) {
        Source::Url(Transport::File) => {
            let p = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
            // file://host/path: keep the authority as part of the path (it is rarely set)
            ("local".into(), p.to_string())
        }
        Source::Url(_) if url.contains("://") => {
            let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
            let (auth, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
            (host.to_string(), path.to_string())
        }
        Source::Url(_) => match scp_split(url) {
            Some((auth, path)) => {
                let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
                (host.to_string(), path.to_string())
            }
            None => ("other".into(), url.to_string()),
        },
        _ => ("other".into(), url.to_string()),
    }
}

/// The address has a `..` path segment: its checkout directory name would climb out, so it is
/// refused instead of silently renamed.
pub fn climbs_out(url: &str) -> bool {
    let (_, path) = host_and_path(url);
    path.split(['/', '\\']).any(|s| s == "..")
}

/// The directory a checkout of `url` lives in: `<data>/packages/<host>/<path>` (`.git` dropped).
pub fn checkout_dir(url: &str) -> PathBuf {
    let (host, path) = host_and_path(url);
    let mut d = mare_shared::paths::packages_dir().join(safe_segment(&host.trim_matches(['[', ']']).to_lowercase()));
    let segs: Vec<&str> =
        path.split(['/', '\\']).filter(|s| !s.is_empty() && *s != "." && *s != ".." && *s != "~").collect();
    for (i, s) in segs.iter().enumerate() {
        let s = if i + 1 == segs.len() { s.strip_suffix(".git").unwrap_or(s) } else { s };
        d = d.join(safe_segment(s));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_recognized_by_gits_rules() {
        use Transport::*;
        for (s, want) in [
            ("someone/repo", Source::Short),
            ("group/sub/repo", Source::Short),
            ("https://gitlab.com/g/s/r.git", Source::Url(Https)),
            ("ssh://git@host:2222/g/r.git", Source::Url(Ssh)),
            ("git+ssh://host/r", Source::Url(Ssh)),
            ("file:///srv/git/r.git", Source::Url(File)),
            ("/srv/git/r.git", Source::Url(File)),
            ("C:\\git\\r.git", Source::Url(File)),
            ("D:/git/r", Source::Url(File)),
            ("git@github.com:a/r.git", Source::Url(Ssh)),
            ("corp@git.example.com:g/sub/r.git", Source::Url(Ssh)),
            ("git.example.com:r.git", Source::Url(Ssh)),
            ("[::1]:g/r.git", Source::Url(Ssh)),
            ("http://host/r", Source::Url(Http)),
            ("git://host/r", Source::Url(Git)),
            ("ext::sh -c touch% /tmp/pwned", Source::Helper("ext".into())),
            ("fd::17", Source::Helper("fd".into())),
            ("ftp://host/r", Source::Unsupported("ftp".into())),
        ] {
            assert_eq!(classify(s), want, "{s}");
        }
        // a colon after the first slash is not scp (`owner/repo:subdir` is Yazi's subdir syntax)
        assert_eq!(classify("owner/repo:sub"), Source::Short);
    }

    #[test]
    fn transports_outside_the_allowlist_are_refused() {
        assert!(check("https://h/r", false).is_ok());
        assert!(check("git@h:r", false).is_ok());
        assert!(check("/srv/r.git", false).is_ok());
        let e = check("ext::sh -c id", true).unwrap_err().to_string();
        assert!(e.contains("never allowed"), "{e}");
        assert!(check("fd::3", true).is_err());
        assert!(check("http://h/r", false).unwrap_err().to_string().contains("allow_insecure"));
        assert!(check("git://h/r", false).is_err());
        assert!(check("http://h/r", true).is_ok() && check("git://h/r", true).is_ok());
        assert!(check("ftp://h/r", true).is_err());
    }

    #[test]
    fn checkouts_are_named_by_host_and_path() {
        let root = mare_shared::paths::packages_dir();
        let gh = checkout_dir("https://github.com/a/x.git");
        let gl = checkout_dir("https://gitlab.com/a/x.git");
        assert_eq!(gh, root.join("github.com/a/x"));
        assert_eq!(gl, root.join("gitlab.com/a/x"));
        assert_eq!(checkout_dir("git@gitlab.com:a/x.git"), gl, "ssh and https of one repo share a checkout");
        assert_eq!(checkout_dir("ssh://git@GitLab.com:22/g/sub/r.git"), root.join("gitlab.com_22/g/sub/r"));
        assert_eq!(checkout_dir("https://user:tok@h.example/../r.git"), root.join("h.example/r"));
        assert_eq!(checkout_dir("file:///srv/git/r.git"), root.join("local/srv/git/r"));
        assert_eq!(checkout_dir("/srv/git/r.git"), root.join("local/srv/git/r"));
    }

    #[test]
    fn no_source_or_name_leads_outside_the_packages_root() {
        for url in [
            "https://../../etc/x.git",
            "https://h/../../../etc/passwd",
            "https://./.git",
            "file:///../../x",
            "..:../../x",
            "git@..:..",
            "C:\\..\\..\\x",
            "https://h/a/..../b",
            "https://h//",
        ] {
            let d = checkout_dir(url);
            assert!(under_packages(&d), "{url} → {}", d.display());
        }
    }

    #[test]
    fn check_rejects_flag_like_host_and_strips_ssh_credentials() {
        // Host starting with '-' could be parsed as a git/ssh option.
        let err = check("https://-evil.example/a/b.git", false).unwrap_err().to_string();
        assert!(err.contains("must not start with `-`"), "{err}");

        // SSH credentials: password is flagged and stripped while preserving user.
        assert!(has_url_credentials("ssh://alice:secret@host/repo.git"));
        assert!(!has_url_credentials("ssh://alice@host/repo.git"));
        assert_eq!(strip_url_credentials("ssh://alice:secret@host/repo.git"), "ssh://alice@host/repo.git");
        assert_eq!(strip_url_credentials("ssh://:secret@host/repo.git"), "ssh://host/repo.git");
        assert_eq!(strip_url_credentials("ssh://alice@host/repo.git"), "ssh://alice@host/repo.git");

        // Windows drive prefix is not treated as SCP syntax; invalid scheme returns None.
        assert!(scp_split("C:/foo/bar").is_none());
        assert_eq!(classify("://no-scheme"), Source::Short);
        assert!(!under_packages(std::path::Path::new("/definitely/not/under/packages")));
    }
}
