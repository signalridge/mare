//! A session's files: lock, attachments, snapshots, sub-agent sessions; deleting them all;
//! listing the sessions of a directory; the per-cwd directory name.

use super::*;

/// Every file of a session: the session file, its lock file, attachments, snapshots, and sub-agent sessions (delete).
pub fn session_files(path: &Path) -> Vec<PathBuf> {
    let mut out = single_session_files(path);
    out.extend(child_session_files(path));
    out
}

fn single_session_files(path: &Path) -> Vec<PathBuf> {
    let mut out =
        vec![path.to_path_buf(), lock_path(path), crate::lock::rollback_path(path), crate::lock::commit_path(path)];
    // directories that belong to the session: pasted images and the file snapshot journal
    // (`<id>.attachments`, as compaction's image offload names it; `<id>.jsonl.snapshots`)
    let base = path.with_extension("");
    for d in [
        PathBuf::from(format!("{}.attachments", base.display())),
        PathBuf::from(format!("{}.snapshots", path.display())),
    ] {
        if d.is_dir() {
            out.push(d);
        }
    }
    out
}

/// The sub-agent sessions of a session (`<dir>/agents/<run_id>/*.jsonl` whose header names it
/// as `parent_session_id`). Only matching session files and their own artifacts are
/// included; other files in a run directory are not necessarily owned by this session.
fn child_session_files(path: &Path) -> Vec<PathBuf> {
    let Some(id) = read_header(path).and_then(|h| h.get("session_id").and_then(Value::as_str).map(str::to_string))
    else {
        return vec![];
    };
    let Some(agents) = path.parent().map(|d| d.join("agents")) else { return vec![] };
    if !std::fs::symlink_metadata(&agents).is_ok_and(|m| m.file_type().is_dir()) {
        return vec![];
    }
    let mut out = vec![];
    for run in std::fs::read_dir(&agents).into_iter().flatten().flatten() {
        // Never traverse a symlink supplied in the agents directory.
        if !run.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let run = run.path();
        let mut mine = vec![];
        for f in std::fs::read_dir(&run).into_iter().flatten().flatten() {
            let f = f.path();
            if f.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let belongs = read_header(&f).is_some_and(|h| {
                h.get("type").and_then(Value::as_str) == Some("session")
                    && h.get("session_id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
                    && h.get("parent_session_id").and_then(Value::as_str) == Some(id.as_str())
            });
            if belongs {
                mine.push(f);
            }
        }
        for f in mine {
            out.extend(single_session_files(&f));
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_CHILD_SNAPSHOT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Delete a session and everything that belongs to it ([`session_files`]: lock,
/// attachments, snapshot journal, sub-agent sessions), holding its lock meanwhile:
/// a session open in another process ([`LOCKED`]) or in this one
/// ([`OPEN_HERE`]: close it first) is refused and left alone. Missing pieces are fine; any other
/// failure is reported (with what could not be removed) after trying the rest. The session index
/// is the caller's (a tombstone line, never a rebuild).
pub fn delete_session(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("{NOT_FOUND}: {}", path.display());
    }
    let is_session = || {
        read_header(path).is_some_and(|h| {
            h.get("type").and_then(Value::as_str) == Some("session")
                && h.get("session_id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
        })
    };
    if !is_session() {
        anyhow::bail!("not a session file: {}", path.display());
    }
    let guard = lock_session(path)?;
    if !is_session() {
        anyhow::bail!("not a session file: {}", path.display());
    }
    if let Err(e) = crate::format::check_file_header(path)
        && !e.to_string().contains(UNSUPPORTED_VERSION)
    {
        return Err(e);
    }
    // A child can outlive the active parent session. Hold every child lock before
    // removing anything so deletion cannot unlink a transcript still being written.
    let mut children = child_session_files(path);
    children.sort();
    #[cfg(test)]
    if let Some(hook) = AFTER_CHILD_SNAPSHOT.with(|h| h.borrow_mut().take()) {
        hook();
    }
    let mut child_guards = Vec::new();
    for child in children.iter().filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl") && p.is_file()) {
        child_guards.push(lock_session(child)?);
        if let Err(e) = crate::format::check_file_header(child)
            && !e.to_string().contains(UNSUPPORTED_VERSION)
        {
            return Err(e);
        }
    }
    // Never delete from a later enumeration: only files whose child writers were
    // locked above belong to this deletion. The parent lock excludes conforming
    // child creators; this check also detects an uncoordinated change so far.
    let mut current = child_session_files(path);
    current.sort();
    if current != children {
        anyhow::bail!("child sessions changed while acquiring deletion locks; retry");
    }
    let lock = lock_path(path);
    let mut failed = vec![];
    let mut files = single_session_files(path);
    files.extend(children);
    for f in files {
        if f == lock {
            continue;
        }
        let r = if f.is_dir() { std::fs::remove_dir_all(&f) } else { std::fs::remove_file(&f) };
        if let Err(e) = r
            && e.kind() != std::io::ErrorKind::NotFound
        {
            failed.push(format!("{} ({e})", f.display()));
        }
    }
    drop(child_guards);
    drop(guard);
    let _ = std::fs::remove_file(&lock);
    if let Some(agents) = path.parent().map(|d| d.join("agents"))
        && std::fs::symlink_metadata(&agents).is_ok_and(|m| m.file_type().is_dir())
    {
        for run in std::fs::read_dir(&agents).into_iter().flatten().flatten() {
            let _ = std::fs::remove_dir(run.path()); // only empty run directories
        }
        let _ = std::fs::remove_dir(agents); // only when it became empty
    }
    sync_dir(path)?;
    if !failed.is_empty() {
        anyhow::bail!("could not delete: {}", failed.join(", "));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub header: Option<Value>,
    pub modified: std::time::SystemTime,
    pub size: u64,
}

/// List `*.jsonl` sessions in a directory, newest first.
pub fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    list_sessions_with_unsupported(dir).0
}

/// List sessions and report files whose well-formed session headers use an unsupported version.
pub fn list_sessions_with_unsupported(dir: &Path) -> (Vec<SessionInfo>, Vec<String>) {
    let mut out = Vec::new();
    let mut unsupported = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (out, unsupported);
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let header = read_header(&p);
        if header
            .as_ref()
            .is_some_and(|h| crate::format::check_header(h).is_err_and(|e| e.to_string().contains(UNSUPPORTED_VERSION)))
        {
            unsupported.push(p.to_string_lossy().into_owned());
            continue;
        }
        out.push(SessionInfo {
            header,
            path: p,
            modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            size: meta.len(),
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    unsupported.sort();
    (out, unsupported)
}

/// Directory name for a cwd: `--Users-me-project--`.
pub fn cwd_slug(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    let s = s.trim_start_matches('/');
    let slug: String = s.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '_' { c } else { '-' }).collect();
    format!("--{slug}--")
}
