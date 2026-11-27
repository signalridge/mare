//! Pre-scan of the raw arguments before clap parses: the working directory, `--clean` /
//! `--clean`, `--trust` / `--no-trust` and `--version`. Extension flag discovery evaluates
//! the chosen project's configuration with these, so they must be read exactly as clap will
//! read them. Which options take a value comes from the clap definition itself, so attached
//! short values (`-C/dir`), short clusters (`-pC/dir`) and values of other options that merely
//! look like these flags (`--append-system-prompt=--trust`) are told apart the same way.
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use clap::CommandFactory;

pub(crate) struct PreScan {
    pub cwd: Option<PathBuf>,
    pub no_plugins: bool,
    pub cli_trust: Option<bool>,
    pub skip_discovery: bool,
    pub json: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Takes {
    Nothing,
    Value,
    /// `num_args = 0..=1`: the next argument is its value only when it doesn't look like a flag
    /// (or is a negative number and the option allows those).
    Optional {
        negative_numbers: bool,
    },
}

#[derive(Default)]
struct Grammar {
    short: HashMap<char, Takes>,
    long: HashMap<String, Takes>,
    subcommands: HashSet<String>,
}

fn takes(arg: &clap::Arg) -> Takes {
    if !arg.get_action().takes_values() {
        return Takes::Nothing;
    }
    match arg.get_num_args() {
        Some(range) if range.min_values() == 0 => {
            Takes::Optional { negative_numbers: arg.is_allow_negative_numbers_set() }
        }
        _ => Takes::Value,
    }
}

fn collect(cmd: &clap::Command, g: &mut Grammar) {
    for arg in cmd.get_arguments() {
        let t = takes(arg);
        if let Some(c) = arg.get_short() {
            g.short.entry(c).or_insert(t);
        }
        for c in arg.get_short_and_visible_aliases().unwrap_or_default() {
            g.short.entry(c).or_insert(t);
        }
        for l in arg.get_long_and_visible_aliases().unwrap_or_default() {
            g.long.entry(l.to_string()).or_insert(t);
        }
    }
    for sub in cmd.get_subcommands() {
        collect(sub, g);
    }
}

fn grammar() -> Grammar {
    let mut g = Grammar::default();
    let cmd = super::Cli::command();
    for sub in cmd.get_subcommands() {
        g.subcommands.insert(sub.get_name().to_string());
        for alias in sub.get_all_aliases() {
            g.subcommands.insert(alias.to_string());
        }
    }
    g.subcommands.insert("help".to_string());
    collect(&cmd, &mut g);
    g
}

fn is_negative_number(s: &OsStr) -> bool {
    s.to_str().is_some_and(|s| s.strip_prefix('-').is_some_and(|n| !n.is_empty() && n.parse::<f64>().is_ok()))
}

/// Whether clap would take `next` as the value of an option that `takes` one.
fn value_follows(t: Takes, next: Option<&OsStr>) -> bool {
    match (t, next) {
        (_, None) | (Takes::Nothing, _) => false,
        (Takes::Value, Some(n)) => !n.as_encoded_bytes().starts_with(b"-") || n == "-",
        (Takes::Optional { negative_numbers }, Some(n)) => {
            !n.as_encoded_bytes().starts_with(b"-") || n == "-" || (negative_numbers && is_negative_number(n))
        }
    }
}

#[cfg(unix)]
fn split_long(s: &OsStr) -> Option<(Option<&str>, Option<OsString>)> {
    use std::os::unix::ffi::OsStrExt;
    let long = s.as_bytes().strip_prefix(b"--")?;
    let (name_bytes, attached) = match long.iter().position(|&b| b == b'=') {
        Some(pos) => (&long[..pos], Some(OsStr::from_bytes(&long[pos + 1..]).to_os_string())),
        None => (long, None),
    };
    Some((std::str::from_utf8(name_bytes).ok(), attached))
}

#[cfg(not(unix))]
fn split_long(s: &OsStr) -> Option<(Option<&str>, Option<OsString>)> {
    let long = s.to_str()?.strip_prefix("--")?;
    let (name, attached) = match long.split_once('=') {
        Some((n, v)) => (Some(n), Some(OsString::from(v))),
        None => (Some(long), None),
    };
    Some((name, attached))
}

#[cfg(unix)]
fn scan_short_cluster(s: &OsStr, g: &Grammar, next: Option<&OsStr>, out: &mut PreScan) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(cluster) = s.as_bytes().strip_prefix(b"-").filter(|c| !c.is_empty()) else {
        return false;
    };
    let Some(chunk) = cluster.utf8_chunks().next() else {
        return false;
    };
    for (at, c) in chunk.valid().char_indices() {
        let t = g.short.get(&c).copied().unwrap_or(Takes::Nothing);
        if c == 'V' {
            out.skip_discovery = true;
        }
        if t == Takes::Nothing {
            continue;
        }
        // the rest of the cluster is this option's value (clap drops a leading '=')
        let rest = &cluster[at + c.len_utf8()..];
        let (value, consumed_next) = if !rest.is_empty() {
            let val = rest.strip_prefix(b"=").unwrap_or(rest);
            (Some(OsStr::from_bytes(val).to_os_string()), false)
        } else if value_follows(t, next) {
            (next.map(OsStr::to_os_string), true)
        } else {
            (None, false)
        };
        if c == 'C'
            && let Some(v) = value
        {
            out.cwd = Some(PathBuf::from(v));
        }
        return consumed_next;
    }
    false
}

#[cfg(not(unix))]
fn scan_short_cluster(s: &OsStr, g: &Grammar, next: Option<&OsStr>, out: &mut PreScan) -> bool {
    let Some(s) = s.to_str() else {
        return false;
    };
    let Some(cluster) = s.strip_prefix('-').filter(|c| !c.is_empty()) else {
        return false;
    };
    for (at, c) in cluster.char_indices() {
        let t = g.short.get(&c).copied().unwrap_or(Takes::Nothing);
        if c == 'V' {
            out.skip_discovery = true;
        }
        if t == Takes::Nothing {
            continue;
        }
        // the rest of the cluster is this option's value (clap drops a leading '=')
        let rest = &cluster[at + c.len_utf8()..];
        let (value, consumed_next) = if !rest.is_empty() {
            (Some(OsString::from(rest.strip_prefix('=').unwrap_or(rest))), false)
        } else if value_follows(t, next) {
            (next.map(OsStr::to_os_string), true)
        } else {
            (None, false)
        };
        if c == 'C'
            && let Some(v) = value
        {
            out.cwd = Some(PathBuf::from(v));
        }
        return consumed_next;
    }
    false
}

pub(crate) fn pre_scan_args<I, S>(args: I) -> PreScan
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let g = grammar();
    let args: Vec<OsString> = args.into_iter().skip(1).map(Into::into).collect();
    let mut out = PreScan { cwd: None, no_plugins: false, cli_trust: None, skip_discovery: false, json: false };
    let mut saw_positional = false;
    let mut in_subcommand = false;
    let mut i = 0;
    while i < args.len() {
        let s = args[i].as_os_str();
        let next = args.get(i + 1).map(OsString::as_os_str);
        i += 1;
        if s == "--" {
            break;
        }
        if let Some((name, attached)) = split_long(s) {
            let t = name.and_then(|n| g.long.get(n).copied()).unwrap_or(Takes::Nothing);
            let value = match attached {
                Some(v) => Some(v),
                None if value_follows(t, next) => {
                    i += 1;
                    next.map(OsStr::to_os_string)
                }
                None => None,
            };
            match name {
                Some("clean") => out.no_plugins = true,
                Some("trust") => out.cli_trust = Some(true),
                Some("no-trust") => out.cli_trust = Some(false),
                Some("version") => out.skip_discovery = true,
                Some("cwd") => {
                    if let Some(v) = value {
                        out.cwd = Some(PathBuf::from(v));
                    }
                }
                Some("json") if !in_subcommand => out.json = true,
                Some("mode") if !in_subcommand => match value.as_deref().and_then(|v| v.to_str()) {
                    Some("json") => out.json = true,
                    Some("text" | "rpc") => out.json = false,
                    _ => {}
                },
                _ => {}
            }
            continue;
        }
        if scan_short_cluster(s, &g, next, &mut out) {
            i += 1;
        } else if (s == "-" || !s.as_encoded_bytes().starts_with(b"-")) && !saw_positional {
            saw_positional = true;
            if s.to_str().is_some_and(|name| g.subcommands.contains(name)) {
                in_subcommand = true;
                out.json = false;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::pre_scan_args;

    #[test]
    fn pre_scan_json_stops_at_double_dash_and_subcommands() {
        assert!(pre_scan_args(["mare", "--json", "hello"]).json);
        assert!(pre_scan_args(["mare", "--mode=json", "hello"]).json);
        assert!(pre_scan_args(["mare", "--mode", "json", "hello"]).json);
        assert!(pre_scan_args(["mare", "hello", "--json"]).json);
        assert!(!pre_scan_args(["mare", "--", "--json"]).json);
        assert!(!pre_scan_args(["mare", "doctor", "--json"]).json);
        assert!(!pre_scan_args(["mare", "--json", "doctor"]).json);
        assert!(!pre_scan_args(["mare", "--mode", "json", "--mode", "text"]).json);
        assert!(!pre_scan_args(["mare", "--append-system-prompt=--json"]).json);
        // -V skips flag discovery; unknown --mode values do not flip json; -C sets cwd.
        let v = pre_scan_args(["mare", "-V"]);
        assert!(v.skip_discovery);
        let unknown_mode = pre_scan_args(["mare", "--mode", "bogus", "hi"]);
        assert!(!unknown_mode.json);
        let with_cwd = pre_scan_args(["mare", "-C", "/tmp/work", "--json", "hi"]);
        assert_eq!(with_cwd.cwd.as_deref(), Some(std::path::Path::new("/tmp/work")));
        assert!(with_cwd.json);
    }
}
