//! Git operations on one checkout: safe clone, origin repointing, resolving a target, checkout,
//! fetch, local edits, new commits.

use crate::*;

/// Remove now-empty directories from `p`'s parent up to the packages root.
pub(crate) fn prune_empty_parents(p: &Path) {
    let root = mare_shared::paths::packages_dir();
    let mut cur = p.parent();
    while let Some(d) = cur {
        if d == root || !d.starts_with(&root) || std::fs::remove_dir(d).is_err() {
            break;
        }
        cur = d.parent();
    }
}

/// A single git status snapshot shared by update, restore, clean, reporting, and hashing.
pub(crate) struct WorktreeStatus {
    pub modified_or_staged: bool,
    pub untracked: bool,
    pub ignored: bool,
    pub error: Option<String>,
    entries: String,
}

impl WorktreeStatus {
    pub(crate) fn has_changes(&self) -> bool {
        self.modified_or_staged || self.untracked || self.error.is_some()
    }

    pub(crate) fn is_clean(&self) -> bool {
        !self.has_changes() && !self.ignored
    }
}

pub(crate) fn worktree_status(o: &PkgOptions, dir: &Path) -> WorktreeStatus {
    let (entries, error) = match o.git(Some(dir), &["status", "--porcelain", "--ignored", "--untracked-files=all"]) {
        Ok(entries) => (entries, None),
        Err(err) => (String::new(), Some(format!("{err:#}"))),
    };
    let mut state = WorktreeStatus { modified_or_staged: false, untracked: false, ignored: false, error, entries };
    for line in state.entries.lines() {
        if line.starts_with("??") {
            state.untracked = true;
        } else if line.starts_with("!!") {
            state.ignored = true;
        } else if !line.is_empty() {
            state.modified_or_staged = true;
        }
    }
    state
}

/// Flag local work without hiding a failure to inspect the checkout.
pub(crate) fn flag_local_changes(state: &WorktreeStatus, log: Log) {
    if let Some(err) = &state.error {
        log(format!("  ⚠ could not inspect local changes: {err}"));
    } else if state.has_changes() {
        log("  ⚠ local changes (kept; the package loads with them):".into());
        for l in state.entries.lines().filter(|line| !line.starts_with("!!")).take(10) {
            log(format!("    {l}"));
        }
    }
}

/// A clone / fetch failure with what to check: credentials are never prompted for, so a private
/// repository the system git can't authenticate to fails right away.
pub(crate) fn remote_error(e: anyhow::Error, url: &str) -> anyhow::Error {
    if e.downcast_ref::<git::AbortError>().is_some() {
        return e;
    }
    let msg = mare_shared::redact::redact_str(&format!("{e:#}"));
    let safe_url = mare_shared::redact::redact_str(url);
    if git::is_auth_failure(&msg) {
        anyhow::anyhow!(
            "authentication failed (mare never prompts for credentials; the system git's credential helper / ssh agent must have them) — check with `git ls-remote {safe_url}`: {msg}"
        )
    } else {
        anyhow::anyhow!("{msg} — check with `git ls-remote {safe_url}`")
    }
}

pub(crate) fn stop_if_cancelled() -> Result<()> {
    if cancelled() {
        return Err(git::AbortError::Cancelled { what: None }.into());
    }
    Ok(())
}

/// Do not move a checkout away from the commit another scope has locked.
pub(crate) fn ensure_other_scopes_allow(
    o: &PkgOptions,
    s: &PackageSpec,
    dir: &Path,
    lockfile: &Path,
    target: &str,
) -> Result<()> {
    if target.starts_with('-') || target.contains(['\0', '\n', '\r']) {
        bail!("{}: invalid revision {target:?}", s.name);
    }
    let revision = format!("{target}^{{commit}}");
    let commit = match o.git(Some(dir), &["rev-parse", "--verify", "--end-of-options", &revision]) {
        Ok(commit) => commit,
        Err(_) => {
            fetch(o, dir)?;
            o.git(Some(dir), &["rev-parse", "--verify", "--end-of-options", &revision])?
        }
    };
    if !crate::lock::is_valid_commit_sha(&commit) {
        bail!("{}: resolved revision is not a valid commit hash: {commit:?}", s.name);
    }
    if let Some(other) = crate::lock::other_scope_reference(dir, lockfile, Some(&commit), true)? {
        bail!("{}: checkout conflict with locked project {}", s.name, other.display());
    }
    Ok(())
}

/// Snapshot of a checkout directory before an install/update mutation, used to roll back
/// partial changes on failure.
pub(crate) struct PreCheckout {
    pub(crate) dir: PathBuf,
    pub(crate) dir_existed: bool,
    pub(crate) has_git: bool,
    pub(crate) head: Option<String>,
    pub(crate) origin: Option<String>,
}

impl PreCheckout {
    pub(crate) fn capture(o: &PkgOptions, dir: PathBuf) -> Self {
        let dir_existed = dir.exists();
        let has_git = mare_shared::git::git_dir(&dir).is_some();
        let head = has_git.then(|| head(o, &dir)).flatten();
        let origin = has_git.then(|| configured_origin(o, &dir).ok()).flatten();
        Self { dir, dir_existed, has_git, head, origin }
    }

    pub(crate) fn rollback(&self, o: &PkgOptions, submodules: bool, log: Log) -> Result<()> {
        let fresh = tokio_util::sync::CancellationToken::new();
        with_cancel(&fresh, || {
            if !self.dir_existed {
                if self.dir.exists() {
                    std::fs::remove_dir_all(&self.dir)
                        .with_context(|| format!("removing partial checkout {}", self.dir.display()))?;
                    prune_empty_parents(&self.dir);
                    log("  ⎿ rolled back (removed partial checkout)".into());
                }
                return Ok(());
            }
            if !self.has_git {
                return Ok(());
            }
            if let Some(orig) = &self.origin {
                let current = configured_origin(o, &self.dir).ok();
                if current.as_deref() != Some(orig.as_str()) {
                    o.git(Some(&self.dir), &["remote", "set-url", "origin", "--", orig])
                        .context("restoring origin URL")?;
                }
            }
            if let Some(prev) = &self.head
                && head(o, &self.dir).as_deref() != Some(prev.as_str())
            {
                checkout(o, &self.dir, prev, submodules).with_context(|| format!("restoring {}", short(prev)))?;
                log(format!("  ⎿ rolled back to {}", short(prev)));
            }
            Ok(())
        })
    }
}

/// Clone into a temporary directory next to the checkout, then rename it into place (lazy.nvim
/// clones with a `.cloning` marker; Yazi `git.rs` the same idea): an interrupted clone never
/// looks installed. `core.autocrlf=false`, `--filter=blob:none`, submodules unless
/// `submodules = false` (lazy.nvim `task/git.lua` clone).
fn safe_clone(o: &PkgOptions, s: &PackageSpec, dir: &Path) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let parent = dir.parent().context("checkout directory has no parent")?;
    std::fs::create_dir_all(parent)?;
    let leaf = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "pkg".into());
    let tmp = parent.join(format!(".{leaf}.cloning-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&tmp);
    let tmp_s = tmp.to_string_lossy().into_owned();
    let mut args = vec!["clone", "--quiet", "--filter=blob:none", "--origin=origin", "-c", "core.autocrlf=false"];
    #[cfg(windows)]
    args.extend(["-c", "core.longpaths=true"]);
    if s.submodules {
        args.push("--recurse-submodules");
    }
    args.extend(["--", &s.url, &tmp_s]);
    if let Err(e) = o.git(None, &args) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(remote_error(e, &s.url));
    }
    if mare_shared::git::marker_present(dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("{} has an invalid or newly created .git entry; refusing to replace it", dir.display());
    }
    if dir.exists() {
        // a leftover without .git (an interrupted older clone): replaced
        let _ = std::fs::remove_dir_all(dir);
    }
    std::fs::rename(&tmp, dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp);
    })?;
    Ok(())
}

/// Read the configured URL without applying Git's `url.*.insteadOf` transport rewrites.
pub(crate) fn configured_origin(o: &PkgOptions, dir: &Path) -> Result<String> {
    o.git(Some(dir), &["config", "--get", "remote.origin.url"])
}

/// Clone if missing; if the checkout's `origin` is not the configured URL (the source changed in
/// init.lua, e.g. https ↔ ssh of the same repository), repoint it and fetch.
/// Returns (dir, source_changed).
pub(crate) fn ensure_clone(o: &PkgOptions, s: &PackageSpec, log: Log) -> Result<(PathBuf, bool)> {
    ensure_clone_at(o, s, &checkout_dir(&s.url), log)
}

pub(crate) fn ensure_clone_at(o: &PkgOptions, s: &PackageSpec, dir: &Path, log: Log) -> Result<(PathBuf, bool)> {
    source::check(&s.url, o.allow_insecure)?;
    if source::climbs_out(&s.url) {
        bail!("{}: a `..` path segment would put the checkout outside the packages directory", s.url);
    }
    if !source::under_packages(dir) {
        bail!("{}: checkout directory {} is outside the packages directory", s.url, dir.display());
    }
    if mare_shared::git::git_dir(dir).is_none() {
        if mare_shared::git::marker_present(dir) {
            bail!("{} has an invalid .git entry; refusing to replace it", dir.display());
        }
        log(format!("  cloning {} …", mare_shared::redact::redact_str(&s.url)));
        safe_clone(o, s, dir)?;
        return Ok((dir.to_path_buf(), false));
    }
    let origin = configured_origin(o, dir).unwrap_or_default();
    if origin != s.url {
        log(format!(
            "  source changed: {} → {}",
            mare_shared::redact::redact_str(&origin),
            mare_shared::redact::redact_str(&s.url)
        ));
        if o.git(Some(dir), &["remote", "set-url", "origin", "--", &s.url]).is_err() {
            o.git(Some(dir), &["remote", "add", "origin", "--", &s.url])?;
        }
        o.git(Some(dir), &["fetch", "--quiet", "--tags", "--force", "--", "origin"])
            .map_err(|e| remote_error(e, &s.url))?;
        let _ = o.git(Some(dir), &["remote", "set-head", "origin", "--auto"]);
        return Ok((dir.to_path_buf(), true));
    }
    Ok((dir.to_path_buf(), false))
}

/// The default branch tip of `origin` (after `remote set-head --auto`).
fn origin_head(o: &PkgOptions, dir: &Path) -> Result<String> {
    let _ = o.git(Some(dir), &["remote", "set-head", "origin", "--auto"]);
    o.git(Some(dir), &["rev-parse", "refs/remotes/origin/HEAD"])
}

/// The branch recorded in the lock: the declared one, else origin's default branch.
pub(crate) fn branch_of(o: &PkgOptions, dir: &Path, t: &Target) -> Option<String> {
    if let Target::Branch(b) = t {
        return Some(b.clone());
    }
    let r = o.git(Some(dir), &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).ok()?;
    let b = r.strip_prefix("origin/").unwrap_or(&r).to_string();
    (!b.is_empty()).then_some(b)
}

/// Semver tags (`v1.2.3` or `1.2.3`), newest last.
fn semver_tags(o: &PkgOptions, dir: &Path) -> Vec<(semver::Version, String)> {
    let mut tags: Vec<(semver::Version, String)> = o
        .git(Some(dir), &["tag", "--list"])
        .unwrap_or_default()
        .lines()
        .filter_map(|t| semver::Version::parse(t.trim_start_matches('v')).ok().map(|v| (v, t.to_string())))
        .collect();
    tags.sort();
    tags
}

/// The revision a target names in this checkout (fetched).
pub(crate) fn resolve(o: &PkgOptions, dir: &Path, t: &Target) -> Result<String> {
    let check_ref = |r: &str| -> Result<()> {
        if r.is_empty() || r.starts_with('-') || r.contains(['\0', '\n', '\r']) {
            bail!("invalid git ref {r:?}");
        }
        Ok(())
    };
    Ok(match t {
        Target::Commit(c) => {
            check_ref(c)?;
            c.clone()
        }
        Target::Tag(tag) => {
            check_ref(tag)?;
            format!("refs/tags/{tag}")
        }
        Target::Branch(b) => {
            check_ref(b)?;
            format!("refs/remotes/origin/{b}")
        }
        Target::Rev(r) => {
            let rev = r.strip_prefix('=').unwrap_or(r);
            check_ref(rev)?;
            rev.to_string()
        }
        Target::Version(v) => {
            let req =
                semver::VersionReq::parse(v).with_context(|| format!("version = \"{v}\" is not a semver range"))?;
            let tags = semver_tags(o, dir);
            match tags.iter().rev().find(|(ver, _)| req.matches(ver)) {
                Some((_, tag)) => {
                    check_ref(tag)?;
                    format!("refs/tags/{tag}")
                }
                None => bail!("no tag matches version = \"{v}\""),
            }
        }
        Target::Latest => match semver_tags(o, dir).last() {
            Some((_, tag)) => {
                check_ref(tag)?;
                format!("refs/tags/{tag}")
            }
            None => origin_head(o, dir)?,
        },
    })
}

pub(crate) fn checkout(o: &PkgOptions, dir: &Path, rev: &str, submodules: bool) -> Result<String> {
    if rev.is_empty() || rev.starts_with('-') || rev.contains(['\0', '\n', '\r']) {
        bail!("invalid revision {rev:?}");
    }
    o.git(Some(dir), &["checkout", "--quiet", "--detach", "--end-of-options", rev, "--"])?;
    if submodules && dir.join(".gitmodules").exists() {
        o.git(Some(dir), &["submodule", "update", "--init", "--recursive", "--quiet"])?;
    }
    o.git(Some(dir), &["rev-parse", "HEAD"])
}

pub(crate) fn head(o: &PkgOptions, dir: &Path) -> Option<String> {
    o.git(Some(dir), &["rev-parse", "HEAD"]).ok()
}

/// Best-effort fetch (offline is fine: the checkout may already have the revision); a
/// cancellation still stops the operation.
pub(crate) fn fetch(o: &PkgOptions, dir: &Path) -> Result<()> {
    let _ = o.git(Some(dir), &["fetch", "--quiet", "--tags", "--", "origin"]);
    stop_if_cancelled()
}

pub(crate) fn short(c: &str) -> &str {
    &c[..c.len().min(12)]
}

/// The commits between two revisions, newest first (lazy.nvim `task/git.lua` log after an
/// update): at most 20 lines.
pub(crate) fn new_commits(o: &PkgOptions, dir: &Path, from: &str, to: &str) -> Vec<String> {
    if from.starts_with('-') || to.starts_with('-') {
        return vec![];
    }
    let range = format!("{from}..{to}");
    o.git(Some(dir), &["log", "--oneline", "--no-decorate", "--no-color", "-n", "21", "--end-of-options", &range, "--"])
        .map(|s| s.lines().map(str::to_string).collect::<Vec<_>>())
        .map(|mut v| {
            if v.len() > 20 {
                v.truncate(20);
                v.push("…".into());
            }
            v
        })
        .unwrap_or_default()
}
