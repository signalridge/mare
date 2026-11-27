//! Reports: the status rows of `mare plugin list` / `/plugins`, a package's log and diff, `mare doctor`.

use super::find_spec;
use crate::effective::health::blocked;
use crate::effective::specs::validate_checkout_constraints;
use crate::lock::checkout_lock;
use crate::repo::checkout::{head, new_commits, resolve, short, worktree_status};
use crate::*;

/// One row of `mare plugin list` / the `/plugins` panel.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub name: String,
    pub scope: &'static str,
    /// the shown name (`name = …` next to the source), else the name
    pub display: String,
    pub url: String,
    pub dir: String,
    pub local: bool,
    pub installed: bool,
    /// `enabled = false` (or only needed by a disabled package): listed, not installed or loaded
    pub disabled: bool,
    /// what is checked out (HEAD)
    pub commit: Option<String>,
    /// what the lock records
    pub locked: Option<String>,
    pub dirty: bool,
    pub problem: Option<String>,
    pub constraint: Option<String>,
    pub branch: Option<String>,
}

pub fn status(cfg: &serde_json::Value) -> Result<Vec<Status>> {
    let o = PkgOptions::from_config(cfg);
    let lock = read_lock(cfg)?;
    let user_lock = read_lock(&serde_json::json!({}))?;
    let specs = specs(cfg);
    let issues = crate::effective::specs::dependency_issues(cfg);
    let dep_problem = |name: &str| -> Option<String> {
        let mut parts = vec![];
        if let Some(missing) = issues.missing.get(name) {
            parts.push(format!("missing dependency: {}", missing.join(", ")));
        }
        for cyc in &issues.cycles {
            if cyc.iter().any(|n| n == name) {
                parts.push(format!("dependency cycle: {}", cyc.join(" -> ")));
            }
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    };
    let row = |s: PackageSpec, disabled: bool| {
        let dir = s.root();
        let installed = dir.exists();
        let checkout = checkout_dir(&s.url);
        let git_checkout = installed && s.dir.is_none() && mare_shared::git::git_dir(&checkout).is_some();
        let entry = (if s.project_lock { &lock } else { &user_lock }).packages.get(&s.name);
        let mut problem = if installed && !disabled { blocked(&dir) } else { None };
        if !disabled && let Some(dp) = dep_problem(&s.name) {
            problem = Some(match problem {
                Some(p) => format!("{p}; {dp}"),
                None => dp,
            });
        }
        Status {
            scope: s.scope,
            local: s.dir.is_some(),
            installed,
            disabled,
            commit: if git_checkout { head(&o, &checkout) } else { None },
            locked: entry.map(|e| e.commit.clone()),
            branch: entry.and_then(|e| e.branch.clone()),
            dirty: git_checkout && !worktree_status(&o, &checkout).is_clean(),
            problem,
            constraint: s.target.descriptor(),
            dir: dir.to_string_lossy().into_owned(),
            url: s.url,
            display: s.display.unwrap_or_else(|| s.name.clone()),
            name: s.name,
        }
    };
    let mut rows: Vec<Status> = specs.into_iter().map(|s| row(s, false)).collect();
    for s in disabled_specs(cfg) {
        if !rows.iter().any(|r| r.name == s.name) {
            rows.push(row(s, true));
        }
    }
    Ok(rows)
}

pub fn list(cfg: &serde_json::Value, log: Log) -> Result<()> {
    for s in status(cfg)? {
        let state = if s.disabled {
            "disabled"
        } else if s.local {
            "local directory"
        } else if !s.installed {
            "missing (run `mare plugin sync`)"
        } else if s.problem.is_some() {
            "not loaded (see mare doctor)"
        } else if s.dirty {
            "installed, local changes"
        } else if s.commit.is_some() && s.locked.is_some() && s.commit != s.locked {
            "installed, not at the locked commit (`mare plugin restore`)"
        } else {
            "installed"
        };
        let commit = s.commit.as_deref().or(s.locked.as_deref()).map(short).unwrap_or("-");
        log(format!("{:<36} {:<14} {}", s.display, commit, state));
    }
    Ok(())
}

/// Recent commits of one package's checkout, the locked one marked (`/plugins` row action).
pub fn log_of(cfg: &serde_json::Value, name: &str, log: Log) -> Result<()> {
    let o = PkgOptions::from_config(cfg);
    let s = find_spec(cfg, name)?;
    let lock = crate::lock::read_spec_lock(cfg, &s)?;
    let dir = checkout_dir(&s.url);
    if mare_shared::git::git_dir(&dir).is_none() {
        bail!("{name} is not installed");
    }
    let locked = lock.packages.get(name).map(|e| e.commit.clone());
    let out = o.git(
        Some(&dir),
        &["log", "--no-color", "--format=%H %h %s (%cr)", "-n", "30", "--end-of-options", "HEAD", "--"],
    )?;
    for l in out.lines() {
        let (full, rest) = l.split_once(' ').unwrap_or((l, ""));
        let mark = if locked.as_deref() == Some(full) { " ← lock" } else { "" };
        log(format!("{rest}{mark}"));
    }
    Ok(())
}

/// What changed in one package's checkout: local edits, and the locked commit → HEAD
/// (`/plugins` row action).
pub fn diff_of(cfg: &serde_json::Value, name: &str, log: Log) -> Result<()> {
    let o = PkgOptions::from_config(cfg);
    let s = find_spec(cfg, name)?;
    let lock = crate::lock::read_spec_lock(cfg, &s)?;
    let dir = checkout_dir(&s.url);
    if mare_shared::git::git_dir(&dir).is_none() {
        bail!("{name} is not installed");
    }
    let local = o.git(Some(&dir), &["diff", "--stat", "--no-color", "--end-of-options", "HEAD", "--"])?;
    if !local.is_empty() {
        log("local changes:".into());
        local.lines().for_each(|l| log(format!("  {l}")));
    }
    let entry = lock.packages.get(name);
    let current = head(&o, &dir);
    if let (Some(e), Some(h)) = (entry, current.as_ref())
        && e.commit != *h
    {
        log(format!("lock {} → HEAD {}:", short(&e.commit), short(h)));
        if !e.commit.starts_with('-') && !h.starts_with('-') {
            let range = format!("{}..{h}", e.commit);
            let d = o
                .git(Some(&dir), &["diff", "--stat", "--no-color", "--end-of-options", &range, "--"])
                .unwrap_or_default();
            d.lines().for_each(|l| log(format!("  {l}")));
        }
    }
    match (entry, current) {
        (None, _) => log("not in the lock (run `mare plugin sync`)".into()),
        (Some(e), Some(h)) if local.is_empty() && e.commit == h => {
            log("no changes (at the locked commit, no local edits)".into());
        }
        _ => {}
    }
    Ok(())
}

/// `mare doctor` checks: git present and its version, a plink `GIT_SSH` without
/// `-batch`, the lock parses.
/// `(name, ok, detail)`.
pub fn doctor() -> Vec<(String, bool, String)> {
    doctor_base(&serde_json::json!({}))
}

fn doctor_base(cfg: &serde_json::Value) -> Vec<(String, bool, String)> {
    let mut out = vec![];
    match git::run(None, &["--version"], Duration::from_secs(10), false) {
        Ok(v) => {
            let ver = v.split_whitespace().nth(2).unwrap_or("").to_string();
            let parts: Vec<u64> = ver.split('.').take(2).filter_map(|p| p.parse().ok()).collect();
            // --filter=blob:none needs 2.19, `remote set-head --auto` and symbolic refs are older
            let ok = parts.len() == 2 && (parts[0], parts[1]) >= (2, 19);
            let detail = if ok { v.clone() } else { format!("{v} — packages need git 2.19 or newer") };
            out.push(("git".into(), ok, detail));
        }
        Err(e) => out.push(("git".into(), false, format!("not usable ({e:#}): packages can't be installed"))),
    }
    out.extend(git::ssh_doctor());
    let lock = lock_path(cfg);
    match read_lock(cfg) {
        Ok(l) if lock.exists() => {
            out.push(("package lock".into(), true, format!("{} ({} packages)", lock.display(), l.packages.len())))
        }
        Ok(_) => out.push(("package lock".into(), true, "none yet (created by the first sync)".into())),
        Err(e) => out.push(("package lock".into(), false, format!("{e:#}"))),
    }
    match read_extras(cfg) {
        Ok(_) => {}
        Err(e) => out.push(("plugins.json".into(), false, format!("{e:#}"))),
    }
    out
}

/// `mare doctor` checks including package dependency missing/cycle checks for `cfg`.
pub fn doctor_for(cfg: &serde_json::Value) -> Vec<(String, bool, String)> {
    let mut out = doctor_base(cfg);
    out.extend(
        crate::extras::enabled::implicit_extra_specs(cfg)
            .1
            .into_iter()
            .map(|notice| ("plugin selection".into(), false, notice)),
    );
    let issues = crate::effective::specs::dependency_issues(cfg);
    if !issues.missing.is_empty() {
        let detail = issues
            .missing
            .iter()
            .map(|(pkg, deps)| format!("{pkg} needs {}", deps.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        out.push(("package dependencies".into(), false, format!("missing: {detail}")));
    }
    if !issues.cycles.is_empty() {
        let detail = issues.cycles.iter().map(|c| c.join(" -> ")).collect::<Vec<_>>().join("; ");
        out.push(("package dependency cycles".into(), false, detail));
    }
    if issues.missing.is_empty() && issues.cycles.is_empty() && !specs(cfg).is_empty() {
        out.push(("package dependencies".into(), true, "all declared dependencies present, no cycles".into()));
    }
    let options = PkgOptions::from_config(cfg);
    let all = specs(cfg);
    let known: Vec<PathBuf> =
        all.iter().filter(|spec| spec.dir.is_none()).map(|spec| checkout_dir(&spec.url)).collect();
    for spec in all.iter().filter(|spec| spec.dir.is_none()) {
        let lock = crate::lock::read_spec_lock(cfg, spec).ok();
        let dir = checkout_dir(&spec.url);
        if mare_shared::git::git_dir(&dir).is_none() {
            continue;
        }
        if let Some(entry) = lock.as_ref().and_then(|lock| lock.packages.get(&spec.name)) {
            if let Some(current) = head(&options, &dir)
                && current != entry.commit
            {
                out.push((
                    spec.name.clone(),
                    false,
                    format!("HEAD {} differs from locked {}", short(&current), short(&entry.commit)),
                ));
            }
        } else {
            out.push((spec.name.clone(), false, "not in the lock".into()));
        }
        match crate::repo::checkout::configured_origin(&options, &dir) {
            Ok(origin) if origin != spec.url => out.push((
                spec.name.clone(),
                false,
                format!(
                    "origin {} differs from {}",
                    mare_shared::redact::redact_str(&origin),
                    mare_shared::redact::redact_str(&spec.url)
                ),
            )),
            Err(err) => out.push((spec.name.clone(), false, format!("cannot read origin: {err:#}"))),
            _ => {}
        }
        let state = worktree_status(&options, &dir);
        if !state.is_clean() {
            out.push((
                spec.name.clone(),
                false,
                "local changes, untracked or ignored files, or unreadable git status".into(),
            ));
        }
    }
    fn orphans(dir: &Path, known: &[PathBuf], own: &Path, out: &mut Vec<(String, bool, String)>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_dir())
                || path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
            {
                continue;
            }
            if mare_shared::git::git_dir(&path).is_some() {
                if !known.contains(&path)
                    && matches!(crate::lock::other_scope_reference(&path, own, None, false), Ok(None))
                {
                    out.push(("orphaned checkout".into(), false, path.display().to_string()));
                }
            } else if !mare_shared::git::marker_present(&path) {
                orphans(&path, known, own, out);
            }
        }
    }
    orphans(&mare_shared::paths::packages_dir(), &known, &lock_path(cfg), &mut out);
    for diagnostic in diagnostics(cfg) {
        out.push((diagnostic.plugin, false, format!("{}: {}", diagnostic.path, diagnostic.message)));
    }
    let cwd = cfg
        .get(PROJECT_SCOPE_KEY)
        .and_then(serde_json::Value::as_str)
        .and_then(|dir| Path::new(dir).parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let (Ok(enabled), Ok(catalog)) = (extras_for(&cwd, cfg), catalog_for(cfg)) {
        for entry in enabled.iter().filter(|entry| entry.enabled) {
            if let Some(recommended) = catalog
                .iter()
                .find(|item| item.name == entry.name && item.repo == entry.source)
                .and_then(|item| item.manifest.recommended.as_ref())
            {
                for cmd in &recommended.cmd {
                    if !crate::extras::enabled::on_path(cmd) {
                        out.push((entry.name.clone(), false, format!("recommended command {cmd} is not on PATH")));
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheck {
    pub name: String,
    pub status: &'static str,
    pub behind_count: u64,
    pub commits: Vec<String>,
    pub breaking: bool,
    pub error: Option<String>,
}

/// Fetch and compare declared packages. A failed fetch is reported as unreachable, never current.
pub fn check(cfg: &serde_json::Value, names: &[String]) -> Result<Vec<UpdateCheck>> {
    let _ = read_lock(cfg)?;
    let specs = specs(cfg);
    validate_checkout_constraints(&specs)?;
    for name in names {
        if !specs.iter().any(|spec| spec.name == *name) {
            bail!("no declared package named {name}");
        }
    }
    let o = PkgOptions::from_config(cfg);
    let mut out = Vec::new();
    for spec in specs.into_iter().filter(|spec| names.is_empty() || names.contains(&spec.name)) {
        let mut row = UpdateCheck {
            name: spec.name.clone(),
            status: "up_to_date",
            behind_count: 0,
            commits: Vec::new(),
            breaking: false,
            error: None,
        };
        if spec.dir.is_some() {
            row.status = "local";
        } else if spec.exact_pin() {
            row.status = "pinned";
        } else {
            let dir = checkout_dir(&spec.url);
            let result = (|| -> Result<()> {
                let _guard = checkout_lock(&dir)?;
                if mare_shared::git::git_dir(&dir).is_none() {
                    bail!("checkout {} is not installed", dir.display());
                }
                o.git(Some(&dir), &["fetch", "--quiet", "--tags", "--", "origin"])?;
                let target = resolve(&o, &dir, &spec.target)?;
                let current = head(&o, &dir).context("cannot read checkout HEAD")?;
                if current.starts_with('-') || target.starts_with('-') {
                    bail!("invalid revision range for {}", spec.name);
                }
                let range = format!("{current}..{target}");
                row.behind_count =
                    o.git(Some(&dir), &["rev-list", "--count", "--end-of-options", &range, "--"])?.parse()?;
                row.commits = new_commits(&o, &dir, &current, &target);
                if row.behind_count > 0 {
                    row.status = "outdated";
                    let messages = o.git(Some(&dir), &["log", "--format=%s%n%b", "--end-of-options", &range, "--"])?;
                    row.breaking = messages.lines().any(|line| {
                        line.starts_with("BREAKING CHANGE:")
                            || line.split_once(':').is_some_and(|(prefix, _)| prefix.ends_with('!'))
                    });
                }
                Ok(())
            })();
            if let Err(err) = result {
                row.status = "unreachable";
                row.error = Some(mare_shared::redact::redact_str(&format!("{err:#}")));
            }
        }
        out.push(row);
    }
    Ok(out)
}
