use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, TryLockError,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use mlua::{Lua, Table, UserData, UserDataMethods, Value};

use mare_shared::lua::{HISTORY_LOSSLESS_MAX_NODES, json_to_lua, lua_to_json_lossy_bounded, rt_err};
use mare_store::{SessionFile, list_sessions_with_unsupported};

use super::attachments::{AttachmentStage, StagingDir, attachment_dir, install_staged_attachments};
use super::{check_handle_read, closed, host_only, sessions_root};

#[cfg(debug_assertions)]
static BEFORE_CLOSE_MARKER: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

#[cfg(debug_assertions)]
pub fn before_close_marker(hook: impl FnOnce() + Send + 'static) {
    *BEFORE_CLOSE_MARKER.lock().unwrap() = Some(Box::new(hook));
}

pub(super) struct SessionState {
    pub(super) file: Mutex<Option<SessionFile>>,
    closed: AtomicBool,
    pub(super) generation: AtomicU64,
    root: PathBuf,
    blocking: crate::blocking::Scope,
}

impl SessionState {
    pub(super) fn new(file: SessionFile, root: PathBuf, blocking: crate::blocking::Scope) -> Self {
        Self {
            file: Mutex::new(Some(file)),
            closed: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            root,
            blocking,
        }
    }

    pub(super) fn lock(&self) -> anyhow::Result<MutexGuard<'_, Option<SessionFile>>> {
        if self.closed.load(Ordering::Acquire) {
            anyhow::bail!("session file is closed");
        }
        let guard = self.file.lock().unwrap();
        // A queued operation may have passed the first check before close claimed the handle.
        if self.closed.load(Ordering::Acquire) {
            anyhow::bail!("session file is closed");
        }
        Ok(guard)
    }

    fn close(self: &Arc<Self>, checkpoint: bool) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let root = self.root.clone();
        let finish = move |mut file: Option<SessionFile>| {
            if let Some(f) = file.as_mut()
                && checkpoint
                && record_checkpoint(f, &root).is_err()
            {
                let _ = f.flush();
            }
        };
        match self.file.try_lock() {
            Ok(mut guard) => finish(guard.take()),
            Err(TryLockError::WouldBlock) => {
                // Non-cancellable blocking tasks retain the file lock. Keep their session exclusively
                // owned through checkpointing, without blocking Lua's cancellation cleanup.
                let state = Arc::clone(self);
                let token = match self.blocking.token() {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::error!("session close after blocking scope shutdown: {error}");
                        return;
                    }
                };
                std::thread::spawn(move || {
                    let _token = token;
                    let file = state.file.lock().unwrap_or_else(|e| e.into_inner()).take();
                    finish(file);
                });
            }
            Err(TryLockError::Poisoned(e)) => finish(e.into_inner().take()),
        }
    }
}

pub(super) fn read_session<T>(
    state: &SessionState,
    expected: Option<u64>,
    read: impl FnOnce(&mut mare_store::SessionSnapshot) -> anyhow::Result<T>,
) -> anyhow::Result<(T, u64)> {
    let guard = state.lock()?;
    let generation = state.generation.load(Ordering::Acquire);
    if expected.is_some_and(|expected| expected != generation) {
        anyhow::bail!("session source file changed since the tail was read");
    }
    let mut reader = guard.as_ref().ok_or_else(|| anyhow::anyhow!("session file is closed"))?.snapshot_reader()?;
    drop(guard);
    let result = read(&mut reader)?;
    if state.generation.load(Ordering::Acquire) != generation {
        anyhow::bail!("session source file changed while it was read");
    }
    Ok((result, generation))
}

/// A session file. Appends are plain O_APPEND writes; fsync happens only at the semantic
/// boundaries the session plugin marks with `sync = true` (before a model call, before a
/// side-effecting tool, at each step start — session-checkpoint-policy), at an
/// explicit `flush()`, and on close. No timer-based group commit.
pub struct LuaSessionFile {
    /// Closing invalidates the handle immediately. An in-flight operation retains the session
    /// lock and registry entry until it finishes, then releases them after checkpointing.
    pub(super) file: Arc<SessionState>,
    /// an open journal arms the durable-start gate for tool calls in this VM
    pub(super) journal: crate::exec_gate::JournalHold,
    pub(super) created_new: bool,
}

pub(super) struct SnapshotRevision {
    pub(super) file: Arc<SessionState>,
    pub(super) stamp: mare_store::FileStamp,
    pub(super) generation: u64,
}

impl UserData for SnapshotRevision {}

impl UserData for LuaSessionFile {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // append(entry, {sync = false, ack}) — sync fsyncs now (semantic boundaries: before a model
        // call, before a side-effecting tool, at each step start); otherwise a plain write, made
        // durable by the next sync / flush. `ack = tool_call_id` requires a matching tool_state/started
        // entry, implies sync, and admits the call once fsynced.
        m.add_method("append", |lua, this, (v, opts): (Value, Option<Table>)| {
            host_only(lua, "append")?;
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_append_failure(lua) {
                return Err(rt_err("injected journal append failure"));
            }
            let (mut j, lossy) = lua_to_json_lossy_bounded(lua, v, HISTORY_LOSSLESS_MAX_NODES)?;
            crate::projection::JsonProjector::from_lua(lua)?.project_json_checked(&mut j)?;
            this.write(vec![j], opts)?;
            Ok((true, lossy))
        });
        // append_batch({entry, ...}, {sync, ack}) -> (true, lossy); same key/value/number rules as append
        // sync); a failed write or fsync truncates the file back to before the batch
        m.add_method("append_batch", |lua, this, (list, opts): (Table, Option<Table>)| {
            host_only(lua, "append_batch")?;
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_append_failure(lua) {
                return Err(rt_err("injected journal append failure"));
            }
            let projector = crate::projection::JsonProjector::from_lua(lua)?;
            let mut lossy = false;
            let entries = list
                .sequence_values::<Value>()
                .map(|v| {
                    let (mut j, entry_lossy) = lua_to_json_lossy_bounded(lua, v?, HISTORY_LOSSLESS_MAX_NODES)?;
                    projector.project_json_checked(&mut j)?;
                    lossy |= entry_lossy;
                    Ok(j)
                })
                .collect::<mlua::Result<Vec<_>>>()?;
            this.write(entries, opts)?;
            Ok((true, lossy))
        });
        m.add_async_method("append_batch_async", |lua, this, (list, opts): (Table, Option<Table>)| {
            let prep = (|| -> mlua::Result<_> {
                host_only(&lua, "append_batch_async")?;
                #[cfg(debug_assertions)]
                if crate::exec_gate::take_append_failure(&lua) {
                    return Err(rt_err("injected journal append failure"));
                }
                let projector = crate::projection::JsonProjector::from_lua(&lua)?;
                let mut lossy = false;
                let entries = list
                    .sequence_values::<Value>()
                    .map(|v| {
                        let (mut j, entry_lossy) = lua_to_json_lossy_bounded(&lua, v?, HISTORY_LOSSLESS_MAX_NODES)?;
                        projector.project_json_checked(&mut j)?;
                        lossy |= entry_lossy;
                        Ok(j)
                    })
                    .collect::<mlua::Result<Vec<_>>>()?;
                let (sync, ack_id) = this.parse_write_opts(&entries, opts)?;
                let ack = match ack_id {
                    Some(id) => Some((this.journal.begin_ack()?, id)),
                    None => None,
                };
                #[cfg(debug_assertions)]
                if entries.iter().any(|e| {
                    mare_store::record::core::<mare_store::record::State>(e).is_some_and(|record| record.is_closed())
                }) && let Some(hook) = BEFORE_CLOSE_MARKER.lock().unwrap().take()
                {
                    hook();
                }
                Ok((this.file.clone(), entries, sync, ack, lossy))
            })();
            async move {
                let (file, entries, sync, ack, lossy) = prep?;
                crate::blocking::spawn(&lua, move || {
                    let mut guard = file.lock()?;
                    // Count attempts too: a failed write can be rolled back to the same length.
                    file.generation.fetch_add(1, Ordering::AcqRel);
                    guard
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("session file is closed"))?
                        .append_batch(&entries, sync)
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                if let Some((token, id)) = ack {
                    token.ack(id)?;
                }
                Ok((true, lossy))
            }
        });
        m.add_method("flush", |lua, this, ()| {
            host_only(lua, "flush")?;
            this.file.lock().map_err(rt_err)?.as_mut().ok_or_else(closed)?.flush().map_err(rt_err)
        });
        m.add_async_method("flush_async", |lua, this, ()| {
            let allowed = host_only(&lua, "flush_async");
            let file = this.file.clone();
            async move {
                allowed?;
                crate::blocking::spawn(&lua, move || {
                    file.lock()?.as_mut().ok_or_else(|| anyhow::anyhow!("session file is closed"))?.flush()
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)
            }
        });
        // Select this writable journal as the session whose tool starts gate execution.
        m.add_method("activate", |lua, this, ()| {
            host_only(lua, "activate")?;
            this.journal.activate()
        });
        // checkpoint(): fsync and record a resume checkpoint in the session index
        // — after a compaction; a clean close records one automatically
        m.add_async_method("checkpoint", |lua, this, ()| {
            let allowed = host_only(&lua, "checkpoint");
            let file = this.file.clone();
            async move {
                allowed?;
                crate::blocking::spawn(&lua, move || match file.lock()?.as_mut() {
                    Some(f) => record_checkpoint(f, &file.root),
                    None => Ok(false),
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)
            }
        });
        // close(): flush, record a checkpoint and release the session (lock + in-process registry)
        // when idle; otherwise defer cleanup until in-flight work completes. Idempotent.
        m.add_method("close", |lua, this, ()| {
            host_only(lua, "close")?;
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_close_failure(lua) {
                return Err(rt_err("injected journal close failure"));
            }
            this.journal.release();
            this.file.close(!this.journal.is_poisoned());
            Ok(())
        });
        // A prepared target never became the active journal; do not write its checkpoint.
        m.add_method("discard", |lua, this, ()| {
            host_only(lua, "close")?;
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_discard_before_release_failure(lua) {
                return Err(rt_err("injected discard failure before release"));
            }
            this.journal.release();
            this.file.close(false);
            #[cfg(debug_assertions)]
            if crate::exec_gate::take_close_failure(lua) {
                return Err(rt_err("injected target discard failure"));
            }
            Ok(())
        });
        // Transfer prepared attachments into a newly-created session's exclusive directory.
        m.add_method("install_staged_attachments", |lua, this, stage: mlua::AnyUserData| {
            host_only(lua, "install_staged_attachments")?;
            if !this.created_new {
                return Err(rt_err("attachments can only be installed for a newly created session"));
            }
            let stage = stage.borrow::<AttachmentStage>()?.acquire()?;
            let guard = this.file.lock().map_err(rt_err)?;
            let path = guard.as_ref().ok_or_else(closed)?.path();
            let file = guard.as_ref().ok_or_else(closed)?;
            if !file.matches_path_checked(path).map_err(rt_err)? {
                return Err(rt_err("session attachment target changed before installation"));
            }
            install_staged_attachments(lua, &stage, path).map_err(rt_err)?;
            if !file.matches_path_checked(path).map_err(rt_err)? {
                return Err(rt_err("session attachment target changed during installation"));
            }
            Ok(())
        });
        // stats() -> {scanned_from, checkpoint_mismatch, torn_bytes}: how open validated the file
        m.add_method("stats", |lua, this, ()| {
            check_handle_read(lua, this)?;
            let st = this.file.lock().map_err(rt_err)?.as_ref().ok_or_else(closed)?.stats();
            let t = lua.create_table()?;
            t.set("scanned_from", st.scanned_from)?;
            t.set("checkpoint_mismatch", st.checkpoint_mismatch)?;
            t.set("torn_bytes", st.torn_bytes)?;
            Ok(t)
        });
        m.add_method("path", |lua, this, ()| {
            check_handle_read(lua, this)?;
            Ok(this.file.lock().map_err(rt_err)?.as_ref().ok_or_else(closed)?.path().to_string_lossy().into_owned())
        });
        m.add_method("matches_path", |lua, this, path: String| {
            host_only(lua, "matches_path")?;
            this.file
                .lock()
                .map_err(rt_err)?
                .as_ref()
                .ok_or_else(closed)?
                .matches_path_checked(Path::new(&path))
                .map_err(rt_err)
        });
    }
}

impl LuaSessionFile {
    fn parse_write_opts(
        &self,
        entries: &[serde_json::Value],
        opts: Option<Table>,
    ) -> mlua::Result<(bool, Option<String>)> {
        let opt = |k: &str| opts.as_ref().and_then(|o| o.get::<Option<Value>>(k).ok().flatten());
        let ack = match opt("ack") {
            None => None,
            Some(Value::String(s)) => Some(s.to_string_lossy()),
            Some(v) => return Err(rt_err(format!("append: ack must be a call id string, got {}", v.type_name()))),
        };
        if let Some(id) = ack.as_deref() {
            self.journal.check_ack()?;
            let matching_start = entries.iter().any(|entry| {
                mare_store::record::core::<mare_store::record::ToolState>(entry)
                    .is_some_and(|record| record.is_started_for(id))
            });
            if !matching_start {
                return Err(rt_err("ack requires a tool_state started record with the same tool_call_id"));
            }
        }
        let sync = ack.is_some() || matches!(opt("sync"), Some(Value::Boolean(true)));
        Ok((sync, ack))
    }

    fn write(&self, entries: Vec<serde_json::Value>, opts: Option<Table>) -> mlua::Result<()> {
        let (sync, ack) = self.parse_write_opts(&entries, opts)?;
        #[cfg(debug_assertions)]
        if entries
            .iter()
            .any(|e| mare_store::record::core::<mare_store::record::State>(e).is_some_and(|record| record.is_closed()))
            && let Some(hook) = BEFORE_CLOSE_MARKER.lock().unwrap().take()
        {
            hook();
        }
        let mut file = self.file.lock().map_err(rt_err)?;
        // Count attempts too: a failed write can be rolled back to the same length.
        self.file.generation.fetch_add(1, Ordering::AcqRel);
        file.as_mut().ok_or_else(closed)?.append_batch(&entries, sync).map_err(rt_err)?;
        if let Some(id) = ack {
            self.journal.ack(id)?;
        }
        Ok(())
    }
}

/// fsync, then record the file's resume checkpoint and source generation in the session index
/// (which also catches the session's row up with what was appended).
fn record_checkpoint(f: &mut SessionFile, root: &Path) -> anyhow::Result<bool> {
    match f.checkpoint()? {
        Some(c) => {
            let path = f.path().to_path_buf();
            if !f.matches_path(&path) {
                return Ok(false);
            }
            let generation = mare_store::index::checkpoint_generation(&path)?;
            if !f.matches_path(&path) {
                return Ok(false);
            }
            let ix = mare_store::index::shared(root)?;
            ix.lock().unwrap_or_else(|e| e.into_inner()).set_checkpoint_for_generation(&path, &c, generation)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

impl Drop for LuaSessionFile {
    fn drop(&mut self) {
        self.file.close(!self.journal.is_poisoned());
    }
}

/// The resume checkpoint of a session file (none when the index can't be read: a full scan).
fn read_checkpoint(path: &Path) -> Option<mare_store::Checkpoint> {
    let ix = mare_store::index::shared(&sessions_root()).ok()?;
    let ix = ix.lock().unwrap_or_else(|e| e.into_inner());
    ix.checkpoint(path).ok().flatten()
}

pub(super) fn install_session_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // open(path) -> handle: lock (`<path>.lock`, inode re-checked) → validate (from the last
    // checkpoint when it matches) → append (a torn tail is cut first). Unsupported versions
    // are rejected; the file and existing backups are left untouched.
    t.set(
        "open",
        // open(path, {existing = true}): resuming — a missing file is an error carrying
        // mare_store::NOT_FOUND instead of a new, headerless session (nothing is created)
        lua.create_function(|lua, (p, opts): (String, Option<Table>)| {
            host_only(lua, "open")?;
            let existing =
                opts.as_ref().and_then(|o| o.get::<Option<bool>>("existing").ok().flatten()).unwrap_or(false);
            let auxiliary =
                opts.as_ref().map(|o| o.get::<Option<bool>>("auxiliary")).transpose()?.flatten().unwrap_or(false);
            // a matching checkpoint lets open validate only what was appended after it
            let cp = read_checkpoint(Path::new(&p));
            let create_new =
                opts.as_ref().map(|o| o.get::<Option<bool>>("create_new")).transpose()?.flatten().unwrap_or(false);
            if existing && create_new {
                return Err(rt_err("store.open: existing and create_new are mutually exclusive"));
            }
            let f = if create_new {
                SessionFile::create_new(Path::new(&p))
            } else if existing {
                SessionFile::open_existing(Path::new(&p), cp.as_ref())
            } else {
                SessionFile::open_with(Path::new(&p), cp.as_ref())
            };
            let file = Arc::new(SessionState::new(f.map_err(rt_err)?, sessions_root(), crate::blocking::scope(lua)));
            let defer_activation = opts
                .as_ref()
                .map(|o| o.get::<Option<bool>>("defer_activation"))
                .transpose()?
                .flatten()
                .unwrap_or(false);
            Ok(LuaSessionFile {
                file,
                journal: crate::exec_gate::JournalHold::new(lua, auxiliary, defer_activation)?,
                created_new: create_new,
            })
        })?,
    )?;
    // A private, exclusively-created staging directory for a not-yet-published fork.
    t.set(
        "reserve_staging_attachments",
        lua.create_function(|lua, path: String| {
            host_only(lua, "reserve_staging_attachments")?;
            let path = attachment_dir(Path::new(&path));
            mare_shared::private::create_dir_new(&path).map_err(rt_err)?;
            lua.create_userdata(AttachmentStage {
                inner: Mutex::new(Some(Arc::new(StagingDir::capture(path).map_err(rt_err)?))),
            })
        })?,
    )?;
    // Read an attachment for `journal` by directory-handle constraint, refusing symlinked
    // attachment directories/files and verifying content sha256 against the attachment name.
    t.set(
        "read_attachment",
        lua.create_async_function(|lua, (journal, name, max_bytes): (String, String, Option<u64>)| {
            let allowed = host_only(&lua, "read_attachment");
            let max = max_bytes.unwrap_or(32 * 1024 * 1024);
            async move {
                allowed?;
                let journal = PathBuf::from(journal);
                match crate::blocking::spawn(&lua, move || {
                    super::attachments::read_verified_attachment(&journal, &name, max)
                })?
                .await
                {
                    Ok(Ok(mut bytes)) => {
                        bytes.reserve_exact(1);
                        Ok(mlua::MultiValue::from_vec(vec![Value::String(lua.create_external_string(bytes)?)]))
                    }
                    Ok(Err(e)) => Ok(mlua::MultiValue::from_vec(vec![
                        Value::Nil,
                        Value::String(lua.create_string(e.to_string())?),
                    ])),
                    Err(e) => Err(rt_err(e)),
                }
            }
        })?,
    )?;
    // list(dir) -> { {path, header, mtime, size} } newest first
    // files(path) -> every file of a session (session, lock, attachments, snapshots, sub-agents) — for delete
    t.set(
        "files",
        lua.create_function(|lua, p: String| {
            host_only(lua, "files")?;
            Ok(mare_store::session_files(Path::new(&p))
                .into_iter()
                .map(|f| f.to_string_lossy().into_owned())
                .collect::<Vec<_>>())
        })?,
    )?;
    // delete(path) -> true | nil, err: the session and everything that belongs to it (lock,
    // backup, attachments, snapshots, sub-agent sessions), under its lock — refused (LOCKED /
    // OPEN_HERE) while open. The index is the caller's: append a tombstone {session_id, deleted = true}.
    t.set(
        "delete",
        lua.create_async_function(|lua, p: String| {
            let allowed = host_only(&lua, "delete");
            async move {
                allowed?;
                let r = crate::blocking::spawn(&lua, move || mare_store::delete_session(Path::new(&p)))?
                    .await
                    .map_err(rt_err)?;
                Ok(match r {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(format!("{e:#}"))),
                })
            }
        })?,
    )?;
    t.set(
        "list",
        // async: reads the header of every session file in the dir
        lua.create_async_function(|lua, p: String| {
            let allowed = host_only(&lua, "list");
            async move {
                allowed?;
                let (sessions, unsupported) =
                    crate::blocking::spawn(&lua, move || list_sessions_with_unsupported(Path::new(&p)))?
                        .await
                        .map_err(rt_err)?;
                let out = lua.create_table()?;
                for (i, s) in sessions.into_iter().enumerate() {
                    let e = lua.create_table()?;
                    e.set("path", s.path.to_string_lossy().into_owned())?;
                    if let Some(h) = &s.header {
                        e.set("header", json_to_lua(&lua, h)?)?;
                    }
                    let ms =
                        s.modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
                    e.set("mtime", ms)?;
                    e.set("size", s.size)?;
                    out.set(i + 1, e)?;
                }
                Ok((out, unsupported))
            }
        })?,
    )?;
    // dir(cwd) -> sessions directory for a working directory
    t.set(
        "dir",
        lua.create_function(|_, cwd: String| {
            let d: PathBuf = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(Path::new(&cwd)));
            Ok(d.to_string_lossy().into_owned())
        })?,
    )?;
    Ok(())
}
