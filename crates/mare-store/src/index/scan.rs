//! A session's index row, derived from its JSONL file alone (the file is the only source of
//! truth; the index can always be rebuilt from it). A full parse reads every line; a catch-up
//! parses only the bytes appended after the row's watermark (`indexed_len`).

use std::borrow::Cow;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mare_protocol::session_records::{Content, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::record::{Archive, CoreRecordType, Label, MessageRecord, Name, SessionHeader};

/// Keep shown fields short and single-line (titles and previews come from user text).
const MAX_FIELD: usize = 160;

/// One `sessions` row (see the schema in `mod.rs`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub session_id: String,
    pub path: String,
    /// "main" | "fork" | "agent"
    pub kind: String,
    pub parent_session_id: Option<String>,
    pub run_id: Option<String>,
    pub cwd: String,
    pub project_root: Option<String>,
    pub title: Option<String>,
    pub preview: Option<String>,
    /// the latest own prompt (one line), kept current by each catch-up
    pub last_input: Option<String>,
    pub labels: Vec<String>,
    /// ms since the epoch; `None` = not archived
    pub archived_at_ms: Option<i64>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub git_branch: Option<String>,
    /// ms since the epoch
    pub created_at_ms: i64,
    /// last message activity, ms since the epoch
    pub updated_at_ms: i64,
    pub seq: i64,
    pub message_count: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub format_version: i64,
    /// reconcile watermark: the file's length (at a line boundary) and mtime (ns) when parsed
    pub indexed_len: i64,
    pub indexed_mtime: i64,
    /// physical file identity at the watermark (device/inode on Unix, volume/file index on Windows)
    pub indexed_dev: i64,
    pub indexed_ino: i64,
    /// identity and length of `<session>.lock`; its length is even when stable and odd while a
    /// batch rollback is in progress or awaiting journal reopen recovery
    pub indexed_lock_dev: i64,
    pub indexed_lock_ino: i64,
    pub indexed_lock_len: i64,
    /// resume checkpoint
    pub verified_len: Option<i64>,
    pub verified_mtime: Option<i64>,
}

impl Row {
    /// The Lua-facing `mare.store.IndexRow` shape.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "session_id": self.session_id,
            "path": self.path,
            "kind": self.kind,
            "cwd": self.cwd,
            "created_at_ms": self.created_at_ms,
            "updated_at_ms": self.updated_at_ms,
            "seq": self.seq,
            "messages": self.message_count,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "archived": self.archived_at_ms.is_some(),
        });
        let m = v.as_object_mut().expect("object");
        let mut opt = |k: &str, x: &Option<String>| {
            if let Some(s) = x {
                m.insert(k.into(), Value::String(s.clone()));
            }
        };
        opt("parent_session_id", &self.parent_session_id);
        opt("run_id", &self.run_id);
        opt("project_root", &self.project_root);
        opt("title", &self.title);
        opt("preview", &self.preview);
        opt("last_input", &self.last_input);
        opt("model", &self.model);
        opt("provider", &self.provider);
        opt("git_branch", &self.git_branch);
        if !self.labels.is_empty() {
            m.insert("labels".into(), json!(self.labels));
        }
        if let Some(a) = self.archived_at_ms {
            m.insert("archived_at_ms".into(), json!(a));
        }
        v
    }
}

/// The path as stored and compared: components re-joined, so `a//b`, `a/./b` and (on Windows)
/// `a/b` vs `a\b` name one row.
pub fn norm(p: &Path) -> String {
    p.components().collect::<PathBuf>().to_string_lossy().into_owned()
}

/// Where a file sits under the sessions root: `(agent, legacy_archive_dir)`.
/// `<root>/<slug>/agents/<run_id>/<file>` is a sub-agent's session; `<root>/<slug>/archive/<file>`
/// is a session archived by moving it (before archive entries existed).
fn place(path: &Path) -> (bool, bool) {
    let named = |d: Option<&Path>, n: &str| d.and_then(Path::file_name).is_some_and(|x| x == n);
    let dir = path.parent();
    (named(dir.and_then(Path::parent), "agents"), named(dir, "archive"))
}

/// The nearest Git worktree containing `cwd`, else `cwd`.
pub fn project_root(cwd: &str) -> String {
    mare_shared::git::git_root(Path::new(cwd)).map_or_else(|| cwd.to_string(), |d| d.to_string_lossy().into_owned())
}

/// The checked-out branch of the repository at `root` (`.git/HEAD`, following a worktree's
/// `.git` file); `None` when detached or not a repository.
pub fn git_branch(root: &str) -> Option<String> {
    let git_dir = mare_shared::git::git_dir(Path::new(root))?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

pub fn one_line(s: &str) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= MAX_FIELD {
        return s;
    }
    let mut out: String = s.chars().take(MAX_FIELD).collect();
    out.push('…');
    out
}

fn truthy(v: &Option<Value>) -> bool {
    !matches!(v, None | Some(Value::Null) | Some(Value::Bool(false)))
}

/// Parse state carried across lines.
#[derive(Default)]
struct Acc {
    labels: Vec<(String, String)>,
    archive_entry: Option<Option<i64>>,
    new_messages: i64,
    saw_label: bool,
}

/// The outcome of a catch-up.
pub enum Progress {
    /// parsed what was appended: this many new message entries
    Grew(i64),
    /// the file can't be continued from the watermark (shrank, rewritten, a label changed):
    /// parse it in full
    NeedFull,
}

const SOURCE_READ_TRIES: usize = 2;

/// Persistent rollback epoch stored by extending the existing session lock file. Missing or empty
/// lock files have revision zero. An odd revision with a live session lock is pending; an odd
/// revision without a writer is parsed in full until the next SessionFile open repairs the marker.
pub(super) fn rollback_revision(path: &Path) -> Result<(i64, i64, i64)> {
    if crate::lock::rollback_pending(path)? {
        anyhow::bail!("{}: {}", crate::ROLLBACK_IN_PROGRESS, path.display());
    }
    let stamp = match crate::stamp(&crate::lock_path(path)) {
        Ok(stamp) if stamp.len > 0 => stamp,
        Ok(_) => return Ok((0, 0, 0)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0, 0)),
        Err(error) => return Err(error.into()),
    };
    if stamp.len % 2 == 1 && crate::lock::session_locked(path)? {
        anyhow::bail!("{}: {}", crate::ROLLBACK_IN_PROGRESS, path.display());
    }
    Ok((stamp.dev as i64, stamp.ino as i64, stamp.len as i64))
}

/// Stable path generation used when a parse has no `Row` to carry its identity (for example an
/// unsupported header). Bind the file stamp and rollback revision to the same observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SourceGeneration {
    pub stamp: crate::FileStamp,
    pub rollback: (i64, i64, i64),
}

pub(super) fn source_generation(path: &Path) -> Result<SourceGeneration> {
    for _ in 0..SOURCE_READ_TRIES {
        let before = crate::stamp(path)?;
        let rollback_before = rollback_revision(path)?;
        let after = crate::stamp(path)?;
        let rollback_after = rollback_revision(path)?;
        if before == after && rollback_before == rollback_after {
            return Ok(SourceGeneration { stamp: after, rollback: rollback_after });
        }
    }
    anyhow::bail!("session file {} changed while its generation was observed; try again", path.display())
}

#[cfg(test)]
thread_local! {
    static READ_FROM_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static AFTER_READ_FROM_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn reset_read_from_opens() {
    READ_FROM_OPENS.set(0);
}

#[cfg(test)]
pub(super) fn read_from_opens() -> usize {
    READ_FROM_OPENS.get()
}

#[cfg(all(test, unix))]
pub(super) fn after_next_read_from_open(hook: impl FnOnce() + 'static) {
    AFTER_READ_FROM_OPEN.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn read_from_opened() {
    READ_FROM_OPENS.set(READ_FROM_OPENS.get() + 1);
    AFTER_READ_FROM_OPEN.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

/// Read a complete header and its handle identity, retrying if the path was replaced mid-read.
/// Bind bytes and metadata to one physical
/// generation rather than trusting a path, size, or modification time alone.
fn read_header_snapshot(path: &Path) -> Result<(Value, crate::FileStamp, (i64, i64, i64))> {
    for _ in 0..SOURCE_READ_TRIES {
        let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let before = crate::stable::stamp_file(&f)?;
        let rollback_before = rollback_revision(path)?;
        let mut line = String::new();
        let read = {
            let mut reader = std::io::BufReader::new(&mut f);
            reader.read_line(&mut line)
        };
        let header = read.map_err(anyhow::Error::from).and_then(|_| {
            if !line.ends_with('\n') {
                anyhow::bail!("{}: missing complete session header in {}", crate::CORRUPT, path.display());
            }
            serde_json::from_str(&line)
                .with_context(|| format!("{}: invalid session header in {}", crate::CORRUPT, path.display()))
        });
        let after = crate::stable::stamp_file(&f)?;
        let named = crate::stamp(path).ok();
        let rollback_after = rollback_revision(path)?;
        if (before.dev, before.ino) != (after.dev, after.ino)
            || !named.is_some_and(|now| (now.dev, now.ino) == (before.dev, before.ino))
            || rollback_before != rollback_after
        {
            continue;
        }
        let header = header?;
        crate::format::check_header(&header)?;
        return Ok((header, before, rollback_after));
    }
    anyhow::bail!("session file {} changed while its header was read; try again", path.display())
}

struct StreamedScan {
    bytes_read: u64,
    stamp: crate::FileStamp,
    at_line_start: bool,
    rollback_after: (i64, i64, i64),
    aborted: bool,
}

/// Stream complete `\n`-terminated lines of `path` from `from`, bound to one physical file
/// generation. Calls `on_event(None)` at the start of each attempt (in case a concurrent
/// replacement forces a retry) and `on_event(Some(raw))` for each non-empty complete line.
fn stream_from<F>(path: &Path, from: u64, check_rollback: bool, mut on_event: F) -> Result<StreamedScan>
where
    F: FnMut(Option<&[u8]>) -> std::ops::ControlFlow<()>,
{
    let mut line = Vec::new();
    for _ in 0..SOURCE_READ_TRIES {
        let _ = on_event(None);
        let rollback_before = if check_rollback { Some(rollback_revision(path)?) } else { None };
        let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        #[cfg(test)]
        read_from_opened();
        let before = crate::stable::stamp_file(&f)?;
        let at_line_start = if from == 0 {
            true
        } else if before.len < from {
            false
        } else {
            f.seek(SeekFrom::Start(from - 1))?;
            let mut previous = [0u8; 1];
            f.read_exact(&mut previous)?;
            previous[0] == b'\n'
        };
        let mut bytes_read = 0u64;
        let mut aborted = false;
        let mut saw_any_line = false;
        if before.len >= from && at_line_start {
            f.seek(SeekFrom::Start(from))?;
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, (&mut f).take(before.len - from));
            loop {
                line.clear();
                let n = reader.read_until(b'\n', &mut line)?;
                if n == 0 || !line.ends_with(b"\n") {
                    break;
                }
                bytes_read += n as u64;
                let raw = &line[..n - 1];
                let deliver = !saw_any_line || !raw.is_empty();
                saw_any_line = true;
                if deliver && on_event(Some(raw)).is_break() {
                    aborted = true;
                    break;
                }
            }
        }
        let after = crate::stable::stamp_file(&f)?;
        let named = crate::stamp(path).ok();
        let same_file = (before.dev, before.ino) == (after.dev, after.ino)
            && named.is_some_and(|now| (now.dev, now.ino) == (before.dev, before.ino));
        let not_rewritten = after.len >= before.len && (after.len > before.len || after.mtime_ns == before.mtime_ns);
        let rollback_after = if check_rollback { rollback_revision(path)? } else { (0, 0, 0) };
        if !same_file || !not_rewritten || rollback_before.is_some_and(|rb| rb != rollback_after) {
            continue;
        }
        return Ok(StreamedScan { bytes_read, stamp: before, at_line_start, rollback_after, aborted });
    }
    if check_rollback {
        anyhow::bail!("session file {} changed while it was parsed; try again", path.display())
    } else {
        anyhow::bail!("session file {} changed while it was read; try again", path.display())
    }
}

#[derive(Deserialize)]
struct RecordTag<'a> {
    #[serde(rename = "type", borrow)]
    ty: Option<Cow<'a, str>>,
}

/// Message envelope for index scanning: `content` and unknown fields are skipped without
/// allocating unless this is an own user message whose preview/last_input is needed.
#[derive(Deserialize)]
struct ScanMessageRecord {
    created_at_ms: Option<i64>,
    message: Option<ScanMessage>,
}

#[derive(Deserialize)]
struct ScanMessage {
    role: Option<String>,
    model_id: Option<String>,
    provider_id: Option<String>,
    usage: Option<Usage>,
    compaction: Option<Value>,
    notification: Option<Value>,
    turn_aborted: Option<Value>,
    branch_summary: Option<Value>,
}

/// Apply one line to the row. Returns false for a line that needs a full parse to apply
/// (only in catch-up mode).
fn apply(row: &mut Row, acc: &mut Acc, raw: &[u8], full: bool) -> bool {
    let Ok(tag) = serde_json::from_slice::<RecordTag>(raw) else { return true };
    let Some(kind) = tag.ty.as_deref().and_then(CoreRecordType::from_name) else { return true };
    match kind {
        CoreRecordType::Session => {
            let Ok(l) = serde_json::from_slice::<SessionHeader>(raw) else { return true };
            if !full {
                return false; // a second header: not an append-only file
            }
            if let Some(id) = l.session_id {
                row.session_id = id;
            }
            row.format_version = l.version.unwrap_or(crate::CURRENT_VERSION as i64);
            row.cwd = l.cwd.unwrap_or_default();
            row.created_at_ms = l.created_at_ms.unwrap_or(0);
            row.parent_session_id = l.parent_session_id;
            row.run_id = l.run_id;
            if row.kind == "agent" {
                row.title = l.label.map(|s| one_line(&s));
            }
        }
        CoreRecordType::Message => {
            let Ok(l) = serde_json::from_slice::<ScanMessageRecord>(raw) else { return true };
            let Some(m) = l.message else { return true };
            row.message_count += 1;
            acc.new_messages += 1;
            if let Some(t) = l.created_at_ms {
                row.updated_at_ms = row.updated_at_ms.max(t);
            }
            if m.role.as_deref() == Some("assistant") {
                if m.model_id.is_some() {
                    row.model = m.model_id;
                    row.provider = m.provider_id;
                }
                if let Some(u) = m.usage {
                    let count = |n: Option<serde_json::Number>| n.and_then(|n| n.as_f64()).unwrap_or(0.0) as i64;
                    row.tokens_in += count(u.input);
                    row.tokens_out += count(u.output);
                }
            }
            let own = m.role.as_deref() == Some("user")
                && !truthy(&m.compaction)
                && !truthy(&m.notification)
                && !truthy(&m.turn_aborted)
                && !truthy(&m.branch_summary);
            if own
                && let Ok(full_msg) = serde_json::from_slice::<MessageRecord>(raw)
                && let Some(t) =
                    full_msg.message.as_ref().and_then(|m| m.content.as_ref()).and_then(Content::first_text)
                && !t.starts_with("<bash command=")
            {
                let t = one_line(t);
                if row.preview.is_none() {
                    row.preview = Some(t.clone());
                }
                row.last_input = Some(t);
            }
        }
        CoreRecordType::Name => {
            let Ok(l) = serde_json::from_slice::<Name>(raw) else { return true };
            if let Some(n) = l.name {
                row.title = Some(one_line(&n));
            }
        }
        CoreRecordType::Label => {
            let Ok(l) = serde_json::from_slice::<Label>(raw) else { return true };
            if !full {
                acc.saw_label = true;
                return false;
            }
            let target = match &l.target_entry_id {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            acc.labels.retain(|(t, _)| *t != target);
            if let Some(lb) = l.label.filter(|s| !s.is_empty()) {
                acc.labels.push((target, lb));
            }
        }
        CoreRecordType::Archive => {
            let Ok(l) = serde_json::from_slice::<Archive>(raw) else { return true };
            acc.archive_entry = Some(l.archived.unwrap_or(true).then(|| l.created_at_ms.unwrap_or(0)));
        }
        CoreRecordType::ToolState | CoreRecordType::State | CoreRecordType::Compaction | CoreRecordType::Custom => {}
    }
    true
}

/// A full parse of a session file. `None`: no session header (empty or not a session).
pub fn parse_full(path: &Path) -> Result<Option<Row>> {
    let (agent, archive_dir) = place(path);
    let norm_path = norm(path);
    let mut row = Row::default();
    let mut acc = Acc::default();
    let mut saw_first = false;
    let mut empty_header = false;
    let mut header_err: Option<anyhow::Error> = None;

    let scanned = stream_from(path, 0, true, |event| match event {
        None => {
            row = Row {
                path: norm_path.clone(),
                kind: if agent { "agent".into() } else { "main".into() },
                ..Default::default()
            };
            acc = Acc::default();
            saw_first = false;
            empty_header = false;
            header_err = None;
            std::ops::ControlFlow::Continue(())
        }
        Some(raw) => {
            if !saw_first {
                saw_first = true;
                if raw.is_empty() {
                    empty_header = true;
                    return std::ops::ControlFlow::Break(());
                }
                let parsed_header = serde_json::from_slice::<Value>(raw)
                    .with_context(|| format!("{}: invalid session header in {}", crate::CORRUPT, path.display()))
                    .and_then(|header| {
                        crate::format::check_header(&header)?;
                        Ok(())
                    });
                if let Err(e) = parsed_header {
                    header_err = Some(e);
                    return std::ops::ControlFlow::Break(());
                }
            } else if raw.is_empty() {
                return std::ops::ControlFlow::Continue(());
            }
            apply(&mut row, &mut acc, raw, true);
            std::ops::ControlFlow::Continue(())
        }
    })?;

    if let Some(err) = header_err {
        return Err(err);
    }
    if empty_header || scanned.bytes_read == 0 || row.session_id.is_empty() {
        return Ok(None);
    }
    row.indexed_len = scanned.bytes_read as i64;
    row.indexed_mtime = scanned.stamp.mtime_ns as i64;
    row.indexed_dev = scanned.stamp.dev as i64;
    row.indexed_ino = scanned.stamp.ino as i64;
    row.indexed_lock_dev = scanned.rollback_after.0;
    row.indexed_lock_ino = scanned.rollback_after.1;
    row.indexed_lock_len = scanned.rollback_after.2;
    if !agent && row.parent_session_id.is_some() {
        row.kind = "fork".into();
    }
    row.labels = acc.labels.into_iter().map(|(_, l)| l).collect();
    let mtime_ms = scanned.stamp.mtime_ns as i64 / 1_000_000;
    if row.updated_at_ms == 0 {
        row.updated_at_ms = if row.created_at_ms > 0 { row.created_at_ms } else { mtime_ms };
    }
    row.archived_at_ms = match acc.archive_entry {
        Some(a) => a.map(|t| if t > 0 { t } else { mtime_ms }),
        None => archive_dir.then_some(mtime_ms),
    };
    if !row.cwd.is_empty() {
        row.project_root = Some(project_root(&row.cwd));
    }
    Ok(Some(row))
}

/// Continue a row from its watermark with what was appended since.
pub fn catch_up(row: &mut Row, path: &Path) -> Result<Progress> {
    let (header, header_stamp, header_rollback) = read_header_snapshot(path)?;
    if header.get("session_id").and_then(Value::as_str) != Some(row.session_id.as_str())
        || header.get("version").and_then(Value::as_i64) != Some(row.format_version)
        || header_stamp.dev as i64 != row.indexed_dev
        || header_stamp.ino as i64 != row.indexed_ino
        || header_rollback != (row.indexed_lock_dev, row.indexed_lock_ino, row.indexed_lock_len)
    {
        return Ok(Progress::NeedFull);
    }
    let from = row.indexed_len.max(0) as u64;
    let mut next = row.clone();
    let mut acc = Acc::default();
    let scanned = stream_from(path, from, false, |event| match event {
        None => {
            next = row.clone();
            acc = Acc::default();
            std::ops::ControlFlow::Continue(())
        }
        Some(raw) => {
            if apply(&mut next, &mut acc, raw, false) {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            }
        }
    })?;
    if rollback_revision(path)? != header_rollback {
        return Ok(Progress::NeedFull);
    }
    if scanned.aborted
        || !scanned.at_line_start
        || scanned.stamp.dev != header_stamp.dev
        || scanned.stamp.ino != header_stamp.ino
        || scanned.stamp.dev as i64 != row.indexed_dev
        || scanned.stamp.ino as i64 != row.indexed_ino
    {
        return Ok(Progress::NeedFull);
    }
    let (len, mtime) = (scanned.stamp.len, scanned.stamp.mtime_ns as i64);
    // shorter, or the same length with another mtime (edited in place)
    if len < from || (len == from && mtime != row.indexed_mtime) {
        return Ok(Progress::NeedFull);
    }
    if let Some(a) = acc.archive_entry {
        next.archived_at_ms = a.map(|t| if t > 0 { t } else { mtime / 1_000_000 });
    }
    next.indexed_len = (from + scanned.bytes_read) as i64;
    next.indexed_mtime = mtime;
    next.indexed_dev = scanned.stamp.dev as i64;
    next.indexed_ino = scanned.stamp.ino as i64;
    next.indexed_lock_dev = header_rollback.0;
    next.indexed_lock_ino = header_rollback.1;
    next.indexed_lock_len = header_rollback.2;
    *row = next;
    Ok(Progress::Grew(acc.new_messages))
}

/// Confirm that a parsed row still belongs to the file currently named by its path. This final
/// header/inode check narrows the interval between a stable read and committing its derived row.
pub(super) fn source_matches(path: &Path, row: &Row) -> Result<bool> {
    let (header, stamp, rollback) = read_header_snapshot(path)?;
    Ok(header.get("session_id").and_then(Value::as_str) == Some(row.session_id.as_str())
        && stamp.dev as i64 == row.indexed_dev
        && stamp.ino as i64 == row.indexed_ino
        && rollback == (row.indexed_lock_dev, row.indexed_lock_ino, row.indexed_lock_len))
}

/// The supported session ID currently named by a path, read from a stable header snapshot.
pub(super) fn session_id(path: &Path) -> Result<Option<String>> {
    let (header, _, _) = read_header_snapshot(path)?;
    Ok(header.get("session_id").and_then(Value::as_str).map(str::to_owned))
}

/// Every session file under the root: `<slug>/*.jsonl`, `<slug>/archive/*.jsonl` and
/// `<slug>/agents/<run_id>/*.jsonl` (stat only, never opened).
pub fn session_files(root: &Path) -> Vec<PathBuf> {
    let jsonl = |d: &Path, out: &mut Vec<PathBuf>| {
        for f in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|x| x == "jsonl") && f.file_type().is_ok_and(|t| t.is_file()) {
                out.push(p);
            }
        }
    };
    let mut out = vec![];
    for d in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if !d.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let d = d.path();
        jsonl(&d, &mut out);
        jsonl(&d.join("archive"), &mut out);
        for run in std::fs::read_dir(d.join("agents")).into_iter().flatten().flatten() {
            if run.file_type().is_ok_and(|t| t.is_dir()) {
                jsonl(&run.path(), &mut out);
            }
        }
    }
    // newest first by the timestamp in the file name (`2026-11-08T07-49-17_<id>.jsonl`)
    out.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(p: &Path, lines: &[Value]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn msg(id: &str, parent: Option<&str>, role: &str, text: &str, ts: i64) -> Value {
        json!({"type":"message","entry_id":id,"parent_entry_id":parent,"created_at_ms":ts,
               "message":{"role":role,"content":[{"type":"text","text":text}],
                          "model_id":"m1","provider_id":"p1","usage":{"input":10,"output":3}}})
    }

    /// Everything the picker shows comes from the file: counts, first own prompt, last name,
    /// labels (cleared ones dropped), archive state, tokens, model.
    #[test]
    fn a_full_parse_derives_the_row_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("--w--/2026-01-01T00-00-00_a.jsonl");
        write(
            &p,
            &[
                json!({"type":"session","version":3,"session_id":"a","cwd":"/w","created_at_ms":5}),
                json!({"type":"message","entry_id":"n","parent_entry_id":null,"created_at_ms":6,
                       "message":{"role":"user","notification":true,"content":"bg job done"}}),
                msg("u1", Some("n"), "user", "fix the\nparser", 7),
                msg("a1", Some("u1"), "assistant", "ok", 8),
                msg("u2", Some("u1"), "user", "again", 9), // a branch: not the last message's child
                json!({"type":"name","name":"Parser work"}),
                json!({"type":"label","target_entry_id":"u1","label":"start"}),
                json!({"type":"label","target_entry_id":"a1","label":"x"}),
                json!({"type":"label","target_entry_id":"a1","label":""}),
                json!({"type":"archive","archived":true,"created_at_ms":10}),
            ],
        );
        let r = parse_full(&p).unwrap().unwrap();
        assert_eq!((r.session_id.as_str(), r.kind.as_str(), r.cwd.as_str()), ("a", "main", "/w"));
        assert_eq!((r.message_count, r.tokens_in, r.tokens_out), (4, 10, 3));
        assert_eq!((r.created_at_ms, r.updated_at_ms, r.archived_at_ms), (5, 9, Some(10)));
        assert_eq!(r.preview.as_deref(), Some("fix the parser"));
        assert_eq!(r.last_input.as_deref(), Some("again"), "the latest own prompt");
        assert_eq!(r.title.as_deref(), Some("Parser work"));
        assert_eq!(r.labels, ["start"]);
        assert_eq!((r.model.as_deref(), r.provider.as_deref()), (Some("m1"), Some("p1")));
        assert_eq!(r.indexed_len as u64, std::fs::metadata(&p).unwrap().len());
    }

    /// A catch-up parses only what was appended; an unarchive entry clears the archive; a torn
    /// tail waits for the next parse; a label (which may clear an earlier one) asks for a full
    /// parse, as does a file that shrank.
    #[test]
    fn a_catch_up_continues_from_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("--w--/s.jsonl");
        write(
            &p,
            &[json!({"type":"session","version":3,"session_id":"s","cwd":"/w"}), msg("u1", None, "user", "one", 1)],
        );
        let mut r = parse_full(&p).unwrap().unwrap();
        write(&p, &[msg("a1", Some("u1"), "assistant", "two", 2), json!({"type":"archive","archived":false})]);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{\"type\":\"mess").unwrap();
        assert!(matches!(catch_up(&mut r, &p).unwrap(), Progress::Grew(1)));
        assert_eq!((r.message_count, r.updated_at_ms, r.archived_at_ms), (2, 2, None));
        assert!(r.indexed_len < std::fs::metadata(&p).unwrap().len() as i64, "the torn tail isn't consumed");
        assert!(matches!(catch_up(&mut r, &p).unwrap(), Progress::Grew(0)));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"age\"}\n").unwrap();
        write(&p, &[json!({"type":"label","target_entry_id":"u1","label":"x"})]);
        assert!(matches!(catch_up(&mut r, &p).unwrap(), Progress::NeedFull));
        std::fs::write(&p, "{\"type\":\"session\",\"version\":3,\"session_id\":\"s\"}\n").unwrap();
        assert!(matches!(catch_up(&mut r, &p).unwrap(), Progress::NeedFull));
    }

    #[test]
    fn agents_forks_and_legacy_archive_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let agent = root.join("--w--/agents/r1/000-explore.jsonl");
        write(
            &agent,
            &[
                json!({"type":"session","version":3,"session_id":"c","parent_session_id":"p","run_id":"r1","label":"explore"}),
            ],
        );
        let fork = root.join("--w--/f.jsonl");
        write(&fork, &[json!({"type":"session","version":3,"session_id":"f","parent_session_id":"p"})]);
        let old = root.join("--w--/archive/o.jsonl");
        write(&old, &[json!({"type":"session","version":3,"session_id":"o"})]);
        let a = parse_full(&agent).unwrap().unwrap();
        assert_eq!(
            (a.kind.as_str(), a.parent_session_id.as_deref(), a.run_id.as_deref(), a.title.as_deref()),
            ("agent", Some("p"), Some("r1"), Some("explore"))
        );
        assert_eq!(parse_full(&fork).unwrap().unwrap().kind, "fork");
        assert!(parse_full(&old).unwrap().unwrap().archived_at_ms.is_some(), "moved to archive/ = archived");
        assert_eq!(session_files(root).len(), 3);
    }
}
