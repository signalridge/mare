//! Telling a sandbox denial from an ordinary failure in a command's output.

use std::path::Path;

use super::{Backend, Denial, Network, Policy};
use crate::realpath::expand;

/// Classify a failed sandboxed command. Proxy denials are authoritative. Otherwise:
/// - EPERM ("Operation not permitted", Seatbelt) / EROFS ("Read-only file system", bwrap) /
///   EACCES (Landlock) naming a path the policy forbids → `write`/`read` (an error inside the
///   allowed roots is the program's own problem and is not reported);
/// - with a proxied network the proxy port is the only reachable endpoint, so a direct
///   connection failure can only be the sandbox → `network` (host parsed when present).
pub fn classify(
    policy: &Policy,
    cwd: &Path,
    output: &str,
    proxy_denied: Option<String>,
    backend: Backend,
) -> Option<Denial> {
    if let Some(host) = proxy_denied {
        return Some(Denial { kind: "network", path: None, host: Some(host) });
    }
    let proxied = matches!(policy.network, Network::Proxy(_));
    for line in output.lines() {
        // bwrap under restricted reads: a masked path is missing (crate::read_mask)
        if let Some(d) = hidden_read(policy, cwd, line, backend) {
            return Some(d);
        }
        let l = line.to_ascii_lowercase();
        let fs_denial = l.contains("operation not permitted")
            || l.contains("read-only file system")
            || (matches!(backend, Backend::Landlock { .. }) && l.contains("permission denied"))
            // Windows: Win32 "Access is denied." (cmd, native tools), .NET/PowerShell
            // "Access to the path '…' is denied", msys/Git Bash "Permission denied"
            || (matches!(backend, Backend::WinAcl | Backend::WinUser)
                && (l.contains("access is denied")
                    || (l.contains("access to the path") && l.contains("denied"))
                    || l.contains("unauthorizedaccess")
                    || l.contains("permission denied")));
        if fs_denial {
            for cand in path_candidates(line) {
                let p = expand(&cand, cwd);
                if !policy.can_write(&p) {
                    return Some(Denial { kind: "write", path: Some(p.to_string_lossy().into_owned()), host: None });
                }
                if !policy.can_read(&p) {
                    return Some(Denial { kind: "read", path: Some(p.to_string_lossy().into_owned()), host: None });
                }
            }
            if l.contains("read-only file system") {
                return Some(Denial { kind: "write", path: None, host: None });
            }
        }
        const NET: &[&str] = &[
            "failed to connect",
            "couldn't connect",
            "could not connect",
            "connection refused",
            "network is unreachable",
            "no route to host",
            "connect: operation not permitted",
        ];
        if proxied && NET.iter().any(|n| l.contains(n)) {
            return Some(Denial { kind: "network", path: None, host: connect_host(line) });
        }
    }
    None
}

/// `Failed to connect to example.com port 443` / `connect to 1.2.3.4:80` → host
fn connect_host(line: &str) -> Option<String> {
    let l = line.to_ascii_lowercase();
    let i = l.find("connect to ")? + "connect to ".len();
    let tok = line[i..].split_whitespace().next()?;
    let tok = tok.trim_matches(|c: char| c == '\'' || c == '"' || c == ',' || c == ':');
    let host = match tok.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => h,
        _ => tok,
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Path-looking tokens in an error line: quoted strings and `word:`-prefixed segments such as
/// `touch: /etc/x: Operation not permitted` or `PermissionError: [Errno 1] ...: '/etc/x'`.
pub(crate) fn path_candidates(line: &str) -> Vec<String> {
    let mut out = vec![];
    for q in ['\'', '"', '`'] {
        let parts: Vec<&str> = line.split(q).collect();
        for (i, p) in parts.iter().enumerate() {
            if i % 2 == 1 && (looks_like_path(p) || looks_like_quoted_path(p)) {
                out.push((*p).to_string());
            }
        }
    }
    let segs: Vec<&str> = line.split(": ").collect();
    for (i, raw_seg) in segs.iter().enumerate() {
        let seg = strip_action_prefix(raw_seg.trim()).trim_matches(|c| c == '\'' || c == '"' || c == '`');
        let is_argv0 = i == 0 && segs.len() > 1 && !segs.get(1).is_some_and(|next| is_fs_error_phrase(next));
        if !is_argv0
            && (looks_like_path(seg)
                || (i > 0
                    && is_relative_name(seg)
                    && (segs.get(i.wrapping_sub(1)).is_some_and(|prev| is_fs_error_phrase(prev))
                        || segs.get(i + 1).is_some_and(|next| is_fs_error_phrase(next)))))
        {
            out.push(seg.to_string());
        }
    }
    for tok in line.split_whitespace() {
        let tok = tok.trim_matches(|c: char| c == '\'' || c == '"' || c == ':' || c == ',' || c == '`');
        if tok.starts_with('/') || tok.starts_with("~/") || is_drive_path(tok) {
            out.push(tok.to_string());
        }
    }
    out
}

fn strip_action_prefix(s: &str) -> &str {
    for prefix in ["cannot create directory ", "cannot create ", "cannot touch ", "cannot remove ", "cannot open "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest;
        }
    }
    s
}

fn is_fs_error_phrase(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("read-only file system")
        || l.contains("operation not permitted")
        || l.contains("permission denied")
        || l.contains("access is denied")
}

fn looks_like_quoted_path(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('\n')
        && (s.starts_with('/')
            || s.starts_with("~/")
            || s.starts_with("./")
            || s.contains('/')
            || is_drive_path(s)
            || is_relative_name(s))
}

fn is_relative_name(s: &str) -> bool {
    !s.is_empty() && !s.contains(' ') && !s.contains(':') && !is_fs_error_phrase(s)
}

fn looks_like_path(s: &str) -> bool {
    !s.is_empty()
        && !s.contains(' ')
        && (s.starts_with('/') || s.starts_with("~/") || s.starts_with("./") || s.contains('/') || is_drive_path(s))
}

/// `C:\x` / `C:/x`
fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// `classify` for bwrap: a masked path reads as missing (`ENOENT`). Named in an error and
/// outside the read set → a `read` denial.
pub fn hidden_read(policy: &Policy, cwd: &Path, line: &str, backend: Backend) -> Option<Denial> {
    if backend != Backend::Bwrap || !policy.restrict_read || !line.to_ascii_lowercase().contains("no such file") {
        return None;
    }
    crate::ossandbox::path_candidates(line).into_iter().find_map(|c| {
        let p = crate::ossandbox::expand(&c, cwd);
        (!policy.can_read(&p)).then(|| Denial {
            kind: "read",
            path: Some(p.to_string_lossy().into_owned()),
            host: None,
        })
    })
}
