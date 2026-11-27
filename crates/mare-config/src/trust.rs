//! Project trust: a project's `.mare/plugins` (and `.mare/init.lua`) only load after the
//! user trusted the directory once. Decisions live in `~/.local/state/mare/trust.json`.

use std::path::Path;

use anyhow::Result;

/// trust.json format version. v1: `{version, decisions: {path: bool}}`. Unknown top-level
/// fields are kept on rewrite.
pub const VERSION: u64 = 1;
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(15);

/// The whole file (unknown fields kept) and its decisions.
fn load_file_at(
    p: &Path,
) -> Result<(serde_json::Map<String, serde_json::Value>, serde_json::Map<String, serde_json::Value>)> {
    let Some(raw) = crate::versioned_json::read(p, VERSION).map_err(anyhow::Error::msg)? else {
        return Ok((serde_json::Map::new(), serde_json::Map::new()));
    };
    let d = match raw.get("decisions") {
        Some(serde_json::Value::Object(m)) => m.clone(),
        _ => anyhow::bail!("{} is corrupt (expected 'decisions' object)", p.display()),
    };
    if d.values().any(|v| !v.is_boolean()) {
        anyhow::bail!("{} is corrupt (expected boolean decisions)", p.display());
    }
    Ok((raw, d))
}

fn lock_file(p: &Path) -> Result<std::fs::File> {
    lock_file_with_timeout(p, LOCK_TIMEOUT)
}

fn lock_file_with_timeout(p: &Path, timeout: std::time::Duration) -> Result<std::fs::File> {
    let lp = p.with_file_name("trust.json.lock");
    if let Some(dir) = lp.parent().filter(|d| !d.as_os_str().is_empty()) {
        mare_shared::private::create_dir_all(dir)?;
    }
    let f = mare_shared::private::open(std::fs::OpenOptions::new().write(true).create(true).truncate(false), &lp)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for lock {}", lp.display());
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

fn key(cwd: &Path) -> Result<String> {
    Ok(mare_shared::paths::normalize_workspace_path(cwd)?.to_string_lossy().into_owned())
}

fn lookup_at(p: &Path, cwd: &Path) -> Result<Option<bool>> {
    let (_, t) = load_file_at(p)?;
    let rr = mare_shared::project::resource_root(cwd);
    let subject = mare_shared::project::trust_subject(&rr);
    let deny_bound = rr.bound.as_deref().unwrap_or(subject);
    for d in rr.start.ancestors() {
        if let Some(false) = t.get(&key(d)?).and_then(|v| v.as_bool()) {
            return Ok(Some(false));
        }
        if d == deny_bound {
            break;
        }
    }
    Ok(t.get(&key(subject)?).and_then(|v| v.as_bool()).filter(|v| *v))
}

fn decision_at(p: &Path, cwd: &Path) -> Option<bool> {
    match lookup_at(p, cwd) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(path = %p.display(), "trust file is corrupt, failing closed: {err:#}");
            None
        }
    }
}

fn set_at(p: &Path, cwd: &Path, trusted: bool) -> Result<()> {
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        mare_shared::private::create_dir_all(dir)?;
    }
    let _guard = lock_file(p)?;
    let (mut file, mut t) = load_file_at(p)?;
    t.insert(key(cwd)?, serde_json::Value::Bool(trusted));
    file.insert("version".into(), VERSION.into());
    file.insert("decisions".into(), serde_json::Value::Object(t));
    let json = serde_json::to_string_pretty(&file)? + "\n";
    mare_shared::paths::write_atomic(p, json.as_bytes(), mare_shared::paths::WriteMode::Private)?;
    Ok(())
}

/// Recorded decision under the workspace trust rule (`start..=bound` deny, else `subject` allow),
/// returning `Err` if the trust file is unreadable or corrupt so callers can fail closed.
pub fn lookup_checked(cwd: &Path) -> Result<Option<bool>> {
    lookup_at(&mare_shared::paths::trust_file(), cwd)
}

/// Recorded decision under the workspace trust rule (`start..=bound` deny, else `subject` allow).
pub fn decision(cwd: &Path) -> Option<bool> {
    decision_at(&mare_shared::paths::trust_file(), cwd)
}

pub fn set(cwd: &Path, trusted: bool) -> Result<()> {
    let rr = mare_shared::project::resource_root(cwd);
    set_at(&mare_shared::paths::trust_file(), mare_shared::project::trust_subject(&rr), trusted)
}

/// Record trust on `dir` directly (used by `mare trust [<dir>] [--revoke]`).
pub fn set_dir(dir: &Path, trusted: bool) -> Result<()> {
    set_at(&mare_shared::paths::trust_file(), dir, trusted)
}

/// Project resources that only load in a trusted project:
/// `<root>/.mare/{plugins,init.lua,skills,prompts,SYSTEM.md,APPEND_SYSTEM.md}`,
/// and `.agents/skills` in the cwd or an ancestor up to the git root. A bare `.mare/` needs no decision.
pub fn protected_resources(cwd: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let rr = mare_shared::project::resource_root(cwd);
    if let Some(m) = rr.mare_dir() {
        for name in ["plugins", "plugins.json", "init.lua", "skills", "prompts", "SYSTEM.md", "APPEND_SYSTEM.md"] {
            let p = m.join(name);
            if p.exists() {
                out.push(p);
            }
        }
    }
    // `.agents/skills` up to the git root (to the filesystem root outside a repo), never the
    // user-level `~/.agents/skills`.
    // Intentionally differs from `mare_shared::project::resource_root`'s `bound` (`git_root`, which
    // returns `None` on `Marker::Stop` instead of stopping the walk on the invalid-gitfile dir).
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let user_skills = home_dir().map(|h| canon(&h.join(".agents").join("skills")));
    let mut d = Some(cwd);
    while let Some(dir) = d {
        let p = dir.join(".agents").join("skills");
        if p.is_dir() && user_skills.as_ref() != Some(&canon(&p)) {
            out.push(p);
        }
        // the walk ends at the repository's root, or where Git's discovery stops (an invalid
        // `.git` file: the same boundary context/skills_scan.lua loads `.agents/skills` up to)
        if mare_shared::git::marker(dir) != mare_shared::git::Marker::Skip {
            break;
        }
        d = dir.parent();
    }
    out
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
}

/// Where a trust decision came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `--trust` / `--no-trust`
    Cli,
    /// the saved decision on the resource root (or a deny on start..=root)
    Saved,
    /// asked now (interactive) and saved
    Asked,
    /// nothing is saved and the TUI will ask (an overlay after the first frame) —
    /// protected resources exist, or the folder is in a git repository: untrusted until
    /// answered
    Pending,
    /// protected resources exist but nobody could be asked (headless, no terminal): untrusted
    Undecided,
    /// nothing saved: the global default (`trust_default = "trust"` / `"distrust"`) decided
    Default,
    /// no protected resources, nothing saved, and nobody to ask (or not a git repository): no
    /// decision needed
    NotNeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trust {
    pub trusted: bool,
    pub source: Source,
}

impl Trust {
    /// "trusted" | "untrusted" | "undecided" | "none" (exposed to Lua as `mare.args.trust`).
    /// "undecided" = protected resources skipped because nobody could be asked (counts as
    /// untrusted); "none" = nothing protected here and nothing saved.
    pub fn state(&self) -> &'static str {
        match (self.trusted, self.source) {
            (true, _) => "trusted",
            (false, Source::Undecided | Source::Pending) => "undecided",
            (false, Source::NotNeeded) => "none",
            (false, _) => "untrusted",
        }
    }
}

/// The global default when nothing is saved for a folder (`trust_default`): ask (the
/// default), trust, or distrust without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefaultTrust {
    #[default]
    Ask,
    Trust,
    Distrust,
}

impl DefaultTrust {
    /// "ask" | "trust" | "distrust" ("always" / "never" are accepted too).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "trust" | "always" => Some(Self::Trust),
            "distrust" | "never" => Some(Self::Distrust),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Trust => "trust",
            Self::Distrust => "distrust",
        }
    }

    /// The `trust_default` option as far as it is known before init.lua runs: the command line
    /// (`--set trust_default=…`, in `cli`), then `MARE_TRUST_DEFAULT`, then state.json. A value
    /// set in init.lua takes effect once the configuration is evaluated (the host settles the
    /// pending decision then, `Host::settle_trust`).
    pub fn before_config(cli: &serde_json::Value) -> Result<Self, String> {
        let state = crate::read_state()?;
        let from = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).and_then(Self::parse);
        Ok(from(cli.get("trust_default"))
            .or_else(|| std::env::var("MARE_TRUST_DEFAULT").ok().as_deref().and_then(Self::parse))
            .or_else(|| from(state.as_ref().and_then(|s| s.get("trust_default"))))
            .unwrap_or_default())
    }
}

/// Resolve trust for `cwd`: a command-line `--trust` / `--no-trust` first; otherwise the
/// saved workspace trust decision (`false` anywhere on `start..=bound`, else `true` on `trust_subject`);
/// otherwise the global default, which is to ask — when protected resources exist, and in the TUI
/// also for any git repository. Asking never happens here: `interactive` (the TUI) gives
/// [`Source::Pending`] and the TUI shows the trust overlay after its first frame;
/// elsewhere nothing protected loads ([`Source::Undecided`]).
/// The plugin layer (`trust/resolve`, between the command line and saved decisions) runs once
/// the user-level plugins have started: `Host::settle_trust`.
pub fn resolve_with(cwd: &Path, interactive: bool, cli: Option<bool>) -> Trust {
    resolve_with_default(cwd, interactive, cli, DefaultTrust::Ask)
}

/// [`resolve_with`] with the global default given: `Trust` / `Distrust` decide a folder that has
/// nothing saved instead of asking ([`Source::Default`]).
pub fn resolve_with_default(cwd: &Path, interactive: bool, cli: Option<bool>, default: DefaultTrust) -> Trust {
    resolve_with_default_at(&mare_shared::paths::trust_file(), cwd, interactive, cli, default)
}

fn resolve_with_default_at(p: &Path, cwd: &Path, interactive: bool, cli: Option<bool>, default: DefaultTrust) -> Trust {
    if let Some(t) = cli {
        return Trust { trusted: t, source: Source::Cli };
    }
    match lookup_at(p, cwd) {
        Ok(Some(d)) => return Trust { trusted: d, source: Source::Saved },
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(path = %p.display(), "trust file is corrupt, failing closed: {err:#}");
            return Trust { trusted: false, source: Source::Saved };
        }
    }
    let by_default = |asked: Source| match default {
        DefaultTrust::Ask => Trust { trusted: false, source: asked },
        DefaultTrust::Trust => Trust { trusted: true, source: Source::Default },
        DefaultTrust::Distrust => Trust { trusted: false, source: Source::Default },
    };
    if protected_resources(cwd).is_empty() {
        // a git repository asks on its first interactive run even without protected resources
        // (an untrusted project starts read-only, and the
        // user gets to choose); headless has nothing to skip, so nothing to report
        if interactive && git_root(cwd).is_some() {
            return by_default(Source::Pending);
        }
        return Trust { trusted: false, source: Source::NotNeeded };
    }
    by_default(if interactive { Source::Pending } else { Source::Undecided })
}

/// The nearest Git worktree containing `cwd`, skipping invalid `.git` markers.
pub fn git_root(cwd: &Path) -> Option<std::path::PathBuf> {
    mare_shared::git::git_root(cwd)
}

/// The protected resources in `cwd`, counted by category for the trust overlay:
/// `(category, count)` — plugins and skills count their entries, the others count files.
pub fn summarize(cwd: &Path) -> Vec<(&'static str, usize)> {
    let entries = |p: &Path| std::fs::read_dir(p).map(|d| d.flatten().count()).unwrap_or(0).max(1);
    let mut cats: Vec<(&'static str, usize)> = vec![];
    let mut add = |name: &'static str, n: usize| match cats.iter_mut().find(|(c, _)| *c == name) {
        Some((_, c)) => *c += n,
        None => cats.push((name, n)),
    };
    for p in protected_resources(cwd) {
        let leaf = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match leaf {
            "plugins" => add("plugins", entries(&p)),
            "plugins.json" => add("plugins", 1),
            "skills" => add("skills", entries(&p)),
            "prompts" => add("prompt templates", entries(&p)),
            "init.lua" => add("project config (init.lua)", 1),
            _ => add("system prompt files", 1),
        }
    }
    cats
}

/// [`resolve_with`] without a command-line override; just the answer.
pub fn resolve(cwd: &Path, interactive: bool) -> bool {
    match DefaultTrust::before_config(&serde_json::Value::Null) {
        Ok(default) => resolve_with_default(cwd, interactive, None, default).trusted,
        Err(e) => {
            tracing::warn!("{e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_dir(dir: &Path) {
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs")).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn git_root_skips_empty_git_directory_to_reach_parent_repository() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let child = repo.join("nested");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(child.join(".git")).unwrap();
        assert_eq!(git_root(&child).as_deref(), Some(repo.as_path()));
    }

    #[test]
    fn git_root_accepts_linked_worktree_past_empty_git_directory() {
        let temp = tempfile::tempdir().unwrap();
        let common = temp.path().join("main/.git");
        git_dir(&common);
        let worktree_git = common.join("worktrees/my worktree");
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::write(worktree_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
        let worktree = temp.path().join("worktree");
        let child = worktree.join("nested");
        std::fs::create_dir_all(child.join(".git")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../main/.git/worktrees/my worktree\n").unwrap();
        assert_eq!(git_root(&child).as_deref(), Some(worktree.as_path()));
    }

    // Git setup.c: an invalid `.git` file ends discovery (GIT_DIR_INVALID_GITFILE, "not a git
    // repository") — the repository around it is not used (stops at an invalid `.git` file too)
    #[test]
    fn git_root_stops_at_an_invalid_gitfile() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let child = repo.join("nested");
        let below = child.join("src");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(&below).unwrap();
        for content in ["not a gitfile\n", "gitdir: missing\n", "gitdir: \n", "gitdir: .\n", "gitdir:../.git\n"] {
            std::fs::write(child.join(".git"), content).unwrap();
            assert_eq!(git_root(&child), None, "{content:?}");
            assert_eq!(git_root(&below), None, "{content:?} (from below)");
        }
    }

    #[test]
    fn git_root_passes_over_invalid_git_directories() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let child = repo.join("nested");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(child.join(".git")).unwrap();
        std::fs::write(child.join(".git/HEAD"), "invalid HEAD\n").unwrap();
        assert_eq!(git_root(&child).as_deref(), Some(repo.as_path()));
        std::fs::write(child.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(git_root(&child).as_deref(), Some(repo.as_path()), "missing objects and refs");
        std::fs::create_dir(child.join(".git/objects")).unwrap();
        assert_eq!(git_root(&child).as_deref(), Some(repo.as_path()), "missing refs");
    }

    #[test]
    fn trust_project_skills_stop_at_an_invalid_gitfile() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let child = repo.join("worktree");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(child.join(".agents/skills")).unwrap();
        std::fs::write(child.join(".git"), "gitdir: ../.git/worktrees/gone\n").unwrap();
        // the walk ends where context/skills_scan.lua stops loading: the worktree, not the outer repo
        assert_eq!(protected_resources(&child.join("src")), vec![child.join(".agents/skills")]);
    }

    #[test]
    fn trust_project_skills_walk_past_empty_git_directory() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let child = repo.join("nested");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(child.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".agents/skills")).unwrap();
        std::fs::create_dir_all(temp.path().join(".agents/skills")).unwrap();
        assert_eq!(protected_resources(&child), vec![repo.join(".agents/skills")]);
    }

    #[test]
    fn decisions_inherit_and_override() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: the only test in this binary that touches MARE_HOME
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("work");
        let child = parent.join("proj");
        std::fs::create_dir_all(&child).unwrap();
        assert_eq!(decision(&child), None);
        // Without a git repo + project marker, an allow on a parent does not apply to child.
        set_dir(&parent, true).unwrap();
        assert_eq!(decision(&child), None, "allow on ancestor without resource root does not apply");
        set_dir(&child, false).unwrap();
        assert_eq!(decision(&child), Some(false), "decision on subject applies");
        assert_eq!(decision(&parent), Some(true));
        // Inside a git repo with a resource root, set() records on the root and start..=root deny overrides it.
        let nested_repo = root.path().join("with-root");
        let nested_sub = nested_repo.join("sub");
        git_dir(&nested_repo.join(".git"));
        std::fs::create_dir_all(nested_repo.join(".mare/prompts")).unwrap();
        std::fs::create_dir_all(&nested_sub).unwrap();
        set(&nested_sub, true).unwrap();
        assert_eq!(decision(&nested_repo), Some(true), "set() from sub records on resource root");
        assert_eq!(decision(&nested_sub), Some(true));
        set_dir(&nested_sub, false).unwrap();
        assert_eq!(decision(&nested_sub), Some(false), "deny on start overrides allow on root");
        assert_eq!(decision(&nested_repo), Some(true));
        // a command-line override beats the saved decision
        assert_eq!(resolve_with(&child, false, Some(true)).source, Source::Cli);
        assert!(resolve_with(&child, false, Some(true)).trusted);
        assert_eq!(resolve_with(&child, false, None), Trust { trusted: false, source: Source::Saved });
        // nothing saved and nobody to ask: undecided, nothing protected loads
        let other = root.path().join("other");
        std::fs::create_dir_all(other.join(".mare")).unwrap();
        assert!(protected_resources(&other).is_empty(), "a bare .mare/ needs no decision");
        std::fs::create_dir_all(other.join(".mare/prompts")).unwrap();
        std::fs::create_dir_all(other.join("sub")).unwrap();
        std::fs::create_dir_all(other.join(".agents/skills")).unwrap();
        let found = protected_resources(&other.join("sub"));
        assert!(found.iter().any(|p| p.ends_with(".agents/skills")), "ancestor .agents/skills count");
        assert_eq!(protected_resources(&other).len(), 2);
        let t = resolve_with(&other, false, None);
        assert_eq!((t.trusted, t.state()), (false, "undecided"), "protected resources, nobody to ask");
        let t = resolve_with(&other, true, None);
        assert_eq!((t.trusted, t.source), (false, Source::Pending), "the TUI asks after its first frame");
        let sum = summarize(&other);
        assert!(sum.contains(&("prompt templates", 1)) && sum.iter().any(|(c, _)| *c == "skills"), "{sum:?}");
        let plain = root.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(resolve_with(&plain, false, None).state(), "none", "nothing protected, nothing saved");
        assert_eq!(resolve_with(&plain, true, None).source, Source::NotNeeded, "not a repository: no question");
        // a new git repository without protected resources: the TUI asks on its first run;
        // headless has nothing to skip and says nothing
        let repo = root.path().join("repo");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(git_root(&repo.join("src")).as_deref(), Some(repo.as_path()));
        assert_eq!(resolve_with(&repo.join("src"), true, None), Trust { trusted: false, source: Source::Pending });
        assert_eq!(resolve_with(&repo.join("src"), false, None).source, Source::NotNeeded);
        // the global default decides a folder with nothing saved (`trust_default`)
        let t = resolve_with_default(&other, false, None, DefaultTrust::Trust);
        assert_eq!((t.trusted, t.source, t.state()), (true, Source::Default, "trusted"));
        let t = resolve_with_default(&other, true, None, DefaultTrust::Distrust);
        assert_eq!((t.trusted, t.source, t.state()), (false, Source::Default, "untrusted"), "no question");
        let t = resolve_with_default(&child, true, None, DefaultTrust::Trust);
        assert_eq!(t.source, Source::Saved, "a saved decision beats the default");
        assert_eq!(resolve_with_default(&plain, true, None, DefaultTrust::Trust).source, Source::NotNeeded);
        assert_eq!(DefaultTrust::parse("Always"), Some(DefaultTrust::Trust));
        assert_eq!(DefaultTrust::parse("never"), Some(DefaultTrust::Distrust));
        assert_eq!(DefaultTrust::parse("maybe"), None);
        set(&repo, false).unwrap();
        assert_eq!(resolve_with(&repo.join("src"), true, None).source, Source::Saved, "answered once: never again");

        // trust.json is versioned, written atomically, keeps unknown fields and rejects unversioned files
        let p = mare_shared::paths::trust_file();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["version"], 1, "{v}");
        assert_eq!(v["decisions"][key(&parent).unwrap()], true);
        let mut v = v;
        v["extra"] = serde_json::json!(1);
        std::fs::write(&p, v.to_string()).unwrap();
        set(&plain, true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["extra"], 1, "unknown fields are kept");
        let flat = serde_json::json!({ key(&plain).unwrap(): false });
        std::fs::write(&p, flat.to_string()).unwrap();
        assert_eq!(decision(&plain), None, "unversioned file is rejected and fails closed");
    }

    #[test]
    fn lock_times_out_when_held_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let trust_path = dir.path().join("state/trust.json");
        let lp = trust_path.with_file_name("trust.json.lock");
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).unwrap();
        holder.lock().unwrap();
        let err = lock_file_with_timeout(&trust_path, std::time::Duration::from_millis(50)).unwrap_err();
        assert!(err.to_string().contains("timed out waiting for lock"), "{err}");
        holder.unlock().unwrap();
    }

    /// A git repo under the user's home: neither the user-level `~/.agents/skills` nor an
    /// `.agents/skills` above the git root makes it a project that needs a trust decision.
    #[test]
    fn user_level_skills_and_dirs_above_the_git_root_are_not_project_resources() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: no other test in this binary reads HOME
        unsafe { std::env::set_var("HOME", home.path()) };
        std::fs::create_dir_all(home.path().join(".agents/skills")).unwrap();
        let above = home.path().join("work");
        std::fs::create_dir_all(above.join(".agents/skills")).unwrap();
        let repo = above.join("repo");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert!(protected_resources(&repo.join("src")).is_empty(), "{:?}", protected_resources(&repo.join("src")));
        // inside the repo, project skills still count
        std::fs::create_dir_all(repo.join(".agents/skills")).unwrap();
        assert_eq!(protected_resources(&repo.join("src")).len(), 1);
    }

    #[test]
    fn concurrent_sets_preserve_all_trust_and_distrust_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let trust_path = dir.path().join("state/trust.json");
        let n = 16;
        let projects: Vec<_> = (0..n)
            .map(|i| {
                let p = dir.path().join(format!("proj-{i}"));
                std::fs::create_dir_all(&p).unwrap();
                p
            })
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
        let handles: Vec<_> = projects
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let trust_path = trust_path.clone();
                let p = p.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    set_at(&trust_path, &p, i % 2 == 0).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        for (i, p) in projects.iter().enumerate() {
            assert_eq!(decision_at(&trust_path, p), Some(i % 2 == 0), "lost concurrent decision for proj-{i}");
        }
    }

    #[test]
    fn corrupt_trust_file_fails_closed_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(".mare/prompts")).unwrap();
        let trust_path = dir.path().join("state/trust.json");
        std::fs::create_dir_all(trust_path.parent().unwrap()).unwrap();

        // Missing file still uses the configured default.
        assert_eq!(
            resolve_with_default_at(&trust_path, &proj, false, None, DefaultTrust::Trust),
            Trust { trusted: true, source: Source::Default }
        );

        let corrupt_cases = [
            "",
            "   \n",
            "{not json",
            "[]",
            "null",
            r#"{"version":1}"#,
            r#"{"version":1,"decisions":"invalid"}"#,
            r#"{"version":1,"decisions":[]}"#,
            r#"{"version":1,"decisions":{"/a":"yes"}}"#,
            r#"{"version":2,"decisions":{}}"#,
            r#"{"version":"1","decisions":{}}"#,
            r#"{"/a":"yes"}"#,
        ];

        for raw in corrupt_cases {
            std::fs::write(&trust_path, raw).unwrap();
            assert!(load_file_at(&trust_path).is_err(), "expected error for {raw:?}");
            assert!(set_at(&trust_path, &proj, true).is_err(), "must not overwrite {raw:?}");
            assert_eq!(std::fs::read_to_string(&trust_path).unwrap(), raw, "corrupt file modified for {raw:?}");
            assert_eq!(decision_at(&trust_path, &proj), None, "decision_at for {raw:?}");

            for interactive in [false, true] {
                let t = resolve_with_default_at(&trust_path, &proj, interactive, None, DefaultTrust::Trust);
                assert!(!t.trusted, "trust_default=trust must fail closed for {raw:?} (interactive={interactive})");
                assert_eq!(t.state(), "untrusted", "state for {raw:?}");
                assert_eq!(t.source, Source::Saved, "source for {raw:?} must not trigger default");
            }
        }
    }

    #[test]
    fn deny_on_start_dir_beats_allow_on_root() {
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let mid = repo.join("private");
        let leaf = mid.join("sub");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&leaf).unwrap();

        set_dir(&repo, true).unwrap();
        set_dir(&mid, false).unwrap();
        // Calling set() from leaf records on the root, not leaf, so mid's deny on start..=root still wins.
        set(&leaf, true).unwrap();
        assert_eq!(decision(&leaf), Some(false));
    }

    #[test]
    fn old_allow_on_subdir_does_not_trust_root_above() {
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let sub = repo.join("sub");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();

        set_dir(&sub, true).unwrap();
        assert_eq!(decision(&sub), None, "allow recorded on subdir must not trust repo root");
    }

    #[test]
    fn allow_above_root_does_not_count() {
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let tmp = tempfile::tempdir().unwrap();
        let above = dunce::canonicalize(tmp.path()).unwrap().join("work");
        let repo = above.join("repo");
        let sub = repo.join("sub");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();

        set_dir(&above, true).unwrap();
        assert_eq!(decision(&sub), None, "allow above resource root must not apply");
    }

    #[test]
    fn set_from_subdir_records_on_root_and_set_dir_records_exact() {
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let sub1 = repo.join("sub1");
        let sub2 = repo.join("sub2");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(repo.join(".mare")).unwrap();
        std::fs::create_dir_all(&sub1).unwrap();
        std::fs::create_dir_all(&sub2).unwrap();

        set(&sub1, true).unwrap();
        assert_eq!(decision(&sub2), Some(true), "set() from sub1 recorded allow on repo root");
        let exact_dir = repo.join("standalone");
        std::fs::create_dir_all(exact_dir.join(".mare")).unwrap();
        set_dir(&exact_dir, false).unwrap();
        assert_eq!(decision(&exact_dir), Some(false));
    }

    #[test]
    fn deny_on_git_root_overrides_nested_resource_root() {
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        let sub = repo.join("sub");
        let leaf = sub.join("leaf");
        git_dir(&repo.join(".git"));
        std::fs::create_dir_all(sub.join(".mare/prompts")).unwrap();
        std::fs::create_dir_all(&leaf).unwrap();

        set_dir(&sub, true).unwrap();
        set_dir(&repo, false).unwrap();
        assert_eq!(decision(&leaf), Some(false), "deny recorded on git root must override nested resource root");
    }

    #[test]
    fn plugins_json_is_protected_and_summarized() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = dunce::canonicalize(tmp.path()).unwrap().join("proj");
        std::fs::create_dir_all(proj.join(".mare")).unwrap();
        std::fs::write(proj.join(".mare/plugins.json"), "{}").unwrap();
        let protected = protected_resources(&proj);
        assert!(protected.iter().any(|p| p.ends_with(".mare/plugins.json")), "{protected:?}");
        let summary = summarize(&proj);
        assert!(summary.iter().any(|(cat, n)| *cat == "plugins" && *n >= 1), "{summary:?}");
    }
}
