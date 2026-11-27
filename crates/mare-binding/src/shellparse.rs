//! Parse a shell command into plain-word segments before matching permission rules.
//! Generic: it knows nothing about rules — it only says whether a command is
//! *analyzable* (plain words joined by `&&`, `||`, `;`, `|`) and, if so, its segments.
//!
//! Bash uses tree-sitter-bash for word-only command sequences (Apache-2.0), with a parse
//! budget (time + node count) and an exception for fixed sinks (`/dev/null`, `2>&1`), which
//! do not change what the command can touch.
//! Anything else — substitutions, expansions, subshells, other redirections, `eval`, syntax
//! errors, unknown nodes, an exhausted budget — is *not analyzable* and falls back to approval.
//!
//! PowerShell uses a deliberately conservative lexer on every platform (no process spawn): only
//! plain words joined by `|` / `;`; any `$`, backtick, bracket, redirection, `&`, comma, … makes
//! the command not analyzable.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, Parser};

/// One plain command of a sequence. `op` joins it to the previous segment (`None` for the first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub argv: Vec<String>,
    pub op: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub analyzable: bool,
    pub reason: Option<String>,
    pub segments: Vec<Segment>,
    /// the inner script when the command was a `bash -c '<script>'` style wrapper
    pub script: Option<String>,
}

impl Parsed {
    fn no(reason: impl Into<String>) -> Self {
        Self { analyzable: false, reason: Some(reason.into()), segments: vec![], script: None }
    }
    fn no_with(reason: impl Into<String>, segments: Vec<Segment>) -> Self {
        Self { analyzable: false, reason: Some(reason.into()), segments, script: None }
    }
    fn yes(segments: Vec<Segment>) -> Self {
        Self { analyzable: true, reason: None, segments, script: None }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub time: Duration,
    pub max_nodes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self { time: Duration::from_millis(50), max_nodes: 5000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    PowerShell,
}

pub fn parse(command: &str, shell: Shell, budget: Budget) -> Parsed {
    parse_with(command, shell, budget, &[])
}

/// [`parse`] with extra fixed-sink redirect targets (besides `/dev/null`, `&1`, `&2`) that keep a
/// command analyzable — an extension point for plugins (e.g. `/dev/stderr`, `NUL` on Windows).
pub fn parse_with(command: &str, shell: Shell, budget: Budget, sinks: &[String]) -> Parsed {
    match shell {
        Shell::Bash => parse_bash(command, budget, 0, sinks),
        Shell::PowerShell => parse_powershell(command, 0),
    }
}

/// Analyze a literal argument vector constructed from a `mare.tool.command` template using the
/// same nested-shell and wrapper rules as `parse_with`, without shell word-splitting.
pub fn parse_argv(argv: Vec<String>, shell: Shell, budget: Budget, sinks: &[String]) -> Parsed {
    if argv.is_empty() {
        return Parsed::no("no command");
    }
    let segments = vec![Segment { argv, op: None }];
    match shell {
        Shell::Bash => finalize_bash_segments(segments, budget, 0, sinks),
        Shell::PowerShell => finalize_powershell_segments(segments, 0),
    }
}

fn parse_bash_tree(src: &str, budget: Budget) -> Result<tree_sitter::Tree, &'static str> {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_bash::LANGUAGE.into()).is_err() {
        return Err("bash grammar unavailable");
    }
    let start = Instant::now();
    let mut over = false;
    let mut progress = |_: &tree_sitter::ParseState| {
        if start.elapsed() > budget.time {
            over = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = src.as_bytes();
    parser
        .parse_with_options(
            &mut |i, _| if i < bytes.len() { &bytes[i..] } else { &[] },
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .ok_or(if over { "parse budget exceeded (time)" } else { "parse aborted" })
}

/// Literal argv candidates for advisory path checks. Dynamic words are never expanded or
/// represented as complete paths; finding none is not evidence that the script is safe.
pub fn literal_candidates(src: &str, shell: Shell, budget: Budget) -> Vec<String> {
    let mut words = Vec::new();
    if shell == Shell::Bash
        && let Ok(tree) = parse_bash_tree(src, budget)
    {
        let mut stack = vec![tree.root_node()];
        let mut visited = 0;
        while let Some(node) = stack.pop() {
            visited += 1;
            if visited > budget.max_nodes {
                break;
            }
            if matches!(node.kind(), "command_substitution" | "process_substitution" | "subshell") {
                continue;
            }
            if node.kind() == "command" || node.kind() == "file_redirect" {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let word = if child.kind() == "command_name" { child.named_child(0) } else { Some(child) };
                    if let Some(word) = word.and_then(|n| literal(n, src)) {
                        words.push(word);
                    }
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    // An erroring parse or the conservative PowerShell parser may not expose segments. This
    // lexical fallback only emits whole, statically literal words and never expands anything.
    words.extend(literal_fallback(src, shell));
    candidate_words(words)
}

/// Literal arguments supplied directly as an argv template do not go through shell expansion.
pub fn literal_argv_candidates(argv: &[String]) -> Vec<String> {
    candidate_words(argv.iter().filter(|s| !s.contains(['$', '`', '*', '?', '[', ']'])).cloned())
}

fn candidate_words(words: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for word in words {
        let stripped = word.strip_prefix("--").and_then(|rest| rest.split_once('=')).and_then(|(opt, val)| {
            (!opt.is_empty() && opt.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(val)
        });
        let candidate = stripped.unwrap_or(&word);
        if candidate.is_empty()
            || (candidate.starts_with('~')
                && candidate != "~"
                && !candidate.starts_with("~/")
                && !(cfg!(windows) && candidate.starts_with("~\\")))
            || candidate.contains("://")
            || (stripped.is_none() && candidate.starts_with('-'))
        {
            continue;
        }
        if !out.iter().any(|p| p == candidate) {
            out.push(candidate.to_owned());
        }
    }
    out
}

fn literal_fallback(src: &str, shell: Shell) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = src.chars().peekable();
    let mut word = String::new();
    let mut valid = true;
    let mut quoted = None;
    let mut started = false;
    let flush = |out: &mut Vec<String>, word: &mut String, valid: &mut bool, started: &mut bool| {
        if *started && *valid && !word.is_empty() {
            out.push(std::mem::take(word));
        } else {
            word.clear();
        }
        *valid = true;
        *started = false;
    };
    while let Some(c) = chars.next() {
        if let Some(q) = quoted {
            if c == q {
                if shell == Shell::PowerShell && q == '\'' && chars.peek() == Some(&'\'') {
                    chars.next();
                    word.push('\'');
                } else {
                    quoted = None;
                }
            } else if q == '"' && matches!(c, '$' | '`' | '\\') {
                valid = false;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quoted = Some(c);
                started = true;
            }
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>') => {
                flush(&mut out, &mut word, &mut valid, &mut started);
            }
            '#' if !started => {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            }
            '\\' if shell == Shell::Bash => {
                started = true;
                match chars.next() {
                    Some(next) => word.push(next),
                    None => valid = false,
                }
            }
            '$' | '`' | '*' | '?' | '[' | ']' | '{' | '}' => {
                started = true;
                valid = false;
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if quoted.is_some() {
        valid = false;
    }
    flush(&mut out, &mut word, &mut valid, &mut started);
    out
}

// ---- bash ---------------------------------------------------------------------------------------

const BASH_NAMES: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "ash"];

fn is_shell_binary(name: &str) -> bool {
    BASH_NAMES.contains(&name) || PWSH_NAMES.contains(&name) || name == "cmd"
}

fn is_wrapped_shell(argv: &[String]) -> bool {
    let inner = crate::danger::unwrap_argv(argv);
    inner.len() > 1 && inner.len() < argv.len() && is_shell_binary(&shell_name(&inner[0]))
}

fn shell_name(argv0: &str) -> String {
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0).to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_string()
}

fn parse_bash(src: &str, budget: Budget, depth: u8, sinks: &[String]) -> Parsed {
    let tree = match parse_bash_tree(src, budget) {
        Ok(tree) => tree,
        Err(reason) => return Parsed::no(reason),
    };
    let root = tree.root_node();
    if root.has_error() {
        return Parsed::no("syntax error");
    }

    // walk: allow only word-only command sequences + fixed-sink redirects
    const ALLOWED: &[&str] = &[
        "program",
        "list",
        "pipeline",
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
        "redirected_statement",
        "comment",
    ];
    const OPS: &[&str] = &["&&", "||", ";", "|"];
    let mut nodes = 0usize;
    let mut stack = vec![root];
    let mut commands: Vec<Node> = vec![];
    let mut ops: Vec<(usize, String)> = vec![];
    let mut reject_reason: Option<String> = None;
    let mut has_non_sink_redirect = false;
    while let Some(node) = stack.pop() {
        nodes += 1;
        if nodes > budget.max_nodes {
            return Parsed::no("parse budget exceeded (nodes)");
        }
        let kind = node.kind();
        if node.is_named() {
            if kind == "file_redirect" {
                if !fixed_sink(node, src, sinks) {
                    has_non_sink_redirect = true;
                    if let Some(dyn_reason) = redirect_dynamic_reason(node, src) {
                        reject_reason.get_or_insert(dyn_reason);
                    }
                }
                continue; // validated (or rejected); its children are the target, not a command
            }
            if !ALLOWED.contains(&kind) {
                let backticks = kind == "command_substitution" && src[node.start_byte()..].starts_with('`');
                reject_reason.get_or_insert_with(|| if backticks { "backticks".into() } else { describe(kind) });
                continue; // don't descend into disallowed subtrees (e.g. subshell / $(...))
            }
            if kind == "command" {
                commands.push(node);
            }
        } else if OPS.contains(&kind) {
            ops.push((node.start_byte(), kind.to_string()));
        } else if !(kind == "\"" || kind == "'" || kind == "\n" || kind.trim().is_empty()) {
            reject_reason.get_or_insert_with(|| describe(kind));
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    commands.sort_by_key(Node::start_byte);
    ops.sort();

    let mut segments = Vec::with_capacity(commands.len());
    let mut prev_end = 0usize;
    for cmd in &commands {
        let Some(argv) = plain_words(*cmd, src) else {
            reject_reason.get_or_insert_with(|| "non-literal word".into());
            prev_end = cmd.end_byte();
            continue;
        };
        // the operator between the previous command and this one (a newline acts like `;`)
        let op = if segments.is_empty() {
            None
        } else {
            Some(
                ops.iter()
                    .rev()
                    .find(|(at, _)| *at >= prev_end && *at < cmd.start_byte())
                    .map(|(_, o)| o.clone())
                    .unwrap_or_else(|| ";".into()),
            )
        };
        prev_end = cmd.end_byte();
        segments.push(Segment { argv, op });
    }
    if let Some(reason) = reject_reason {
        return Parsed::no_with(reason, segments);
    }
    let finalized = finalize_bash_segments(segments, budget, depth, sinks);
    if finalized.analyzable && has_non_sink_redirect {
        return Parsed::no_with("redirection", finalized.segments);
    }
    finalized
}

fn finalize_bash_segments(segments: Vec<Segment>, budget: Budget, depth: u8, sinks: &[String]) -> Parsed {
    if segments.is_empty() {
        return Parsed::no("no command");
    }
    for s in &segments {
        let name = shell_name(&s.argv[0]);
        if matches!(name.as_str(), "eval" | "exec" | "source" | ".") {
            return Parsed::no_with(format!("`{name}` runs arbitrary code"), segments);
        }
        if name == "env" {
            for arg in s.argv.iter().skip(1) {
                if arg == "--" {
                    break;
                }
                if arg == "-S" || arg == "--split-string" || arg.starts_with("-S") || arg.starts_with("--split-string=")
                {
                    return Parsed::no_with("`env -S` splits arguments at run time", segments);
                }
            }
        }
    }
    // `bash -lc '<script>'`: parse the inner script once (only when it's the whole command)
    if let Some(script) = nested_script(&segments) {
        if segments.len() != 1 || depth > 0 {
            return if depth == 0 { Parsed::no_with("nested shell", segments) } else { Parsed::no("nested shell") };
        }
        let mut inner = parse_bash(&script, budget, depth + 1, sinks);
        inner.script = Some(script);
        return inner;
    }
    if segments.iter().any(|s| is_shell_binary(&shell_name(&s.argv[0])) && s.argv.len() > 1) {
        // a shell with arguments we don't understand (scripts, other flags, nested pwsh/cmd)
        return Parsed::no_with("nested shell", segments);
    }
    // a shell behind a command wrapper (`env sh -c '…'`, `nohup bash -c '…'`, `timeout 5 sh -c`,
    // `busybox sh -c`): its script runs too, but no rule sees inside it — not analyzable, while
    // keeping outer segments so a definite `forbidden` segment still denies (PERM-ESCAPE-b)
    if segments.iter().any(|s| is_wrapped_shell(&s.argv)) {
        return Parsed::no_with("shell behind a command wrapper", segments);
    }
    Parsed::yes(segments)
}

fn nested_script(segments: &[Segment]) -> Option<String> {
    segments.iter().find_map(|s| match s.argv.as_slice() {
        [sh, flag, script]
            if BASH_NAMES.contains(&shell_name(sh).as_str()) && matches!(flag.as_str(), "-c" | "-lc") =>
        {
            Some(script.clone())
        }
        _ => None,
    })
}

/// `> /dev/null`, `2>/dev/null`, `&>/dev/null`, `2>&1`, `1>&2`: the redirection can't write
/// anywhere meaningful, so it doesn't change what the command can touch (such commands remain
/// matchable).
fn fixed_sink(node: Node, src: &str, extra: &[String]) -> bool {
    let text = node.utf8_text(src.as_bytes()).unwrap_or("");
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let target = t.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
    let Some(rest) = target.strip_prefix(">>").or_else(|| target.strip_prefix('>')) else {
        return false;
    };
    rest == "/dev/null" || matches!(rest, "&1" | "&2") || extra.iter().any(|e| e == rest)
}

/// If a `file_redirect` destination contains any dynamic or non-literal shell syntax (e.g.
/// `> $(cmd)`, `> $VAR`, `` > `cmd` ``), return its reason so the command is treated as opaque
/// rather than a plain literal redirection.
fn redirect_dynamic_reason(node: Node, src: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "file_descriptor" | "number" => {}
            "word" | "string" | "raw_string" | "concatenation" => {
                if literal(child, src).is_none() {
                    return Some("non-literal word".into());
                }
            }
            "command_substitution" => {
                let backticks = src[child.start_byte()..].starts_with('`');
                return Some(if backticks { "backticks".into() } else { "command substitution".into() });
            }
            other => return Some(describe(other)),
        }
    }
    None
}

fn describe(kind: &str) -> String {
    match kind {
        "command_substitution" => "command substitution".into(),
        "process_substitution" => "process substitution".into(),
        "simple_expansion" | "expansion" => "variable expansion".into(),
        "arithmetic_expansion" => "arithmetic expansion".into(),
        "subshell" | "(" | ")" => "subshell".into(),
        "variable_assignment" => "variable assignment".into(),
        "heredoc_redirect" | "herestring_redirect" | "<<" | "<<<" => "here-document".into(),
        "`" => "backticks".into(),
        "&" => "background job".into(),
        k if k.ends_with("_statement") || matches!(k, "function_definition" | "compound_statement") => {
            "control flow".into()
        }
        k => format!("unsupported syntax: {k}"),
    }
}

/// Parse a plain command node: literal words only.
fn plain_words(cmd: Node, src: &str) -> Option<Vec<String>> {
    let mut words = vec![];
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let w = child.named_child(0)?;
                if w.kind() != "word" {
                    return None;
                }
                words.push(literal(w, src)?);
            }
            "file_redirect" => {} // validated as a fixed sink by the walk
            _ => words.push(literal(child, src)?),
        }
    }
    (!words.is_empty()).then_some(words)
}

/// The argv word an unquoted source word becomes, when that is certain. Brace, glob and tilde
/// expansion (and Zsh's `=cmd` and extended globs) rewrite a word at run time, so its spelling is
/// not its argv: such a word is not a literal and the command is not analyzable. A `#` in the
/// middle of a word is literal in POSIX/bash (only `#` at the start of a word starts a comment).
/// A backslash escape has one meaning (`\x` is `x`) and is resolved, so a rule naming `push`
/// matches `pu\sh` (PERM-ESCAPE).
fn plain_literal(word: &str) -> Option<String> {
    if word.starts_with(['=', '#']) || word.contains(['{', '}', '*', '?', '[', ']', '~', '^', '$', '`']) {
        return None;
    }
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\n') => {} // a line continuation joins the word
                Some(next) => out.push(next),
                None => return None,
            }
        } else {
            out.push(c);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn literal(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "word" | "number" => {
            let mut c = node.walk();
            if node.named_children(&mut c).next().is_some() {
                return None;
            }
            plain_literal(node.utf8_text(src.as_bytes()).ok()?)
        }
        "string" => {
            let mut c = node.walk();
            if node.named_children(&mut c).any(|p| p.kind() != "string_content") {
                return None;
            }
            let raw = node.utf8_text(src.as_bytes()).ok()?;
            let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
            // double quotes stop globbing and brace expansion, not escape removal
            if inner.as_bytes().windows(2).any(|p| p[0] == b'\\' && matches!(p[1], b'$' | b'`' | b'"' | b'\\' | b'\n'))
            {
                return None;
            }
            Some(inner.to_string())
        }
        "raw_string" => {
            let raw = node.utf8_text(src.as_bytes()).ok()?;
            Some(raw.strip_prefix('\'')?.strip_suffix('\'')?.to_string())
        }
        "concatenation" => {
            let mut out = String::new();
            let mut c = node.walk();
            for part in node.named_children(&mut c) {
                out.push_str(&literal(part, src)?);
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

// ---- PowerShell (conservative lexer) -----------------------------------------------------------

const PWSH_NAMES: &[&str] = &["pwsh", "powershell"];

fn parse_powershell(src: &str, depth: u8) -> Parsed {
    let mut segments: Vec<Segment> = vec![];
    let mut argv: Vec<String> = vec![];
    let mut word = String::new();
    let mut in_word = false;
    let mut pending_op: Option<String> = None;
    let mut chars = src.chars().peekable();
    let flush_word = |argv: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            argv.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // single quotes: literal ('' is an escaped quote)
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            word.push('\'');
                        }
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return Parsed::no("syntax error"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('$' | '`') => return Parsed::no("variable expansion"),
                        Some(ch) => word.push(ch),
                        None => return Parsed::no("syntax error"),
                    }
                }
            }
            '|' | ';' | '\n' => {
                if c == '|' && chars.peek() == Some(&'|') {
                    return Parsed::no("unsupported syntax: ||");
                }
                flush_word(&mut argv, &mut word, &mut in_word);
                if argv.is_empty() {
                    if c == '\n' {
                        continue;
                    }
                    return Parsed::no("syntax error");
                }
                segments.push(Segment { argv: std::mem::take(&mut argv), op: pending_op.take() });
                pending_op = Some(if c == '|' { "|".into() } else { ";".into() });
            }
            c if c.is_whitespace() => flush_word(&mut argv, &mut word, &mut in_word),
            '$' => return Parsed::no("variable expansion"),
            '`' => return Parsed::no("escape or line continuation"),
            '(' | ')' | '{' | '}' | '[' | ']' | '@' => return Parsed::no("subexpression"),
            '<' | '>' => return Parsed::no("redirection"),
            '&' => return Parsed::no("call operator or background job"),
            ',' => return Parsed::no("array"),
            '#' => return Parsed::no("comment"),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    flush_word(&mut argv, &mut word, &mut in_word);
    if !argv.is_empty() {
        segments.push(Segment { argv, op: pending_op.take() });
    } else if pending_op.is_some() {
        return Parsed::no("syntax error");
    }
    finalize_powershell_segments(segments, depth)
}

const POWERSHELL_PRE_FLAGS: &[&str] = &["-nologo", "-noprofile", "-noninteractive", "-mta", "-sta"];

fn is_ps_command_flag(lower: &str) -> bool {
    lower.starts_with("-c") && "-command".starts_with(lower)
}

fn extract_pwsh_command(argv: &[String]) -> Option<&str> {
    if argv.len() < 3 || !PWSH_NAMES.contains(&shell_name(&argv[0]).as_str()) {
        return None;
    }
    let mut i = 1;
    while i + 1 < argv.len() {
        let lower = argv[i].to_ascii_lowercase();
        if is_ps_command_flag(&lower) {
            let script = &argv[i + 1];
            return (i + 2 == argv.len() && script != "-").then_some(script.as_str());
        }
        if !POWERSHELL_PRE_FLAGS.contains(&lower.as_str()) {
            return None;
        }
        i += 1;
    }
    None
}

fn finalize_powershell_segments(segments: Vec<Segment>, depth: u8) -> Parsed {
    if segments.is_empty() {
        return Parsed::no("no command");
    }
    for s in &segments {
        let name = shell_name(&s.argv[0]);
        if matches!(name.as_str(), "invoke-expression" | "iex" | "invoke-command" | "icm" | "start-process") {
            return Parsed::no(format!("`{name}` runs arbitrary code"));
        }
    }
    // `pwsh -NoProfile -Command '<script>'`: parse once when only parameterless flags precede `-Command`
    if segments.len() == 1
        && depth == 0
        && let Some(script) = extract_pwsh_command(&segments[0].argv)
    {
        let script = script.to_string();
        let mut inner = parse_powershell(&script, 1);
        inner.script = Some(script);
        return inner;
    }
    if segments.iter().any(|s| is_shell_binary(&shell_name(&s.argv[0]))) {
        return if depth == 0 { Parsed::no_with("nested shell", segments) } else { Parsed::no("nested shell") };
    }
    if segments.iter().any(|s| is_wrapped_shell(&s.argv)) {
        return Parsed::no_with("shell behind a command wrapper", segments);
    }
    Parsed::yes(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(s: &str) -> Parsed {
        parse(s, Shell::Bash, Budget::default())
    }
    fn argvs(p: &Parsed) -> Vec<Vec<&str>> {
        p.segments.iter().map(|s| s.argv.iter().map(String::as_str).collect()).collect()
    }

    #[test]
    fn splits_plain_sequences_with_operators() {
        let p = bash("git status && cargo test -p x; echo 'a b' | wc -l");
        assert!(p.analyzable, "{p:?}");
        assert_eq!(
            argvs(&p),
            vec![vec!["git", "status"], vec!["cargo", "test", "-p", "x"], vec!["echo", "a b"], vec!["wc", "-l"]]
        );
        let ops: Vec<_> = p.segments.iter().map(|s| s.op.as_deref()).collect();
        assert_eq!(ops, vec![None, Some("&&"), Some(";"), Some("|")]);
    }

    #[test]
    fn dynamic_constructs_are_not_analyzable() {
        for (cmd, why) in [
            ("echo $(whoami)", "command substitution"),
            ("echo `whoami`", "backticks"),
            ("echo hi > out.txt", "redirection"),
            ("eval ls", "eval"),
            ("echo $HOME", "variable expansion"),
            ("(cd /tmp && ls)", "subshell"),
            ("FOO=1 ls", "variable assignment"),
            ("cat <(ls)", "process substitution"),
            ("ls &&", "syntax error"),
        ] {
            let p = bash(cmd);
            assert!(!p.analyzable, "{cmd} should not be analyzable");
            assert!(p.reason.as_deref().unwrap_or("").contains(why), "{cmd}: {:?}", p.reason);
        }
    }

    /// Words the shell rewrites at run time are not literals; quoting keeps them literal; a
    /// backslash escape is resolved to the character it escapes.
    #[test]
    fn runtime_rewritten_words_are_not_analyzable() {
        for cmd in [
            "git p{ush,}",
            "find . -del*",
            "find . -delet?",
            "find . -delet[e]",
            "echo ~",
            "echo HEAD^",
            "echo =sh",
            r#"echo "\$HOME""#,
            r#"echo "a\\b""#,
        ] {
            assert!(!bash(cmd).analyzable, "{cmd} should not be analyzable");
        }
        for (cmd, argv) in [
            ("git 'push'", vec!["git", "push"]),
            (r#"git "push""#, vec!["git", "push"]),
            ("git p'us'h", vec!["git", "push"]),
            (r"git pu\sh", vec!["git", "push"]),
            (r"gi\t pu\sh", vec!["git", "push"]),
            (r"find . -de\lete", vec!["find", ".", "-delete"]),
            (r#"echo '*?[]~^#=\' "-{a,b}""#, vec!["echo", r"*?[]~^#=\", "-{a,b}"]),
        ] {
            let p = bash(cmd);
            assert!(p.analyzable, "{cmd}: {p:?}");
            assert_eq!(argvs(&p), vec![argv], "{cmd}");
        }
    }

    #[test]
    fn fixed_sink_redirections_stay_analyzable() {
        let p = bash("cargo build > /dev/null 2>&1 && ls 2>/dev/null");
        assert!(p.analyzable, "{p:?}");
        assert_eq!(argvs(&p), vec![vec!["cargo", "build"], vec!["ls"]]);
    }

    #[test]
    fn nested_shell_script_is_parsed_once() {
        let p = bash("bash -lc 'git status && git diff'");
        assert!(p.analyzable, "{p:?}");
        assert_eq!(p.script.as_deref(), Some("git status && git diff"));
        assert_eq!(argvs(&p), vec![vec!["git", "status"], vec!["git", "diff"]]);
        assert!(!bash("sh -c \"sh -c 'ls'\"").analyzable, "only one level");
        assert!(!bash("ls && bash -c 'ls'").analyzable, "wrapper must be the whole command");
    }

    #[test]
    fn a_shell_behind_a_wrapper_is_not_analyzable() {
        for cmd in [
            "env sh -c 'git push'",
            "nohup bash -c 'ls'",
            "env FOO=1 bash -lc 'ls'",
            "timeout 5 sh -c 'ls'",
            "busybox sh -c 'rm -rf dist'",
            "env ash -c 'rm -rf dist'",
            "busybox ash -c 'rm -rf dist'",
        ] {
            let p = bash(cmd);
            assert!(!p.analyzable && p.reason.as_deref() == Some("shell behind a command wrapper"), "{cmd}: {p:?}");
        }
        assert!(bash("env FOO=1 ls -l").analyzable, "a wrapper around a plain command stays analyzable");
        assert!(bash("busybox ls -l").analyzable, "busybox around a plain command stays analyzable");
        for cmd in ["command -v sh git", "command -V sh git", "command -pv sh git"] {
            let p = bash(cmd);
            assert!(p.analyzable, "{cmd} is a lookup query, not a wrapped shell: {p:?}");
        }
        let wrapped_cmd = bash("command sh -c 'rm -rf dist'");
        assert!(
            !wrapped_cmd.analyzable && wrapped_cmd.reason.as_deref() == Some("shell behind a command wrapper"),
            "{wrapped_cmd:?}"
        );
    }

    #[test]
    fn compound_command_with_wrapped_shell_preserves_outer_segments() {
        let p = bash("git push && timeout 60 sh -c 'echo ok'");
        assert!(!p.analyzable);
        assert_eq!(p.reason.as_deref(), Some("shell behind a command wrapper"));
        assert_eq!(argvs(&p), vec![vec!["git", "push"], vec!["timeout", "60", "sh", "-c", "echo ok"]]);
    }

    #[test]
    fn mid_word_hash_is_analyzable_in_bash() {
        let p = bash("echo foo#bar");
        assert!(p.analyzable, "echo foo#bar should be analyzable: {p:?}");
        assert_eq!(argvs(&p), vec![vec!["echo", "foo#bar"]]);

        let p2 = bash("echo foo#bar && git push");
        assert!(p2.analyzable, "echo foo#bar && git push should be analyzable: {p2:?}");
        assert_eq!(argvs(&p2), vec![vec!["echo", "foo#bar"], vec!["git", "push"]]);

        let p_comment = bash("echo # comment");
        assert!(p_comment.analyzable, "{p_comment:?}");
        assert_eq!(argvs(&p_comment), vec![vec!["echo"]]);
        assert!(!bash("# just a comment").analyzable);
    }

    #[test]
    fn non_analyzable_segment_still_collects_subsequent_segments() {
        for cmd in ["echo ${foo} && git push", "echo $(whoami) && git push", "echo ~ && git push"] {
            let p = bash(cmd);
            assert!(!p.analyzable, "{cmd} should not be analyzable");
            assert_eq!(argvs(&p), vec![vec!["git", "push"]], "{cmd}: {p:?}");
        }
        let wrapped = bash("timeout 60 sh -c 'echo ok' && git push");
        assert!(!wrapped.analyzable);
        assert_eq!(wrapped.reason.as_deref(), Some("shell behind a command wrapper"));
        assert_eq!(argvs(&wrapped), vec![vec!["timeout", "60", "sh", "-c", "echo ok"], vec!["git", "push"]]);

        let nested_after_bad = bash("echo ${foo} && bash -c 'ls'");
        assert!(!nested_after_bad.analyzable, "must not unwrap nested shell after non-analyzable segment");
    }

    #[test]
    fn budget_exhaustion_is_not_analyzable() {
        let long = vec!["echo a"; 400].join(" && ");
        let p = parse(&long, Shell::Bash, Budget { time: Duration::from_secs(5), max_nodes: 50 });
        assert!(!p.analyzable && p.reason.as_deref().unwrap().contains("budget"), "{p:?}");
    }

    #[test]
    fn powershell_plain_pipelines_and_rejections() {
        let p = parse("Get-ChildItem -Recurse | Select-String foo; git status", Shell::PowerShell, Budget::default());
        assert!(p.analyzable, "{p:?}");
        assert_eq!(
            argvs(&p),
            vec![vec!["Get-ChildItem", "-Recurse"], vec!["Select-String", "foo"], vec!["git", "status"]]
        );
        let w = parse("pwsh -NoProfile -Command 'git log -1'", Shell::PowerShell, Budget::default());
        assert_eq!(argvs(&w), vec![vec!["git", "log", "-1"]]);
        for cmd in ["echo $env:PATH", "ls > out.txt", "& foo", "iex 'x'", "(Get-Date)"] {
            assert!(!parse(cmd, Shell::PowerShell, Budget::default()).analyzable, "{cmd}");
        }
    }

    #[test]
    fn parse_argv_wrapper_and_nested_shell_edges() {
        let pa = |args: &[&str], shell: Shell| {
            parse_argv(args.iter().map(ToString::to_string).collect(), shell, Budget::default(), &[])
        };
        // Cross-shell & wrapper parity across Bash / PowerShell modes
        for args in [
            &["env", "pwsh", "-Command", "Remove-Item x -Force"][..],
            &["env", "sh", "-c", "git status"][..],
            &["nohup", "env", "VAR=1", "sh", "-c", "git status"][..],
            &["busybox", "sh", "-c", "git status"][..],
        ] {
            for sh in [Shell::Bash, Shell::PowerShell] {
                let p = pa(args, sh);
                assert!(!p.analyzable, "{args:?} under {sh:?}: {p:?}");
                assert_eq!(p.reason.as_deref(), Some("shell behind a command wrapper"), "{args:?}: {p:?}");
            }
        }
        // POSIX shell in Bash mode unwraps `-c`; in PowerShell mode must NOT pose as a plain segment.
        let sh_bash = pa(&["sh", "-c", "git push"], Shell::Bash);
        assert!(sh_bash.analyzable, "{sh_bash:?}");
        assert_eq!(argvs(&sh_bash), vec![vec!["git", "push"]]);
        let sh_in_ps = pa(&["sh", "-c", "git push"], Shell::PowerShell);
        assert!(!sh_in_ps.analyzable, "sh -c in PowerShell must be nested shell: {sh_in_ps:?}");

        // Reject unknown / value-taking flags, `--`, extra trailing args, and stdin `-` in pwsh.
        for bad_pwsh in [
            &["pwsh", "-File", "evil.ps1", "-Command", "git status"][..],
            &["pwsh", "--", "-Command", "git status"][..],
            &["pwsh", "-EncodedCommand", "ZQBjAGgAbwA=", "-Command", "git status"][..],
            &["pwsh", "-Command", "-"][..],
            &["pwsh", "-Command", "git status", "extra"][..],
        ] {
            let p = pa(bad_pwsh, Shell::PowerShell);
            assert!(!p.analyzable, "{bad_pwsh:?}: {p:?}");
        }
        let ok_pwsh = pa(
            &["pwsh", "-NoLogo", "-NoProfile", "-NonInteractive", "-MTA", "-Command", "git status"],
            Shell::PowerShell,
        );
        assert!(ok_pwsh.analyzable, "{ok_pwsh:?}");
        assert_eq!(argvs(&ok_pwsh), vec![vec!["git", "status"]]);

        // Bash edge cases: empty / whitespace scripts, extra args, `--` separator
        for bad_bash in [
            &["bash", "-lc", ""][..],
            &["bash", "-lc", "   "][..],
            &["bash", "-c", "git status", "extra"][..],
            &["bash", "-c", "--", "git status"][..],
        ] {
            let p = pa(bad_bash, Shell::Bash);
            assert!(!p.analyzable, "{bad_bash:?}: {p:?}");
        }
    }

    #[test]
    fn literal_candidates_handles_multiline_comments_and_eq_flags() {
        let bash_comment = literal_candidates("# read ssh\ncat ~/.ssh/id_rsa", Shell::Bash, Budget::default());
        assert!(bash_comment.iter().any(|p| p == "~/.ssh/id_rsa"), "bash multiline comment: {bash_comment:?}");

        let pwsh_comment = literal_candidates("# check\nGet-Content .env", Shell::PowerShell, Budget::default());
        assert!(pwsh_comment.iter().any(|p| p == ".env"), "pwsh multiline comment: {pwsh_comment:?}");

        let eq_flags =
            literal_candidates("docker run --env-file=.env --config=.netrc img", Shell::Bash, Budget::default());
        assert!(eq_flags.iter().any(|p| p == ".env"), "missing --env-file=.env: {eq_flags:?}");
        assert!(eq_flags.iter().any(|p| p == ".netrc"), "missing --config=.netrc: {eq_flags:?}");
    }

    #[test]
    fn env_split_string_is_not_analyzable() {
        for cmd in ["env -S 'sh -c id'", "env --split-string='git status'", "env -u FOO -S 'echo x'"] {
            let p = bash(cmd);
            assert!(!p.analyzable, "{cmd}: {p:?}");
        }
    }

    #[test]
    fn literal_redirection_stays_distinct_from_dynamic_redirection_or_eval() {
        let plain = bash("printf ordinary > partial-marker");
        assert!(!plain.analyzable && plain.reason.as_deref() == Some("redirection"), "{plain:?}");
        assert_eq!(argvs(&plain), vec![vec!["printf", "ordinary"]]);

        let dyn_target = bash("printf ordinary > $(git push)");
        assert!(!dyn_target.analyzable && dyn_target.reason.as_deref() != Some("redirection"), "{dyn_target:?}");

        let with_eval = bash("printf ordinary > out.txt && eval \"$CMD\"");
        assert!(!with_eval.analyzable && with_eval.reason.as_deref() != Some("redirection"), "{with_eval:?}");
    }
}
