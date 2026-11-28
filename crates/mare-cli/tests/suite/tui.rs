//! TUI e2e, main layer: the real app in-process (`mare_cli::testkit`), its frames fed into
//! a virtual terminal (vt100; Codex `tui/src/test_backend.rs` VT100Backend, pi
//! `packages/tui/test/virtual-terminal.ts`). Offline model (`provider-faux`); assertions and
//! snapshots are on screen text, never raw bytes.

use std::cell::RefCell;
use std::future::Future;
use std::num::NonZeroU16;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, resume_unwind};

use futures_util::FutureExt;
use std::time::{Duration, Instant};

use mare_cli::testkit::Vt;
use mare_shared::ui::Mode;
use mare_tui::tty::{KeyCode, KeyEvent, Modifiers};
use serde_json::{Value, json};

use crate::common::*;

mod agents;
mod components;
mod dialogs;
mod editor_api;
mod images;
mod input;
mod interaction_hints;
mod invariants;
mod keys;
mod layout;
mod matrix;
#[path = "onboarding_extras.rs"]
mod onboarding_extras;
mod overlays;
mod questions_native;
mod resume;
mod screen;
mod side;
mod startup;
mod state_transitions;
mod statusline;

/// Ceiling of every wait on the app (a condition poll, not a sleep): generous so a loaded
/// machine finishes first, and a real hang still fails with the screen in the message.
const WAIT: Duration = Duration::from_secs(60);

tokio::task_local! {
    static CLEANUPS: RefCell<Vec<tokio::task::JoinHandle<()>>>;
}

/// Keep Lua's LocalSet alive through asynchronous fixture teardown, even after an assertion.
async fn local(future: impl Future<Output = ()>) {
    tokio::task::LocalSet::new()
        .run_until(CLEANUPS.scope(RefCell::new(Vec::new()), async {
            let result = AssertUnwindSafe(future).catch_unwind().await;
            let mut cleanup_panic = None;
            loop {
                let pending = CLEANUPS.with(|cleanups| std::mem::take(&mut *cleanups.borrow_mut()));
                if pending.is_empty() {
                    break;
                }
                for cleanup in pending {
                    if let Err(error) = cleanup.await {
                        let failure = error.try_into_panic().unwrap_or_else(|error| {
                            let message = format!("virtual terminal teardown failed: {error}");
                            eprintln!("{message}");
                            Box::new(message) as Box<dyn std::any::Any + Send>
                        });
                        cleanup_panic.get_or_insert(failure);
                    }
                }
            }
            if let Err(error) = result {
                resume_unwind(error);
            }
            if let Some(error) = cleanup_panic {
                resume_unwind(error);
            }
        }))
        .await;
}

struct VtResources {
    vt: Vt,
    _env: TestEnv,
}

impl Deref for VtResources {
    type Target = Vt;
    fn deref(&self) -> &Vt {
        &self.vt
    }
}

impl DerefMut for VtResources {
    fn deref_mut(&mut self) -> &mut Vt {
        &mut self.vt
    }
}

struct OwnedVt(Option<VtResources>);

impl OwnedVt {
    async fn new(env: TestEnv, opts: mare_host::HostOptions, w: u16, h: u16) -> Self {
        let vt = Vt::new(opts, w, h).await.unwrap();
        Self(Some(VtResources { vt, _env: env }))
    }
}

impl Deref for OwnedVt {
    type Target = VtResources;
    fn deref(&self) -> &VtResources {
        self.0.as_ref().unwrap()
    }
}

impl DerefMut for OwnedVt {
    fn deref_mut(&mut self) -> &mut VtResources {
        self.0.as_mut().unwrap()
    }
}

impl Drop for OwnedVt {
    fn drop(&mut self) {
        let Some(mut resources) = self.0.take() else { return };
        let cleanup = tokio::task::spawn_local(async move {
            resources.vt.shutdown().await;
            drop(resources);
        });
        CLEANUPS.with(|cleanups| cleanups.borrow_mut().push(cleanup));
    }
}

/// The app plus a virtual terminal with scrollback.
struct Term {
    owned: OwnedVt,
    parser: vt100::Parser,
    /// turns ended before the last submit
    ended: usize,
    saw_welcome_banner: bool,
    skip_drop_invariants: bool,
}

impl Drop for Term {
    fn drop(&mut self) {
        if self.owned.0.is_none() || self.skip_drop_invariants || std::thread::panicking() {
            return;
        }
        if let Err(msg) = self.check_invariants() {
            panic!("{msg}");
        }
    }
}

impl Deref for Term {
    type Target = VtResources;
    fn deref(&self) -> &VtResources {
        &self.owned
    }
}

impl DerefMut for Term {
    fn deref_mut(&mut self) -> &mut VtResources {
        &mut self.owned
    }
}

impl Term {
    async fn new(script: Value, w: u16, h: u16) -> Self {
        Self::with(script, json!({}), w, h).await
    }

    /// A fixed color-capable terminal for assertions about colored presentation.
    async fn colored(script: Value, w: u16, h: u16) -> Self {
        let mut env = TestEnv::new().await;
        env.color_terminal();
        Self::in_env(env, script, json!({}), json!({}), w, h).await
    }

    async fn with(script: Value, config: Value, w: u16, h: u16) -> Self {
        Self::build(script, config, json!({}), w, h, |_| {}).await
    }

    async fn with_all_builtins(
        script: Value,
        config: Value,
        args: Value,
        w: u16,
        h: u16,
        prepare: impl FnOnce(&mut TestEnv),
    ) -> Self {
        let mut env = TestEnv::new().await;
        prepare(&mut env);
        Self::in_env_inner(env, script, config, args, w, h, true).await
    }

    /// `prepare` runs on the workspace before the host starts (e.g. project resources).
    async fn build(script: Value, config: Value, args: Value, w: u16, h: u16, prepare: impl FnOnce(&TestEnv)) -> Self {
        let env = TestEnv::new().await;
        prepare(&env);
        Self::in_env(env, script, config, args, w, h).await
    }

    async fn in_env(env: TestEnv, script: Value, config: Value, args: Value, w: u16, h: u16) -> Self {
        Self::in_env_inner(env, script, config, args, w, h, false).await
    }

    async fn in_env_inner(
        env: TestEnv,
        script: Value,
        config: Value,
        args: Value,
        w: u16,
        h: u16,
        all_builtins: bool,
    ) -> Self {
        // the logo and the welcome line (with the temp cwd) have their own render tests; the
        // permissions plugin on, as in normal use
        let mut b = env.host();
        if all_builtins {
            b = b.all_builtins();
        } else {
            b = b.builtins(&[
                "agents",
                "subagent",
                "jobs",
                "shell-mode",
                "compact",
                "lua-console",
                "mcp",
                "copy",
                "usage",
                "tools-apply-patch",
                "user-config",
                "onboarding",
                "plugins",
                "images",
                "file-ref",
            ]);
        }
        let mut b = b
            .faux(script)
            .config(json!({ "logo": false, "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } } }))
            .config(config)
            .args(args);
        b.opts.mode = Mode::Tui;
        b.opts.load_init = env.config_dir().join("init.lua").exists();
        let owned = OwnedVt::new(env, b.opts, w, h).await;
        let mut t = Self::from_vt(owned, vt100::Parser::new(nz(h), nz(w), 2000));
        t.step().await;
        t
    }

    fn from_vt(owned: OwnedVt, parser: vt100::Parser) -> Self {
        Self { owned, parser, ended: 0, saw_welcome_banner: false, skip_drop_invariants: false }
    }

    fn set_skip_drop_invariants(&mut self, skip: bool) {
        self.skip_drop_invariants = skip;
    }

    /// One frame: run what's queued, draw, feed the bytes to the virtual terminal.
    async fn step(&mut self) {
        self.vt.frame().await;
        let out = self.vt.output();
        self.parser.process(&out);
        if !self.saw_welcome_banner && invariants::has_welcome_banner(&self.screen()) {
            self.saw_welcome_banner = true;
        }
    }

    fn check_invariants(&mut self) -> Result<(), String> {
        let obs = invariants::observe_term(self, self.saw_welcome_banner);
        invariants::check_global_invariants(&obs)
    }

    /// Draw frames until `f(screen)` holds (polling with a deadline; no fixed sleeps).
    async fn until(&mut self, what: &str, f: impl Fn(&str) -> bool) {
        let end = Instant::now() + WAIT;
        loop {
            self.step().await;
            if f(&self.screen()) {
                return;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}; screen:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    fn screen(&self) -> String {
        self.parser.screen().contents()
    }

    /// Everything the terminal holds: scrollback (oldest first) and the visible screen.
    fn history(&mut self) -> String {
        let rows = self.parser.screen().size().0.get() as usize;
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let max = self.parser.screen().scrollback();
        let mut out = String::new();
        let mut off = max;
        loop {
            self.parser.screen_mut().set_scrollback(off);
            let c = self.parser.screen().contents();
            // at offset `off` the top `off.min(rows)` rows come from scrollback
            let take = if off == 0 { rows } else { off.min(rows) };
            for l in c.lines().take(take) {
                out.push_str(l);
                out.push('\n');
            }
            if off == 0 {
                break;
            }
            off = off.saturating_sub(rows);
        }
        self.parser.screen_mut().set_scrollback(0);
        out
    }

    fn key(&mut self, code: KeyCode, m: Modifiers) {
        self.vt.key(KeyEvent::new(code, m));
    }

    /// Type `text`, press Enter, and remember how many turns had ended (for [`Self::idle`]).
    /// Waits for startup to finish first: Enter while startup is still in progress sends
    /// nothing by design (app/startup/gate.rs), and a slow `app/ready` handler under parallel
    /// load would otherwise swallow the submission and stall the test until its deadline.
    async fn submit(&mut self, text: &str) {
        self.until_vt("startup to finish", |t| !t.vt.starting()).await;
        self.ended = self.vt.turns_ended();
        self.vt.type_str(text);
        self.key(KeyCode::Enter, Modifiers::NONE);
        self.step().await;
    }

    async fn resize(&mut self, w: u16, h: u16) {
        self.parser.screen_mut().set_size(nz(h), nz(w));
        self.vt.resize(w, h);
        self.step().await;
    }

    /// Wait until the turn started by the last [`Self::submit`] has ended, then draw.
    async fn idle(&mut self) {
        let end = Instant::now() + WAIT;
        loop {
            self.step().await;
            if self.vt.turns_ended() > self.ended && !self.vt.busy() {
                self.step().await;
                return;
            }
            assert!(Instant::now() < end, "turn never ended; screen:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
}

impl Term {
    /// Draw frames until `f(self)` holds (for conditions that aren't screen text).
    async fn until_vt(&mut self, what: &str, f: impl Fn(&mut Self) -> bool) {
        let end = Instant::now() + WAIT;
        loop {
            self.step().await;
            if f(self) {
                return;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}; screen:\n{}", self.screen());
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    fn ctrl(&mut self, c: char) {
        self.key(KeyCode::Char(c), Modifiers::CONTROL);
    }
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).expect("non-zero terminal size")
}

impl Term {
    /// Screen text for a snapshot: notices dropped, redrawn, trailing blank lines trimmed.
    async fn snap(&mut self) -> String {
        self.vt.dismiss_notices();
        self.step().await;
        trimmed(&self.screen())
    }
}

/// Screen text without trailing blank lines, temp paths and elapsed timers masked (stable snapshots).
fn trimmed(s: &str) -> String {
    let lines: Vec<String> = s.lines().map(|l| mask_working_elapsed(&mask_turn_end(&mask_tmp(l.trim_end())))).collect();
    let end = lines.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
    lines[..end].join("\n")
}

/// Mask wall-clock elapsed seconds (and delayed tips) on the working row (`… (0s · Esc to interrupt)` → `… (Ns · Esc to interrupt)`).
fn mask_working_elapsed(l: &str) -> String {
    let suffix = "s · Esc to interrupt)";
    if let Some(end) = l.find(suffix)
        && let Some(open) = l[..end].rfind('(')
        && open + 1 < end
        && l[open + 1..end].bytes().all(|b| b.is_ascii_digit())
    {
        let after = &l[end + suffix.len()..];
        let tail = after.split("  · Tip:").next().unwrap_or(after);
        return format!("{}N{suffix}{tail}", &l[..open + 1]);
    }
    l.to_string()
}

/// The closing line of a turn ("worked 4s · 28 tokens · 02:46"): duration and clock masked.
fn mask_turn_end(l: &str) -> String {
    let lead = l.len() - l.trim_start().len();
    let body = &l[lead..];
    let Some(rest) = body.strip_prefix("worked ").or_else(|| body.strip_prefix("interrupted after ")) else {
        return l.to_string();
    };
    let verb = &body[..body.len() - rest.len()];
    let mut parts: Vec<String> = rest.split(" · ").map(str::to_string).collect();
    parts[0] = "Ns".into();
    if let Some(last) = parts.last_mut()
        && last.len() == 5
        && last.as_bytes()[2] == b':'
    {
        *last = "HH:MM".into();
    }
    format!("{}{verb}{}", &l[..lead], parts.join(" · "))
}

/// A Windows temp path (`C:\Users\…\Temp\.tmpX`, `~\AppData\Local\Temp\.tmpX`, shortened
/// `…\Temp\.tmpX`), the whole token, as `/tmp/x`: [`mask_tmp`] then masks it like a Unix one.
fn win_tmp_as_unix(l: &str) -> String {
    let mut out = String::new();
    let mut rest = l;
    // a temp dir's own name (`\.tmpX`), or a path cut off by a title's width before it reaches
    // that name (`C:\Users\…\AppData\…`, `~\AppData\…`)
    let next = |r: &str| ["\\.tmp", "C:\\Users\\", "~\\AppData\\"].iter().filter_map(|p| r.find(p)).min();
    while let Some(i) = next(rest) {
        let start =
            rest[..i].rfind([' ', '│']).map(|j| j + rest[j..].chars().next().map_or(1, char::len_utf8)).unwrap_or(0);
        let end = rest[i..].find([' ', '│']).map_or(rest.len(), |e| i + e);
        out.push_str(&rest[..start]);
        out.push_str("/tmp/x");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// `/private/var/folders/…`, `/var/folders/…`, `/tmp/…` (possibly truncated with `…`) → `<tmp>`,
/// and the padding / title-border fill after it collapsed (temp paths differ per platform).
fn mask_tmp(l: &str) -> String {
    let configured = mask_configured_tmp(l);
    let l = &win_tmp_as_unix(&configured);
    let mut out = String::new();
    let mut rest = l.as_str();
    // a temp path shortened by `display_path` ("…/T/.tmpAbC123", "…/folders/k4/…/.tmpX")
    while let Some(i) = ["…/T/.tmp", "…/folders/", "…/tmp/"].iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find([' ', '│']).unwrap_or(tail.len());
        out.push_str("<tmp>");
        rest = tail[end..].trim_start_matches(' ');
        if end < tail.len() && tail[end..].starts_with(' ') {
            out.push(' ');
        }
    }
    let shortened = std::mem::take(&mut out) + rest;
    let mut rest = shortened.as_str();
    while let Some(i) = ["/private/var/folders/", "/var/folders/", "/tmp/"].iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find([' ', '│', '…']).unwrap_or(tail.len());
        out.push_str("<tmp>");
        rest = &tail[end..];
        if let Some(r) = rest.strip_prefix('…') {
            rest = r;
        }
        // temp paths differ in length per platform: the padding after them too
        let trimmed = rest.trim_start_matches(' ');
        if trimmed.len() != rest.len() {
            out.push(' ');
            rest = trimmed;
        }
        // …and the border fill after a path in a box title
        let trimmed = rest.trim_start_matches('─');
        if trimmed.len() != rest.len() {
            out.push('─');
            rest = trimmed;
        }
    }
    out.push_str(rest);
    out
}

/// Mask the temp root selected by the process environment as well as the standard platform
/// locations above. Acceptance can use a short disk-backed directory to keep Unix socket paths
/// within `sockaddr_un` limits, so snapshots must not depend on that configured root's name.
fn mask_configured_tmp(line: &str) -> String {
    let mut temp_dirs = vec![std::env::temp_dir()];
    for variable in ["MARE_TMPDIR", "TMPDIR", "TMP", "TEMP"] {
        if let Some(val) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
            temp_dirs.push(std::path::PathBuf::from(val));
        }
    }
    let mut homes = Vec::new();
    for variable in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(variable).filter(|v| !v.is_empty()).map(std::path::PathBuf::from) {
            homes.push(home);
        }
    }
    if let Some(home) = dirs::home_dir() {
        homes.push(home);
    }
    let mut roots = Vec::new();
    let push_root = |roots: &mut Vec<String>, s: String| {
        let trimmed = s.trim_end_matches(['/', '\\']).to_string();
        if !trimmed.is_empty() && !roots.contains(&trimmed) {
            roots.push(trimmed);
        }
    };
    for raw_temp in temp_dirs {
        let candidates = match dunce::canonicalize(&raw_temp) {
            Ok(canon) if canon != raw_temp => vec![raw_temp, canon],
            _ => vec![raw_temp],
        };
        for temp in candidates {
            push_root(&mut roots, temp.to_string_lossy().into_owned());
            // `display_path` may retain only the end of a long configured temp root.
            for ancestor in temp.ancestors().skip(1) {
                if let Ok(relative) = temp.strip_prefix(ancestor)
                    && relative.components().count() >= 2
                {
                    push_root(&mut roots, format!("…{}{}", std::path::MAIN_SEPARATOR, relative.display()));
                }
            }
            for raw_home in &homes {
                let home_candidates = match dunce::canonicalize(raw_home) {
                    Ok(canon) if canon != *raw_home => vec![raw_home.clone(), canon],
                    _ => vec![raw_home.clone()],
                };
                for home in home_candidates {
                    if let Ok(relative) = temp.strip_prefix(&home) {
                        let rel = relative.to_string_lossy();
                        let rel = rel.trim_end_matches(['/', '\\']);
                        if !rel.is_empty() {
                            push_root(&mut roots, format!("~{}{}", std::path::MAIN_SEPARATOR, rel));
                        }
                    }
                }
            }
        }
    }
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));

    for root in roots {
        // A box title can truncate the path before it reaches the end of the temp root.
        // Match that visible prefix only when it ends in the truncation marker.
        let truncated_start = || {
            let min_prefix = if root.starts_with('~') { 3 } else { 2 };
            line.match_indices(root.chars().next()?).find_map(|(start, _)| {
                let prefix = line[start..].split_once('…')?.0;
                (prefix.len() >= min_prefix && root.starts_with(prefix)).then_some(start)
            })
        };
        let Some(start) = line.find(&root).or_else(truncated_start) else { continue };
        let tail = &line[start..];
        let end = tail.find([' ', '│', '─']).unwrap_or(tail.len());
        let mut masked = String::with_capacity(line.len());
        masked.push_str(&line[..start]);
        masked.push_str("<tmp>");
        let mut rest = &tail[end..];
        let spaced = rest.trim_start_matches(' ');
        if spaced.len() != rest.len() {
            masked.push(' ');
            rest = spaced;
        }
        let bordered = rest.trim_start_matches('─');
        if bordered.len() != rest.len() {
            masked.push('─');
            rest = bordered;
        }
        masked.push_str(rest);
        return masked;
    }
    line.to_string()
}

#[test]
fn snapshots_mask_the_configured_temporary_directory() {
    let path = std::env::temp_dir().join("random-temp-child");
    let raw = format!("cwd {} ─╮", path.display());
    assert_eq!(mask_tmp(&raw), "cwd <tmp> ─╮");
    let parent = std::env::temp_dir();
    if let Some(ancestor) = parent.ancestors().nth(2)
        && let Ok(relative) = parent.strip_prefix(ancestor)
    {
        let shortened = format!(
            "cwd …{}{}{}random-temp-child ─╮",
            std::path::MAIN_SEPARATOR,
            relative.display(),
            std::path::MAIN_SEPARATOR
        );
        assert_eq!(mask_tmp(&shortened), "cwd <tmp> ─╮");
    }
    for home in ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(|variable| std::env::var_os(variable).map(std::path::PathBuf::from))
        .chain(dirs::home_dir())
    {
        if let Ok(relative) = path.strip_prefix(home) {
            let abbreviated = format!("cwd ~{}{} ─╮", std::path::MAIN_SEPARATOR, relative.display());
            assert_eq!(mask_tmp(&abbreviated), "cwd <tmp> ─╮");
            let truncated =
                format!("cwd ~{}{}{}… ─╮", std::path::MAIN_SEPARATOR, relative.display(), std::path::MAIN_SEPARATOR);
            assert_eq!(mask_tmp(&truncated), "cwd <tmp> ─╮");
            let rel_str = relative.to_string_lossy();
            if let Some((idx, _)) = rel_str.char_indices().last()
                && idx > 0
            {
                let proper_prefix = format!("cwd ~{}{}… ─╮", std::path::MAIN_SEPARATOR, &rel_str[..idx]);
                assert_eq!(mask_tmp(&proper_prefix), "cwd <tmp> ─╮");
            }
        }
    }
}

/// Draw frames until `done(vt, screen)` holds (WAIT at most).
async fn pump_until(vt: &mut Vt, parser: &mut vt100::Parser, what: &str, mut done: impl FnMut(&mut Vt, &str) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        vt.frame().await;
        parser.process(&vt.output());
        let screen = parser.screen().contents();
        if done(vt, &screen) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {screen}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn crate_resume_error(vt: &Vt) -> Option<String> {
    global(&vt.host, "SS", vt.host.service_table("session").unwrap());
    eval::<Option<String>>(&vt.host, "return SS:info(nil).resume_error").await
}

#[tokio::test]
async fn virtual_terminal_teardown_drains_after_a_body_panic() {
    let paths = std::rc::Rc::new(RefCell::new(None));
    let observed = paths.clone();
    let result = AssertUnwindSafe(local(async move {
        let term = Term::new(json!([]), 80, 20).await;
        *observed.borrow_mut() = Some((term._env.home.path().to_path_buf(), term._env.work.path().to_path_buf()));
        panic!("original virtual terminal body failure");
    }))
    .catch_unwind()
    .await;
    let error = result.expect_err("the original assertion must be preserved");
    assert_eq!(error.downcast_ref::<&str>().copied(), Some("original virtual terminal body failure"));
    let (home, work) = paths.borrow_mut().take().unwrap();
    assert!(!home.exists() && !work.exists(), "fixture files must be released after async shutdown");
}
