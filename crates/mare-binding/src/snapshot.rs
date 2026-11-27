//! File snapshot journal: before a tool writes, removes or renames a file, core saves its
//! original content, so `/rewind` can put files back to how they were before a turn — whatever
//! edit tool did the writing (replacing the edit tools must not lose the snapshots), with or
//! without git.
//!
//! Layout under the configured directory (the session plugin puts it next to the session file):
//! `index.jsonl` (one record per line, 0600) and `blobs/<sha256>` (original contents, 0600,
//! written atomically). Records: `{"turn": id}` starts a turn; `{"turn", "call", "path",
//! "before": sha|null}` is the first touch of a path in that turn (`null` = didn't exist);
//! `{"turn", "path", "after": sha|null}` is the content a write left behind (to detect later
//! outside edits). A size cap trims the backups of the oldest turns first; a trimmed backup is
//! reported as missing, never guessed.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table, Value};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};

use mare_shared::lua::rt_err;

/// Default cap for stored backups (bytes); `configure{cap_bytes}` changes it.
pub const DEFAULT_CAP: u64 = 64 * 1024 * 1024;
/// Files larger than this are not backed up (recorded as missing; `/rewind` says so).
const MAX_FILE: u64 = 16 * 1024 * 1024;
/// Index records kept: past this, the oldest turns' records go (their backups were trimmed
/// first anyway) so the index and every scan of it stay bounded.
const MAX_RECORDS: usize = 20_000;

struct Journal {
    dir: PathBuf,
    cap: u64,
    turn: Option<String>,
    /// whether a shell command has already been recorded for the active turn
    shell: bool,
    /// paths already captured in the current turn (only the first touch matters)
    seen: HashSet<PathBuf>,
    /// total size of `blobs/`, counted once and then kept up to date (not rescanned per capture)
    blob_bytes: Option<u64>,
}

impl Journal {
    fn new(dir: PathBuf, cap: u64) -> Self {
        // temp files of a blob write cut short by a crash (each write uses a fresh name)
        let blobs = dir.join("blobs");
        for e in std::fs::read_dir(&blobs).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().ends_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Self { dir, cap, turn: None, shell: false, seen: HashSet::new(), blob_bytes: None }
    }
}

/// Per-session snapshot journal handle, stored in the session's Lua `app_data`.
#[derive(Clone, Default)]
pub struct SnapshotJournal(Arc<Mutex<Option<Journal>>>);

/// Snapshot target captured on the Lua thread for async/blocking file operations.
#[derive(Clone)]
pub struct SnapshotCapture {
    journal: SnapshotJournal,
    call: Option<String>,
}

/// Return (initializing if absent) the snapshot journal handle owned by `lua`'s session/runtime.
pub fn handle(lua: &Lua) -> SnapshotJournal {
    if let Some(j) = lua.app_data_ref::<SnapshotJournal>() {
        return j.clone();
    }
    let j = SnapshotJournal::default();
    lua.set_app_data(j.clone());
    j
}

impl SnapshotJournal {
    fn configure(&self, dir: Option<PathBuf>, cap: u64) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *g = dir.map(|d| Journal::new(d, cap));
    }

    fn begin_turn(&self, id: String) -> std::io::Result<bool> {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(j) = g.as_mut() else { return Ok(false) };
        j.turn = Some(id.clone());
        j.shell = false;
        j.seen.clear();
        append(&j.dir, &json!({ "turn": id }))?;
        Ok(true)
    }

    /// Record that the active turn ran a shell command (whose file changes cannot be snapshotted).
    pub fn mark_shell(&self) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(j) = g.as_mut() else { return };
        let Some(turn) = j.turn.clone() else { return };
        if j.shell {
            return;
        }
        j.shell = true;
        if let Err(e) = append(&j.dir, &json!({ "turn": turn, "shell": true })) {
            tracing::warn!("snapshot journal: shell record not written: {e}");
        }
    }

    fn dir(&self) -> Option<PathBuf> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|j| j.dir.clone())
    }

    fn has_turn(&self) -> bool {
        self.0.lock().map(|g| g.as_ref().is_some_and(|j| j.turn.is_some())).unwrap_or(false)
    }

    /// Before a tool touches `path`: save its original content once per turn. Never fails the
    /// write (a snapshot problem is recorded as a missing backup, which `/rewind` reports).
    pub fn capture(&self, path: &Path, call: Option<&str>) {
        // mare's own files (spill output, sessions, packages) aren't user changes
        if path.starts_with(mare_shared::paths::state_dir()) || path.starts_with(mare_shared::paths::data_dir()) {
            return;
        }
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(j) = g.as_mut() else { return };
        let Some(turn) = j.turn.clone() else { return };
        if !j.seen.insert(path.to_path_buf()) {
            return;
        }
        let before = match std::fs::symlink_metadata(path) {
            Err(_) => Some(J::Null),
            Ok(m) if m.is_file() && m.len() <= MAX_FILE && !linked(path) => std::fs::read(path)
                .ok()
                .and_then(|b| put_blob(&j.dir, &b).ok().map(|r| (r, b.len() as u64)))
                .map(|((h, new), n)| {
                    if new && let Some(t) = j.blob_bytes.as_mut() {
                        *t += n;
                    }
                    J::String(h)
                }),
            Ok(_) => None, // too large, a link or not a regular file: no backup
        };
        let mode = file_mode(path);
        let rec = match before {
            Some(b) => json!({ "turn": turn, "call": call, "path": path, "before": b, "mode": mode }),
            None => json!({ "turn": turn, "call": call, "path": path, "missing": true }),
        };
        if let Err(e) = append(&j.dir, &rec) {
            // the write goes ahead; /rewind will report the file as having no snapshot
            tracing::warn!("snapshot journal: record for {} not written: {e}", path.display());
        }
        trim(j);
    }

    /// After a successful write: what it left behind (to tell later outside edits apart).
    pub fn after(&self, path: &Path) {
        let g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(j) = g.as_ref() else { return };
        let Some(turn) = j.turn.clone() else { return };
        let after = std::fs::read(path).ok().map(|b| J::String(sha(&b))).unwrap_or(J::Null);
        if let Err(e) = append(&j.dir, &json!({ "turn": turn, "path": path, "after": after })) {
            tracing::warn!("snapshot journal: record for {} not written: {e}", path.display());
        }
    }
}

impl SnapshotCapture {
    pub fn capture(&self, path: &Path) {
        self.journal.capture(path, self.call.as_deref());
    }

    pub fn after(&self, path: &Path) {
        self.journal.after(path);
    }
}

fn sha(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 0700 / 0600 on Unix, private DACLs on Windows (`mare_shared::private`)
fn private_dir(p: &Path) -> std::io::Result<()> {
    mare_shared::private::create_dir_all(p)
}

fn open_private(p: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true);
    if append {
        o.append(true);
    } else {
        o.write(true).truncate(true);
    }
    mare_shared::private::open(&mut o, p)
}

fn append(dir: &Path, rec: &J) -> std::io::Result<()> {
    private_dir(dir)?;
    let mut f = open_private(&dir.join("index.jsonl"), true)?;
    f.write_all((rec.to_string() + "\n").as_bytes())?;
    f.sync_data()
}

/// Store `bytes` as a blob (atomic: temp → fsync → rename). Returns its hash and whether it
/// was new (counted into the journal's size).
fn put_blob(dir: &Path, bytes: &[u8]) -> std::io::Result<(String, bool)> {
    let h = sha(bytes);
    let blobs = dir.join("blobs");
    private_dir(&blobs)?;
    let target = blobs.join(&h);
    if target.exists() {
        return Ok((h, false));
    }
    mare_shared::paths::write_atomic(&target, bytes, mare_shared::paths::WriteMode::Private).map(|_| (h, true))
}

fn read_index(dir: &Path) -> Vec<J> {
    std::fs::read_to_string(dir.join("index.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// A plain file we may back up and restore: not a symlink, not hard-linked (restoring through a
/// hard link would modify another path as well).
fn linked(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => true,
        #[cfg(unix)]
        Ok(m) => {
            use std::os::unix::fs::MetadataExt;
            m.is_file() && m.nlink() > 1
        }
        #[cfg(not(unix))]
        Ok(_) => false,
        Err(_) => false,
    }
}

/// Capture for an active tool context, or arm an explicitly requested host capture.
/// The execution context's call id takes precedence over the explicit fallback.
pub fn capture_context(lua: &Lua, fallback_call: Option<&str>) -> Option<SnapshotCapture> {
    let ctx = mare_shared::exec_ctx::current(lua);
    if ctx.is_none() && fallback_call.is_none() {
        return None;
    }
    let journal = handle(lua);
    if fallback_call.is_none() && !journal.has_turn() {
        return None;
    }
    let call = ctx
        .and_then(|ctx| ctx.get::<Option<String>>("tool_call_id").ok().flatten())
        .or_else(|| fallback_call.map(str::to_owned));
    Some(SnapshotCapture { journal, call })
}

/// Over the cap: delete the backups of the oldest turns (records stay; restores report them as
/// missing). Blobs still referenced by a newer turn are kept.
fn trim(j: &mut Journal) {
    let blobs = j.dir.join("blobs");
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let total = *j.blob_bytes.get_or_insert_with(|| {
        std::fs::read_dir(&blobs).into_iter().flatten().flatten().map(|e| size(&e.path())).sum()
    });
    if total <= j.cap {
        return;
    }
    let idx = read_index(&j.dir);
    let order = turn_order(&idx);
    let mut by_turn: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for r in &idx {
        if let (Some(t), Some(b)) = (r["turn"].as_str(), r["before"].as_str())
            && let Some(i) = order.iter().position(|x| x == t)
        {
            by_turn.entry(i).or_default().push(b.to_string());
        }
    }
    let mut total = total;
    let newest = order.len().saturating_sub(1);
    let keep_newest: HashSet<String> = by_turn.get(&newest).cloned().unwrap_or_default().into_iter().collect();
    for (i, hashes) in &by_turn {
        if total <= j.cap || *i == newest {
            break;
        }
        // blobs still used by a later turn stay
        let later: HashSet<&String> = by_turn.range(i + 1..).flat_map(|(_, v)| v).collect();
        for h in hashes {
            if later.contains(h) || keep_newest.contains(h) {
                continue;
            }
            let p = blobs.join(h);
            let s = size(&p);
            if std::fs::remove_file(&p).is_ok() {
                total = total.saturating_sub(s);
            }
        }
    }
    j.blob_bytes = Some(total);
    cap_records(&j.dir, &idx, &order);
}

/// Over [`MAX_RECORDS`]: rewrite the index without the oldest turns (atomically), down to half.
fn cap_records(dir: &Path, idx: &[J], order: &[String]) {
    if idx.len() <= MAX_RECORDS {
        return;
    }
    let mut per_turn: std::collections::HashMap<&str, usize> = Default::default();
    for r in idx {
        if let Some(t) = r["turn"].as_str() {
            *per_turn.entry(t).or_default() += 1;
        }
    }
    let (mut n, mut drop) = (idx.len(), HashSet::new());
    for t in order {
        if n <= MAX_RECORDS / 2 || drop.len() + 1 >= order.len() {
            break;
        }
        n -= per_turn.get(t.as_str()).copied().unwrap_or(0);
        drop.insert(t.as_str());
    }
    let mut buf = String::new();
    for r in idx.iter().filter(|r| !r["turn"].as_str().is_some_and(|t| drop.contains(t))) {
        buf.push_str(&r.to_string());
        buf.push('\n');
    }
    if let Err(e) = mare_shared::paths::write_atomic(
        &dir.join("index.jsonl"),
        buf.as_bytes(),
        mare_shared::paths::WriteMode::Private,
    ) {
        tracing::warn!("snapshot journal: index not trimmed: {e}");
    }
}

fn turn_order(idx: &[J]) -> Vec<String> {
    let mut order: Vec<String> = vec![];
    let mut seen: HashSet<&str> = HashSet::new();
    for r in idx {
        if r.get("path").is_none()
            && let Some(t) = r["turn"].as_str()
            && seen.insert(t)
        {
            order.push(t.to_string());
        }
    }
    order
}

struct TurnRecord {
    turn: String,
    shell: bool,
    files: Vec<(String, J)>,
}

/// Per turn (oldest first): each turn's `shell` flag and every file's state before and after that turn.
fn turns(dir: &Path) -> Vec<TurnRecord> {
    let idx = read_index(dir);
    let order = turn_order(&idx);
    let mut out: Vec<TurnRecord> =
        order.iter().map(|t| TurnRecord { turn: t.clone(), shell: false, files: vec![] }).collect();
    // turn -> slot, (turn, path) -> file: one pass, no linear searches
    let slot_of: std::collections::HashMap<String, usize> =
        order.iter().cloned().enumerate().map(|(i, t)| (t, i)).collect();
    let mut file_of: std::collections::HashMap<(usize, String), usize> = Default::default();
    for r in &idx {
        let Some(t) = r["turn"].as_str() else { continue };
        let Some(&si) = slot_of.get(t) else { continue };
        if r.get("shell").and_then(J::as_bool) == Some(true) {
            out[si].shell = true;
        }
        let Some(p) = r["path"].as_str() else { continue };
        let files = &mut out[si].files;
        let fi = *file_of.entry((si, p.to_string())).or_insert_with(|| {
            files.push((p.to_string(), json!({})));
            files.len() - 1
        });
        let entry = &mut files[fi];
        let e = entry.1.as_object_mut().unwrap();
        if r.get("before").is_some() || r.get("missing").is_some() {
            e.entry("before").or_insert(r.get("before").cloned().unwrap_or(J::Null));
            if let Some(m) = r.get("mode").filter(|m| !m.is_null()) {
                e.entry("mode").or_insert(m.clone());
            }
            if r.get("missing").is_some() {
                e.insert("missing".into(), J::Bool(true));
            }
        }
        if let Some(a) = r.get("after") {
            e.insert("after".into(), a.clone());
        }
    }
    out
}

#[derive(Default, Debug)]
struct Outcome {
    restored: Vec<String>,
    conflicts: Vec<String>,
    missing: Vec<String>,
    refused: Vec<String>,
    failed: Vec<(String, String)>,
}

/// Put every file touched in `from` or a later turn back to its content before `from`.
/// `force` restores files changed outside mare since; otherwise they're listed as conflicts and
/// left alone. Paths that are links now are refused.
fn restore(dir: &Path, from: &str, force: bool) -> Result<Outcome, String> {
    let all = turns(dir);
    let Some(start) = all.iter().position(|t| t.turn == from) else {
        return Err(format!("no snapshots for turn {from}"));
    };
    // earliest "before" per path (from `start` on) and the latest "after" written by mare
    let mut plan: BTreeMap<String, (J, bool, Option<J>, Option<u32>)> = BTreeMap::new();
    for t in &all[start..] {
        for (p, e) in &t.files {
            let slot = plan.entry(p.clone()).or_insert_with(|| {
                (
                    e.get("before").cloned().unwrap_or(J::Null),
                    e.get("missing").is_some(),
                    None,
                    e.get("mode").and_then(J::as_u64).map(|m| m as u32),
                )
            });
            if let Some(a) = e.get("after") {
                slot.2 = Some(a.clone());
            }
        }
    }
    let mut out = Outcome::default();
    let mut to_apply: Vec<(String, PathBuf, J, Option<u32>)> = Vec::with_capacity(plan.len());
    // Phase 1: preflight every path without mutating disk. If `!force` and any file was
    // modified outside mare, return all detected conflicts without touching other files.
    for (p, (before, missing, last_after, mode)) in plan {
        let path = PathBuf::from(&p);
        if missing {
            out.missing.push(p);
            continue;
        }
        // never through a link — neither the file nor any directory above it that the user (or
        // the model, running as the user) could have created; a planted link would make mare,
        // which isn't sandboxed, write outside the sandbox
        if linked(&path) || user_linked_ancestor(&path) {
            out.refused.push(p);
            continue;
        }
        let current = std::fs::read(&path).ok().map(|b| J::String(sha(&b))).unwrap_or(J::Null);
        if !force && last_after.as_ref().is_some_and(|a| *a != current) {
            out.conflicts.push(p);
            continue;
        }
        to_apply.push((p, path, before, mode));
    }
    if !force && !out.conflicts.is_empty() {
        return Ok(out);
    }
    // Phase 2: apply file restores / removals.
    for (p, path, before, mode) in to_apply {
        let r = match &before {
            J::Null => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            },
            J::String(h) => match std::fs::read(dir.join("blobs").join(h)) {
                // a damaged backup is reported, never written back
                Ok(bytes) if sha(&bytes) == *h => restore_file_atomic(&path, &bytes, mode).map_err(|e| e.to_string()),
                _ => {
                    out.missing.push(p);
                    continue;
                }
            },
            _ => Err("bad record".into()),
        };
        match r {
            Ok(()) => out.restored.push(p),
            Err(e) => out.failed.push((p, e)),
        }
    }
    Ok(out)
}

/// Permission bits of a file (Unix), recorded with its backup so a deleted file comes back
/// with them.
fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path).ok().filter(|m| m.is_file()).map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Is any existing directory above `path` a symlink the current user owns (and so could have
/// planted)? Root-owned system links (macOS `/var`, `/tmp`) are fine. Windows: any link.
fn user_linked_ancestor(path: &Path) -> bool {
    let mut a = path.parent();
    while let Some(dir) = a {
        if let Ok(m) = std::fs::symlink_metadata(dir)
            && m.file_type().is_symlink()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: getuid has no preconditions
                if m.uid() == unsafe { libc::getuid() } {
                    return true;
                }
            }
            #[cfg(not(unix))]
            return true;
        }
        a = dir.parent();
    }
    false
}

/// Create missing directories one level at a time, refusing to go through a link (never follow
/// links on the restore path).
fn create_dirs_nofollow(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "a directory on the path is a link"));
        }
        Ok(m) if m.is_dir() => return Ok(()),
        Ok(_) => return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "not a directory")),
        Err(_) => {}
    }
    if let Some(parent) = dir.parent() {
        create_dirs_nofollow(parent)?;
    }
    match std::fs::create_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "a directory on the path is a link"))
            } else {
                Ok(())
            }
        }
        r => r,
    }
}

/// Restore `bytes` to `path`: a random-named temp file next to it, created exclusively and
/// without following links (O_EXCL | O_NOFOLLOW, 0600:633-655), fsynced,
/// given the original (or recorded) mode, renamed over the target, then the directory fsynced.
fn restore_file_atomic(path: &Path, bytes: &[u8], recorded_mode: Option<u32>) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    create_dirs_nofollow(parent)?;
    let mode = std::fs::metadata(path).ok().map(|m| m.permissions());
    let mut rnd = [0u8; 8];
    rand::fill(&mut rnd[..]);
    let tmp = parent.join(format!(
        ".{}.mare-rewind.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    {
        let mut f = o.open(&tmp)?;
        let w = f.write_all(bytes).and_then(|_| f.sync_all());
        if let Err(e) = w {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    match mode {
        Some(m) => {
            let _ = std::fs::set_permissions(&tmp, m);
        }
        None => {
            #[cfg(unix)]
            if let Some(m) = recorded_mode {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m));
            }
            #[cfg(not(unix))]
            let _ = recorded_mode;
        }
    }
    // the no-follow open above is the /rewind safety; the rename shares the Windows retry
    if let Err(e) = mare_shared::paths::rename_retry(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let _ = handle(lua);
    let t = lua.create_table()?;
    // configure{dir, cap_bytes?} — where the journal lives (nil dir: off)
    t.set(
        "configure",
        lua.create_function(|lua, opts: Option<Table>| {
            crate::exec_ctx::refuse_deny_read(lua, "snapshot.configure")?;
            let dir = opts.as_ref().and_then(|o| o.get::<Option<String>>("dir").ok().flatten());
            let cap =
                opts.as_ref().and_then(|o| o.get::<Option<u64>>("cap_bytes").ok().flatten()).unwrap_or(DEFAULT_CAP);
            handle(lua).configure(dir.map(PathBuf::from), cap);
            Ok(())
        })?,
    )?;
    // begin_turn(id): later captures belong to this turn
    t.set(
        "begin_turn",
        lua.create_function(|lua, id: String| {
            crate::exec_ctx::refuse_deny_read(lua, "snapshot.begin_turn")?;
            handle(lua).begin_turn(id).map_err(rt_err)
        })?,
    )?;
    // turns() -> { {turn, files = {{path, before?, after?, missing?}}} } oldest first
    t.set(
        "turns",
        lua.create_function(|lua, ()| {
            crate::exec_ctx::refuse_deny_read(lua, "snapshot.turns")?;
            let Some(dir) = handle(lua).dir() else { return Ok(Value::Nil) };
            let list: Vec<J> = turns(&dir)
                .into_iter()
                .map(|t| {
                    let files: Vec<J> = t
                        .files
                        .into_iter()
                        .map(|(p, mut e)| {
                            e["path"] = J::String(p);
                            e
                        })
                        .collect();
                    json!({ "turn": t.turn, "shell": t.shell, "files": files })
                })
                .collect();
            mare_shared::lua::json_to_lua(lua, &J::Array(list))
        })?,
    )?;
    // before(hash) -> original content of a backup (nil when trimmed)
    t.set(
        "content",
        lua.create_function(|lua, h: String| {
            crate::exec_ctx::refuse_deny_read(lua, "snapshot.content")?;
            let Some(dir) = handle(lua).dir() else { return Ok(None) };
            if h.contains('/') || h.contains('.') {
                return Ok(None);
            }
            std::fs::read(dir.join("blobs").join(h)).ok().map(|b| lua.create_string(b)).transpose()
        })?,
    )?;
    // restore(turn, {force}) -> {restored, conflicts, missing, refused, failed}
    t.set(
        "restore",
        lua.create_async_function(|lua, (turn, opts): (String, Option<Table>)| {
            let allowed = crate::exec_ctx::refuse_deny_read(&lua, "snapshot.restore");
            let force = opts.and_then(|o| o.get::<Option<bool>>("force").ok().flatten()).unwrap_or(false);
            let dir = handle(&lua).dir();
            async move {
                allowed?;
                let Some(dir) = dir else {
                    return Err(mlua::Error::runtime("no snapshot journal for this session"));
                };
                let out = tokio::task::spawn_blocking(move || restore(&dir, &turn, force))
                    .await
                    .map_err(rt_err)?
                    .map_err(mlua::Error::runtime)?;
                let failed: Vec<J> = out.failed.into_iter().map(|(p, e)| json!({ "path": p, "error": e })).collect();
                mare_shared::lua::json_to_lua(
                    &lua,
                    &json!({
                        "restored": out.restored,
                        "conflicts": out.conflicts,
                        "missing": out.missing,
                        "refused": out.refused,
                        "failed": failed,
                    }),
                )
            }
        })?,
    )?;
    mare.set("snapshot", t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(journal: &SnapshotJournal, dir: &Path, cap: u64) {
        journal.configure(Some(dir.to_path_buf()), cap);
    }
    fn begin(journal: &SnapshotJournal, id: &str) {
        assert!(journal.begin_turn(id.into()).unwrap());
    }

    #[test]
    fn explicit_capture_tracks_a_turn_started_before_the_write() {
        let lua = Lua::new();
        let tmp = tempfile::tempdir().unwrap();
        let journal_dir = tmp.path().join("journal");
        let path = tmp.path().join("file.txt");
        std::fs::write(&path, "before").unwrap();
        let journal = handle(&lua);
        setup(&journal, &journal_dir, DEFAULT_CAP);

        let capture = capture_context(&lua, Some("rpc.fs.write"))
            .expect("explicit capture remains available before a turn starts");
        begin(&journal, "turn");
        capture.capture(&path);
        let records = read_index(&journal_dir);
        assert_eq!(records.last().unwrap()["call"], "rpc.fs.write");
        assert_eq!(records.last().unwrap()["path"], path.to_string_lossy().as_ref());
    }

    #[test]
    fn restores_before_a_turn_refuses_links_reports_conflicts_and_trims() {
        let journal = SnapshotJournal::default();
        let tmp = tempfile::tempdir().unwrap();
        let j = tmp.path().join("journal");
        let f = tmp.path().join("a.txt");
        let created = tmp.path().join("new.txt");
        std::fs::write(&f, "v1").unwrap();
        setup(&journal, &j, DEFAULT_CAP);
        begin(&journal, "t1");
        journal.capture(&f, Some("c1"));
        std::fs::write(&f, "v2").unwrap();
        journal.after(&f);
        journal.capture(&created, Some("c2"));
        std::fs::write(&created, "new").unwrap();
        journal.after(&created);
        begin(&journal, "t2");
        journal.capture(&f, Some("c3"));
        std::fs::write(&f, "v3").unwrap();
        journal.after(&f);
        // back to before t2: only a.txt (v2)
        let o = restore(&j, "t2", false).unwrap();
        assert_eq!(o.restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v2");
        // outside edit since: a conflict, preflight writes nothing so neither file is changed
        std::fs::write(&f, "edited outside").unwrap();
        let o = restore(&j, "t1", false).unwrap();
        assert_eq!(o.conflicts, vec![f.to_string_lossy().to_string()]);
        assert!(o.restored.is_empty(), "conflict preflight must not restore other files: {o:?}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "edited outside");
        assert!(created.exists(), "conflict preflight must not delete other files before confirmation");
        // forced: back to before t1 — the created file is removed
        let o = restore(&j, "t1", true).unwrap();
        assert!(o.conflicts.is_empty());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert!(!created.exists());
        // a path that is a symlink now is refused, never written through
        #[cfg(unix)]
        {
            let target = tmp.path().join("elsewhere");
            std::fs::write(&target, "keep").unwrap();
            std::fs::remove_file(&f).unwrap();
            std::os::unix::fs::symlink(&target, &f).unwrap();
            let o = restore(&j, "t1", true).unwrap();
            assert!(o.refused.contains(&f.to_string_lossy().to_string()));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
            std::fs::remove_file(&f).unwrap();
        }
        // over the cap: the oldest turn's backups go first and are reported missing
        let j2 = tmp.path().join("journal2");
        let g = tmp.path().join("g.bin");
        setup(&journal, &j2, 10);
        for (i, body) in ["0123456789abcdef", "fedcba9876543210", "zz"].iter().enumerate() {
            std::fs::write(&g, body).unwrap();
            begin(&journal, &format!("u{i}"));
            journal.capture(&g, None);
            std::fs::write(&g, format!("{body}!")).unwrap();
            journal.after(&g);
        }
        let o = restore(&j2, "u0", true).unwrap();
        assert_eq!(o.missing, vec![g.to_string_lossy().to_string()], "trimmed backup reported, not guessed");
    }

    /// The journal stays bounded and tidy: stale blob temp files from a crash are removed when
    /// it is configured, the blob total is kept up to date without rescanning, the index drops
    /// the oldest turns past its cap, and a restore that can't finish leaves no temp file.
    #[test]
    fn journal_cleans_up_caps_its_index_and_restores_leave_no_temp_files() {
        let journal = SnapshotJournal::default();
        let tmp = tempfile::tempdir().unwrap();
        let j = tmp.path().join("journal");
        std::fs::create_dir_all(j.join("blobs")).unwrap();
        std::fs::write(j.join("blobs/.abc.123.tmp"), "half").unwrap();
        setup(&journal, &j, DEFAULT_CAP);
        assert!(!j.join("blobs/.abc.123.tmp").exists(), "crash leftovers removed");
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "12345").unwrap();
        begin(&journal, "t1");
        journal.capture(&f, None);
        trim(journal.0.lock().unwrap().as_mut().unwrap());
        assert_eq!(journal.0.lock().unwrap().as_ref().unwrap().blob_bytes, Some(5));
        // the index cap: the oldest turns' records go, the newest stay
        let mut idx = vec![];
        for t in 0..(MAX_RECORDS / 4 + 1) {
            idx.push(json!({ "turn": format!("t{t}") }));
            for k in 0..3 {
                idx.push(json!({ "turn": format!("t{t}"), "path": format!("/f{k}"), "before": null }));
            }
        }
        let order = turn_order(&idx);
        cap_records(&j, &idx, &order);
        let kept = read_index(&j);
        assert!(kept.len() <= MAX_RECORDS / 2 + 4, "{}", kept.len());
        assert_eq!(kept.last().unwrap()["turn"], format!("t{}", MAX_RECORDS / 4));
        // a restore whose rename fails (the target is a directory now) cleans its temp file up
        let target = tmp.path().join("d");
        std::fs::create_dir_all(target.join("inner")).unwrap();
        assert!(restore_file_atomic(&target, b"x", None).is_err());
        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("mare-rewind"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A planted link must never make the (unsandboxed) restore write elsewhere: a parent
    /// directory replaced by a user-owned symlink is refused, a deleted file comes back with its
    /// recorded mode, and a damaged backup is reported instead of written.
    #[cfg(unix)]
    #[test]
    fn restore_refuses_planted_parent_links_keeps_modes_and_checks_backups() {
        use std::os::unix::fs::PermissionsExt;
        let journal = SnapshotJournal::default();
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let ws = tmp.path().join("ws");
        let sub = ws.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let f = sub.join("a.sh");
        std::fs::write(&f, "v1").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o750)).unwrap();
        let j = tmp.path().join("journal");
        setup(&journal, &j, DEFAULT_CAP);
        begin(&journal, "t1");
        journal.capture(&f, Some("c1"));
        std::fs::remove_file(&f).unwrap();
        journal.after(&f);
        // the model swaps the directory for a link pointing outside the workspace
        std::fs::remove_dir(&sub).unwrap();
        std::os::unix::fs::symlink(&outside, &sub).unwrap();
        let o = restore(&j, "t1", true).unwrap();
        assert_eq!(o.refused.len(), 1, "{o:?}");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "nothing written through the link");
        // with the real directory back, the deleted file returns with its recorded mode
        std::fs::remove_file(&sub).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        let o = restore(&j, "t1", true).unwrap();
        assert_eq!(o.restored.len(), 1, "{o:?}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o750);
        // a damaged backup is reported, never written back
        for e in std::fs::read_dir(j.join("blobs")).unwrap().flatten() {
            std::fs::write(e.path(), "tampered").unwrap();
        }
        std::fs::remove_file(&f).unwrap();
        let o = restore(&j, "t1", true).unwrap();
        assert_eq!(o.missing.len(), 1, "{o:?}");
        assert!(!f.exists());
    }

    #[test]
    fn journals_are_isolated_per_lua_session() {
        let tmp = tempfile::tempdir().unwrap();
        let j1_dir = tmp.path().join("j1");
        let j2_dir = tmp.path().join("j2");
        let f1 = tmp.path().join("f1.txt");
        let f2 = tmp.path().join("f2.txt");
        std::fs::write(&f1, "s1-v1").unwrap();
        std::fs::write(&f2, "s2-v1").unwrap();

        let lua1 = Lua::new();
        let lua2 = Lua::new();
        let h1 = handle(&lua1);
        let h2 = handle(&lua2);
        h1.configure(Some(j1_dir.clone()), DEFAULT_CAP);
        h2.configure(Some(j2_dir.clone()), DEFAULT_CAP);

        h1.begin_turn("t-a".into()).unwrap();
        h2.begin_turn("t-b".into()).unwrap();
        h1.capture(&f1, Some("c1"));
        h2.capture(&f2, Some("c2"));

        // Disabling session 2's journal does not affect session 1.
        h2.configure(None, DEFAULT_CAP);
        assert_eq!(h1.dir().as_deref(), Some(j1_dir.as_path()));
        assert_eq!(h2.dir(), None);

        let t1 = turns(&j1_dir);
        let t2 = turns(&j2_dir);
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0].turn, "t-a");
        assert!(!t1[0].shell);
        assert_eq!(t1[0].files.len(), 1);
        assert_eq!(t1[0].files[0].0, f1.to_string_lossy());
        assert_eq!(t2.len(), 1);
        assert_eq!(t2[0].turn, "t-b");
        assert!(!t2[0].shell);
        assert_eq!(t2[0].files.len(), 1);
        assert_eq!(t2[0].files[0].0, f2.to_string_lossy());
    }
}
