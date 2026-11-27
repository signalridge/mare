//! `mare.crash`: tracks in-flight requests to detect crash loops (a core mechanism: it must hold
//! even when the agent plugin is replaced).
//!
//! `begin(fp)` durably marks the request `fp` as in flight before it is sent; `fp` is the stable
//! request id (generated on the first submission and reused by network and crash retries, so the
//! count accumulates across a resume). `finish(fp)` ends the attempt normally (success or a
//! handled error): the entry is kept as *ended* with its count cleared. A mark still set at the
//! next `begin(fp)` means the previous process died while that request was in flight: one crash.
//! `begin` returns the crash count (persisted before it returns) so the caller can refuse to
//! resend after a limit. State lives in `<state>/crash-guard.json`; beyond [`MAX_ENTRIES`] only
//! ended entries are evicted (oldest first) — a request still recovering is never forgotten.
//! Written temp → fsync → rename → fsync dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mlua::{Lua, Table};
use serde::{Deserialize, Serialize};

use mare_shared::lua::rt_err;

pub const MAX_ENTRIES: usize = 64;
pub const VERSION: u32 = 1;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(15);
const MAX_QUARANTINE_FILES: usize = 3;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Entry {
    /// set between begin and finish; an entry with it unset has ended (evictable)
    pub inflight: bool,
    /// crashes of this request so far (cleared by a normal finish)
    pub crashes: u32,
    /// last begin, ms since the epoch (eviction order)
    pub at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

fn path() -> PathBuf {
    mare_shared::paths::state_dir().join("crash-guard.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn parse_state(bytes: &[u8]) -> Result<State, String> {
    let val: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))?;
    let Some(obj) = val.as_object() else {
        return Err("top-level value is not a JSON object".into());
    };
    if obj.get("version").and_then(|v| v.as_u64()) != Some(u64::from(VERSION)) {
        return Err(format!("unsupported version (expected {VERSION})"));
    }
    serde_json::from_value(val).map_err(|e| format!("invalid state schema: {e}"))
}

fn quarantine_prefix(p: &Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    format!("{name}.bad-")
}

/// List quarantined `crash-guard.json.bad-*` backups in `dir`, newest first.
pub fn quarantined_backups(dir: &Path) -> Vec<PathBuf> {
    let prefix = "crash-guard.json.bad-";
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with(prefix).then(|| e.path())
        })
        .collect();
    files.sort();
    files.reverse();
    files
}

fn quarantine(p: &Path) -> std::io::Result<PathBuf> {
    quarantine_at(p, now_ms())
}

fn quarantine_at(p: &Path, ts: u64) -> std::io::Result<PathBuf> {
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let prefix = quarantine_prefix(p);
    let ts_prefix = format!("{prefix}{ts:013}-");
    let max_seq = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.strip_prefix(&ts_prefix).and_then(|rest| rest.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0);
    let mut seq = max_seq.saturating_add(1);
    let mut dst = parent.join(format!("{ts_prefix}{seq:06}"));
    while dst.exists() {
        seq = seq.saturating_add(1);
        dst = parent.join(format!("{ts_prefix}{seq:06}"));
    }
    std::fs::rename(p, &dst)?;
    // Keep at most MAX_QUARANTINE_FILES newest backups.
    let mut backups: Vec<_> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            name.to_string_lossy().starts_with(&prefix).then(|| e.path())
        })
        .collect();
    backups.sort();
    if backups.len() > MAX_QUARANTINE_FILES {
        let drop_count = backups.len() - MAX_QUARANTINE_FILES;
        for old in backups.into_iter().take(drop_count) {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(dst)
}

/// Load the state under the lock. Missing file -> empty state. I/O or permission error -> Err
/// (never treated as missing or quarantined). Confirmed format corruption -> quarantined in place
/// and reset to empty state.
fn load_for_update(p: &Path) -> std::io::Result<(State, Option<PathBuf>)> {
    let bytes = match std::fs::read(p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((State { version: VERSION, entries: BTreeMap::new() }, None));
        }
        Err(e) => return Err(e),
    };
    match parse_state(&bytes) {
        Ok(s) => Ok((s, None)),
        Err(why) => {
            let backup = quarantine(p)?;
            tracing::warn!(
                path = %p.display(),
                backup = %backup.display(),
                "crash guard state is corrupt ({why}); quarantined and reset"
            );
            Ok((State { version: VERSION, entries: BTreeMap::new() }, Some(backup)))
        }
    }
}

fn save(p: &Path, s: &State) -> std::io::Result<()> {
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        mare_shared::private::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(s).map_err(std::io::Error::other)?;
    mare_shared::paths::write_atomic(p, &bytes, mare_shared::paths::WriteMode::Private)
}

fn lock_timeout_error(lock_path: &Path) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, format!("timed out waiting for lock {}", lock_path.display()))
}

trait LocalWait {
    fn wait_timeout<'a>(
        &self,
        local_lock: &'a std::sync::Mutex<bool>,
        guard: std::sync::MutexGuard<'a, bool>,
        dur: Duration,
    ) -> std::sync::MutexGuard<'a, bool>;
    fn notify_one(&self);
}

impl LocalWait for std::sync::Condvar {
    fn wait_timeout<'a>(
        &self,
        _local_lock: &'a std::sync::Mutex<bool>,
        guard: std::sync::MutexGuard<'a, bool>,
        dur: Duration,
    ) -> std::sync::MutexGuard<'a, bool> {
        Self::wait_timeout(self, guard, dur).unwrap_or_else(|e| e.into_inner()).0
    }

    fn notify_one(&self) {
        Self::notify_one(self);
    }
}

#[cfg(test)]
fn acquire_local_lock(
    local_lock: &std::sync::Mutex<bool>,
    local_cvar: &std::sync::Condvar,
    lock_path: &Path,
    deadline: Instant,
) -> std::io::Result<()> {
    acquire_local_lock_inner(
        local_lock,
        local_cvar,
        lock_path,
        deadline,
        Instant::now,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
}

fn acquire_local_lock_inner<W: LocalWait>(
    local_lock: &std::sync::Mutex<bool>,
    local_cvar: &W,
    lock_path: &Path,
    deadline: Instant,
    now: impl Fn() -> Instant,
    #[cfg(test)] on_wait: Option<&(dyn Fn() + Sync)>,
    #[cfg(test)] post_wait_now: Option<&(dyn Fn() -> Instant + Sync)>,
) -> std::io::Result<()> {
    let mut held = local_lock.lock().unwrap_or_else(|e| e.into_inner());
    while *held {
        let Some(remaining) = deadline.checked_duration_since(now()).filter(|d| !d.is_zero()) else {
            return Err(lock_timeout_error(lock_path));
        };
        #[cfg(test)]
        if let Some(hook) = on_wait {
            hook();
        }
        held = local_cvar.wait_timeout(local_lock, held, remaining);
    }
    // Re-check the shared deadline after re-acquiring the mutex: `wait_timeout` may have
    // timed out right as the holder released `*held = false`.
    #[cfg(test)]
    let check_now = post_wait_now.map_or_else(&now, |f| f());
    #[cfg(not(test))]
    let check_now = now();
    if deadline.checked_duration_since(check_now).is_none_or(|d| d.is_zero()) {
        // We did not take ownership of the free lock and may have consumed the holder's
        // `notify_one()`; pass the wakeup on so other waiters are not stranded.
        local_cvar.notify_one();
        return Err(lock_timeout_error(lock_path));
    }
    *held = true;
    Ok(())
}

fn locked_with_timeout<T>(p: &Path, timeout: Duration, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    static LOCAL: (std::sync::Mutex<bool>, std::sync::Condvar) =
        (std::sync::Mutex::new(false), std::sync::Condvar::new());
    locked_with_deadline_inner(
        p,
        Instant::now() + timeout,
        &LOCAL,
        Instant::now,
        f,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
}

fn locked_with_deadline_inner<T>(
    p: &Path,
    deadline: Instant,
    local: &(std::sync::Mutex<bool>, std::sync::Condvar),
    now: impl Fn() -> Instant,
    f: impl FnOnce() -> std::io::Result<T>,
    #[cfg(test)] on_local_wait: Option<&(dyn Fn() + Sync)>,
    #[cfg(test)] on_would_block: Option<&(dyn Fn() + Sync)>,
) -> std::io::Result<T> {
    struct LocalGuard<'a>(&'a (std::sync::Mutex<bool>, std::sync::Condvar));
    impl Drop for LocalGuard<'_> {
        fn drop(&mut self) {
            let (lock, cvar) = self.0;
            let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
            *held = false;
            cvar.notify_one();
        }
    }

    let lock_path = p.with_extension("lock");

    // Share one deadline across the in-process mutex/condvar and the cross-process file lock so
    // multiple concurrent waiters cannot each wait a full `timeout` in series.
    let (local_lock, local_cvar) = local;
    acquire_local_lock_inner(
        local_lock,
        local_cvar,
        &lock_path,
        deadline,
        &now,
        #[cfg(test)]
        on_local_wait,
        #[cfg(test)]
        None,
    )?;
    let _local = LocalGuard(local);

    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        mare_shared::private::create_dir_all(dir)?;
    }
    let lock =
        mare_shared::private::open(std::fs::OpenOptions::new().create(true).truncate(false).write(true), &lock_path)?;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                #[cfg(test)]
                if let Some(hook) = on_would_block {
                    hook();
                }
                let Some(remaining) = deadline.checked_duration_since(now()).filter(|d| !d.is_zero()) else {
                    return Err(lock_timeout_error(&lock_path));
                };
                std::thread::sleep(LOCK_POLL.min(remaining));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    let r = f();
    let _ = lock.unlock();
    r
}

#[derive(Clone, Copy)]
enum OpKind {
    Begin,
    Finish,
    Release,
}

type OpResult = std::io::Result<(u32, Option<PathBuf>)>;

struct QueuedOp {
    id: u64,
    fp: String,
    kind: OpKind,
    deadline: Instant,
}

struct BatchState {
    leader_running: bool,
    next_id: u64,
    queue: Vec<QueuedOp>,
    results: std::collections::HashMap<u64, OpResult>,
}

struct PathBatcher {
    state: std::sync::Mutex<BatchState>,
    cvar: std::sync::Condvar,
}

fn path_batcher(p: &Path) -> std::sync::Arc<PathBatcher> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock, Weak};

    static BATCHERS: OnceLock<Mutex<HashMap<PathBuf, Weak<PathBatcher>>>> = OnceLock::new();
    let mut guard = BATCHERS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = guard.get(p).and_then(Weak::upgrade) {
        return existing;
    }
    let created = Arc::new(PathBatcher {
        state: Mutex::new(BatchState { leader_running: false, next_id: 0, queue: Vec::new(), results: HashMap::new() }),
        cvar: std::sync::Condvar::new(),
    });
    guard.insert(p.to_path_buf(), Arc::downgrade(&created));
    created
}

fn apply_batch_unlocked(p: &Path, batch: &[QueuedOp]) -> std::io::Result<Vec<(u32, Option<PathBuf>)>> {
    let (mut s, mut quarantined) = load_for_update(p)?;
    let mut dirty = quarantined.is_some();
    let mut results = Vec::with_capacity(batch.len());
    for op in batch {
        let q = quarantined.take();
        match op.kind {
            OpKind::Begin => {
                let e = s.entries.entry(op.fp.clone()).or_default();
                if e.inflight {
                    e.crashes += 1;
                }
                e.inflight = true;
                e.at = now_ms();
                let crashes = e.crashes;
                evict(&mut s);
                dirty = true;
                results.push((crashes, q));
            }
            OpKind::Finish | OpKind::Release => {
                if let Some(e) = s.entries.get_mut(&op.fp) {
                    e.inflight = false;
                    if matches!(op.kind, OpKind::Finish) {
                        e.crashes = 0;
                    }
                    e.at = now_ms();
                    evict(&mut s);
                    dirty = true;
                }
                results.push((0, q));
            }
        }
    }
    if dirty {
        s.version = VERSION;
        save(p, &s)?;
    }
    Ok(results)
}

/// Group-commit concurrent in-process updates to the same state file under one durable
/// `write_atomic` (temp → fsync → rename → dir fsync) so N concurrent callers do not serialize
/// N×2 fsyncs against one `LOCK_TIMEOUT`.
fn run_batched_op(p: &Path, fp: &str, kind: OpKind, timeout: Duration) -> OpResult {
    let batcher = path_batcher(p);
    let lock_path = p.with_extension("lock");
    let deadline = Instant::now() + timeout;

    let (my_id, batch) = {
        let mut st = batcher.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = st.next_id;
        st.next_id = st.next_id.wrapping_add(1);
        st.queue.push(QueuedOp { id, fp: fp.to_owned(), kind, deadline });
        loop {
            if let Some(res) = st.results.remove(&id) {
                return res;
            }
            if !st.queue.iter().any(|q| q.id == id) {
                st = batcher.cvar.wait(st).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            if !st.leader_running {
                st.leader_running = true;
                break (id, std::mem::take(&mut st.queue));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                st.queue.retain(|q| q.id != id);
                return Err(lock_timeout_error(&lock_path));
            };
            st = batcher.cvar.wait_timeout(st, remaining).unwrap_or_else(|e| e.into_inner()).0;
        }
    };

    struct LeaderGuard<'a> {
        batcher: &'a PathBatcher,
        batch: &'a [QueuedOp],
        finished: bool,
    }
    impl Drop for LeaderGuard<'_> {
        fn drop(&mut self) {
            if !self.finished {
                let mut st = self.batcher.state.lock().unwrap_or_else(|e| e.into_inner());
                for op in self.batch {
                    st.results
                        .entry(op.id)
                        .or_insert_with(|| Err(std::io::Error::other("crash guard leader panicked")));
                }
                st.leader_running = false;
                self.batcher.cvar.notify_all();
            }
        }
    }

    let mut guard = LeaderGuard { batcher: &batcher, batch: &batch, finished: false };
    let batch_deadline = batch.iter().map(|op| op.deadline).min().unwrap_or(deadline);
    let remaining = batch_deadline.saturating_duration_since(Instant::now());
    let outcome = if remaining.is_zero() {
        Err(lock_timeout_error(&lock_path))
    } else {
        locked_with_timeout(p, remaining, || apply_batch_unlocked(p, &batch))
    };

    let mut st = batcher.state.lock().unwrap_or_else(|e| e.into_inner());
    let mut my_result = None;
    match outcome {
        Ok(results) => {
            for (op, res) in batch.iter().zip(results) {
                if op.id == my_id {
                    my_result = Some(Ok(res));
                } else {
                    st.results.insert(op.id, Ok(res));
                }
            }
        }
        Err(err) => {
            for op in &batch {
                let cloned = std::io::Error::new(err.kind(), err.to_string());
                if op.id == my_id {
                    my_result = Some(Err(cloned));
                } else {
                    st.results.insert(op.id, Err(cloned));
                }
            }
        }
    }
    st.leader_running = false;
    guard.finished = true;
    batcher.cvar.notify_all();
    my_result.expect("leader always produces its own result")
}

/// Mark `fp` in flight; returns its crash count (including one for a mark left by a dead run).
pub fn begin(p: &Path, fp: &str) -> std::io::Result<u32> {
    begin_with_report(p, fp).map(|(crashes, _)| crashes)
}

/// Mark `fp` in flight; also returns the quarantine backup path when corrupt state was reset.
pub fn begin_with_report(p: &Path, fp: &str) -> std::io::Result<(u32, Option<PathBuf>)> {
    run_batched_op(p, fp, OpKind::Begin, LOCK_TIMEOUT)
}

/// Bound the state: drop the oldest *ended* entries; entries still in flight (recovering) stay.
fn evict(s: &mut State) {
    while s.entries.len() > MAX_ENTRIES {
        let oldest = s.entries.iter().filter(|(_, e)| !e.inflight).min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone());
        match oldest {
            Some(k) => s.entries.remove(&k),
            None => break,
        };
    }
}

/// The attempt ended normally: the request made progress, its crash count is cleared.
pub fn finish(p: &Path, fp: &str) -> std::io::Result<()> {
    finish_with_report(p, fp).map(|_| ())
}

/// The attempt ended normally; also returns the quarantine backup path when corrupt state was reset.
pub fn finish_with_report(p: &Path, fp: &str) -> std::io::Result<Option<PathBuf>> {
    end(p, fp, true)
}

/// The request was not sent (refused at the crash limit): it is no longer in flight — so it is
/// evictable and the next `begin` does not count another crash — but its count is kept, so the
/// same request stays refused.
pub fn release(p: &Path, fp: &str) -> std::io::Result<()> {
    release_with_report(p, fp).map(|_| ())
}

/// The request was not sent; also returns the quarantine backup path when corrupt state was reset.
pub fn release_with_report(p: &Path, fp: &str) -> std::io::Result<Option<PathBuf>> {
    end(p, fp, false)
}

fn end(p: &Path, fp: &str, clear: bool) -> std::io::Result<Option<PathBuf>> {
    let kind = if clear { OpKind::Finish } else { OpKind::Release };
    run_batched_op(p, fp, kind, LOCK_TIMEOUT).map(|(_, quarantined)| quarantined)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BeginCancelState {
    Running,
    Committed,
    HandedOff,
    CancelledWhileRunning,
}

/// Ensures an aborted `mare.crash.begin` future releases its durable mark after `begin_with_report`
/// finishes instead of leaving `inflight = true` on disk to be miscounted as a crash on retry.
struct BeginCancelGuard {
    state: std::sync::Arc<std::sync::Mutex<BeginCancelState>>,
    scope: crate::blocking::Scope,
    path: PathBuf,
    fp: String,
}

impl Drop for BeginCancelGuard {
    fn drop(&mut self) {
        let should_spawn_release = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match *st {
                BeginCancelState::Running => {
                    *st = BeginCancelState::CancelledWhileRunning;
                    false
                }
                BeginCancelState::Committed => {
                    *st = BeginCancelState::HandedOff;
                    true
                }
                BeginCancelState::HandedOff | BeginCancelState::CancelledWhileRunning => false,
            }
        };
        if should_spawn_release {
            let p = self.path.clone();
            let fp = self.fp.clone();
            let _ = self.scope.spawn(move || {
                let _ = release_with_report(&p, &fp);
            });
        }
    }
}

async fn begin_async(scope: crate::blocking::Scope, p: PathBuf, fp: String) -> mlua::Result<(u32, Option<String>)> {
    let cancel_state = std::sync::Arc::new(std::sync::Mutex::new(BeginCancelState::Running));
    let _cancel_guard =
        BeginCancelGuard { state: cancel_state.clone(), scope: scope.clone(), path: p.clone(), fp: fp.clone() };
    let work_state = cancel_state.clone();
    let (crashes, quarantined) = scope
        .spawn(move || {
            let res = begin_with_report(&p, &fp);
            if res.is_ok() {
                let release_now = {
                    let mut st = work_state.lock().unwrap_or_else(|e| e.into_inner());
                    if *st == BeginCancelState::CancelledWhileRunning {
                        true
                    } else {
                        *st = BeginCancelState::Committed;
                        false
                    }
                };
                if release_now {
                    let _ = release_with_report(&p, &fp);
                }
            }
            res
        })?
        .await
        .map_err(rt_err)?
        .map_err(rt_err)?;
    *cancel_state.lock().unwrap_or_else(|e| e.into_inner()) = BeginCancelState::HandedOff;
    Ok((crashes, quarantined.map(|q| q.to_string_lossy().into_owned())))
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    // begin(fingerprint) -> crashes so far (durable before the request is sent), plus optional
    // quarantine backup path if a corrupt state file was reset
    t.set(
        "begin",
        lua.create_async_function(|lua, fp: String| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.crash.begin")?;
            begin_async(crate::blocking::scope(&lua), path(), fp).await
        })?,
    )?;
    // finish(fingerprint) -> optional quarantine backup path if a corrupt state file was reset
    t.set(
        "finish",
        lua.create_async_function(|lua, fp: String| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.crash.finish")?;
            let quarantined = crate::blocking::spawn(&lua, move || finish_with_report(&path(), &fp))?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
            Ok(quarantined.map(|q| q.to_string_lossy().into_owned()))
        })?,
    )?;
    // release(fingerprint) -> optional quarantine backup path if a corrupt state file was reset
    t.set(
        "release",
        lua.create_async_function(|lua, fp: String| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.crash.release")?;
            let quarantined = crate::blocking::spawn(&lua, move || release_with_report(&path(), &fp))?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
            Ok(quarantined.map(|q| q.to_string_lossy().into_owned()))
        })?,
    )?;
    mare.set("crash", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_state(p: &Path) -> State {
        parse_state(&std::fs::read(p).unwrap()).unwrap()
    }

    #[test]
    fn a_mark_left_by_a_dead_run_counts_as_a_crash_and_finish_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        assert_eq!(begin(&p, "a").unwrap(), 0);
        // the process died here: the next begin of the same request sees the mark
        assert_eq!(begin(&p, "a").unwrap(), 1);
        assert_eq!(begin(&p, "a").unwrap(), 2);
        assert_eq!(begin(&p, "b").unwrap(), 0, "per fingerprint");
        finish(&p, "a").unwrap();
        assert!(!read_state(&p).entries["a"].inflight, "finished entries are kept as ended");
        assert_eq!(begin(&p, "a").unwrap(), 0, "a normal finish resets it");
    }

    #[test]
    fn a_refused_request_stays_refused_without_counting_more_crashes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        for _ in 0..3 {
            begin(&p, "a").unwrap();
        }
        assert_eq!(begin(&p, "a").unwrap(), 3, "at the limit");
        release(&p, "a").unwrap();
        assert!(!read_state(&p).entries["a"].inflight, "a refused request is not in flight (evictable)");
        assert_eq!(begin(&p, "a").unwrap(), 3, "still refused; the refusal is not another crash");
    }

    #[test]
    fn a_corrupt_state_file_is_quarantined_and_bounded_to_three() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        for i in 0..5 {
            let raw = format!("{{not json {i}");
            std::fs::write(&p, raw.as_bytes()).unwrap();
            let (crashes, backup) = begin_with_report(&p, "a").unwrap();
            assert_eq!(crashes, 0);
            let backup = backup.expect("corruption must report quarantine backup");
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), raw);
        }
        let backups = quarantined_backups(dir.path());
        assert_eq!(backups.len(), MAX_QUARANTINE_FILES, "keep at most 3 quarantine files");
        assert!(read_state(&p).entries.contains_key("a"), "rewritten as valid state");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(p.with_extension("lock")).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn non_object_and_unsupported_version_are_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        for bad in [b"[]".as_slice(), br#"{"version": 2, "entries": {}}"#, br#"{"entries": {}}"#] {
            std::fs::write(&p, bad).unwrap();
            let (crashes, backup) = begin_with_report(&p, "a").unwrap();
            assert_eq!(crashes, 0);
            let backup = backup.expect("invalid structure/version must quarantine");
            assert_eq!(std::fs::read(&backup).unwrap(), bad);
        }
    }

    #[test]
    fn io_error_is_not_treated_as_missing_or_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        // Make `p` a non-empty directory so std::fs::read fails with EISDIR (an I/O error).
        std::fs::create_dir_all(p.join("keep")).unwrap();
        let err = begin(&p, "a").unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(p.join("keep").exists(), "directory must not be overwritten or removed");
        assert!(quarantined_backups(dir.path()).is_empty(), "I/O error must not quarantine");
    }

    #[test]
    fn lock_times_out_when_held_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        let lp = p.with_extension("lock");
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).unwrap();
        holder.lock().unwrap();
        let err = locked_with_timeout(&p, Duration::from_millis(50), || Ok(())).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        holder.unlock().unwrap();
    }

    #[test]
    fn multiple_waiters_share_single_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        let lp = p.with_extension("lock");
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).unwrap();
        holder.lock().unwrap();

        let local = (std::sync::Mutex::new(false), std::sync::Condvar::new());
        let t0 = Instant::now();
        let deadline = t0 + LOCK_TIMEOUT;
        let expired = std::sync::atomic::AtomicBool::new(false);
        let now =
            || if expired.load(std::sync::atomic::Ordering::Acquire) { deadline + Duration::from_secs(1) } else { t0 };
        let phase = (std::sync::Mutex::new((0usize, 0usize)), std::sync::Condvar::new());

        let on_would_block = || {
            let mut st = phase.0.lock().unwrap_or_else(|e| e.into_inner());
            st.0 += 1;
            phase.1.notify_all();
            while st.1 < 3 {
                st = phase.1.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            expired.store(true, std::sync::atomic::Ordering::Release);
        };
        let on_local_wait = || {
            let mut st = phase.0.lock().unwrap_or_else(|e| e.into_inner());
            st.1 += 1;
            phase.1.notify_all();
        };

        let results: Vec<_> = std::thread::scope(|sc| {
            let leader = sc.spawn(|| {
                locked_with_deadline_inner(&p, deadline, &local, now, || Ok(()), None, Some(&on_would_block))
            });
            let mut st = phase.0.lock().unwrap_or_else(|e| e.into_inner());
            while st.0 == 0 {
                st = phase.1.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            drop(st);
            let peers: Vec<_> = (0..3)
                .map(|_| {
                    sc.spawn(|| {
                        locked_with_deadline_inner(
                            &p,
                            deadline,
                            &local,
                            now,
                            || Ok(()),
                            Some(&on_local_wait),
                            Some(&on_would_block),
                        )
                    })
                })
                .collect();
            std::iter::once(leader).chain(peers).map(|h| h.join().unwrap()).collect()
        });
        holder.unlock().unwrap();

        for r in results {
            assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        }
        assert_eq!(phase.0.lock().unwrap().0, 1, "peers sharing the deadline must time out before polling flock");
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        std::thread::scope(|sc| {
            for i in 0..16 {
                let p = &p;
                sc.spawn(move || begin(p, &format!("fp{i}")).unwrap());
            }
        });
        assert_eq!(read_state(&p).entries.len(), 16);
    }

    #[test]
    fn batched_concurrent_ops_preserve_serial_crash_counts_and_single_quarantine_report() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        let lp = p.with_extension("lock");
        std::fs::write(&p, b"{corrupt-batch").unwrap();

        // Hold the file lock so one leader blocks on flock while the remaining callers accumulate
        // in the path batch queue, then release to commit the queued operations together.
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).unwrap();
        holder.lock().unwrap();

        let batcher = path_batcher(&p);
        let reports: Vec<_> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..6)
                .map(|_| {
                    let p = &p;
                    sc.spawn(move || begin_with_report(p, "same-fp").unwrap())
                })
                .collect();
            // Wait until the leader is active and all 5 peers are queued behind it.
            let wait_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let st = batcher.state.lock().unwrap_or_else(|e| e.into_inner());
                if st.leader_running && st.queue.len() == 5 {
                    break;
                }
                drop(st);
                assert!(Instant::now() < wait_deadline, "queued callers did not reach batch queue in time");
                std::thread::sleep(Duration::from_millis(1));
            }
            holder.unlock().unwrap();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut counts: Vec<u32> = reports.iter().map(|(c, _)| *c).collect();
        counts.sort_unstable();
        assert_eq!(counts, vec![0, 1, 2, 3, 4, 5], "each concurrent begin on same fp must observe serial crash count");
        let quarantine_reports = reports.iter().filter(|(_, q)| q.is_some()).count();
        assert_eq!(quarantine_reports, 1, "corrupt file reset must be reported exactly once across concurrent callers");
        let st = read_state(&p);
        assert_eq!(st.entries["same-fp"].crashes, 5);
        assert!(st.entries["same-fp"].inflight);
    }

    #[test]
    fn only_ended_entries_are_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        // a request still recovering (in flight, crashed once)
        begin(&p, "recovering").unwrap();
        begin(&p, "recovering").unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            let fp = format!("fp{i}");
            begin(&p, &fp).unwrap();
            finish(&p, &fp).unwrap();
        }
        let s = read_state(&p);
        assert_eq!(s.entries.len(), MAX_ENTRIES, "bounded by evicting ended entries");
        assert_eq!(s.entries["recovering"].crashes, 1, "the recovering request is never evicted");
    }

    #[test]
    fn same_millisecond_five_quarantines_retain_newest_three() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        let ts = 1_700_000_000_000u64;
        let mut created = Vec::new();
        for i in 0..5 {
            let raw = format!("{{bad {i}");
            std::fs::write(&p, raw.as_bytes()).unwrap();
            let backup = quarantine_at(&p, ts).unwrap();
            assert!(backup.exists(), "backup #{i} ({}) must exist immediately after quarantine", backup.display());
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), raw);
            created.push((backup, raw));
        }
        let backups = quarantined_backups(dir.path());
        assert_eq!(backups.len(), MAX_QUARANTINE_FILES, "keep at most 3 quarantine files");
        let expected_paths: Vec<_> = created[2..].iter().rev().map(|(path, _)| path.clone()).collect();
        assert_eq!(backups, expected_paths, "newest 3 backups must be retained newest-first");
        for (path, raw) in &created[2..] {
            assert_eq!(&std::fs::read_to_string(path).unwrap(), raw);
        }
    }

    #[derive(Default)]
    struct FakeWaitState {
        tokens: usize,
        waiters: usize,
        done: usize,
        aborted: bool,
    }

    struct FakeWait {
        state: (std::sync::Mutex<FakeWaitState>, std::sync::Condvar),
    }

    impl FakeWait {
        fn new() -> Self {
            Self { state: (std::sync::Mutex::new(FakeWaitState::default()), std::sync::Condvar::new()) }
        }

        fn wait_until(&self, timeout: Duration, pred: impl Fn(&FakeWaitState) -> bool) -> bool {
            let (m, cv) = &self.state;
            let deadline = Instant::now() + timeout;
            let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
            while !pred(&s) && !s.aborted {
                let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                s = cv.wait_timeout(s, rem).unwrap_or_else(|e| e.into_inner()).0;
            }
            pred(&s) && !s.aborted
        }

        fn mark_done(&self) {
            let (m, cv) = &self.state;
            let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
            s.done += 1;
            cv.notify_all();
        }

        fn abort_all(&self) {
            let (m, cv) = &self.state;
            let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
            s.aborted = true;
            cv.notify_all();
        }
    }

    impl LocalWait for FakeWait {
        fn wait_timeout<'a>(
            &self,
            local_lock: &'a std::sync::Mutex<bool>,
            guard: std::sync::MutexGuard<'a, bool>,
            _dur: Duration,
        ) -> std::sync::MutexGuard<'a, bool> {
            let (m, cv) = &self.state;
            let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
            drop(guard);
            s.waiters += 1;
            cv.notify_all();
            while s.tokens == 0 && !s.aborted {
                s = cv.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            if !s.aborted {
                s.tokens -= 1;
            }
            drop(s);
            local_lock.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn notify_one(&self) {
            let (m, cv) = &self.state;
            let mut s = m.lock().unwrap_or_else(|e| e.into_inner());
            s.tokens += 1;
            cv.notify_one();
        }
    }

    struct ScopeCleanup<'a> {
        lock: &'a std::sync::Mutex<bool>,
        wait: &'a FakeWait,
        stop: &'a std::sync::atomic::AtomicBool,
    }

    impl Drop for ScopeCleanup<'_> {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            let mut held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            *held = false;
            self.wait.abort_all();
        }
    }

    #[test]
    fn local_lock_rechecks_deadline_after_wait_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let lp = dir.path().join("crash-guard.lock");
        let lock = std::sync::Mutex::new(true);
        let wait = FakeWait::new();
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(5);
        let expired = std::sync::atomic::AtomicBool::new(false);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let now = || if stop.load(std::sync::atomic::Ordering::Acquire) { t0 + Duration::from_secs(10) } else { t0 };
        let post_wait_now = || {
            if expired.load(std::sync::atomic::Ordering::Acquire) || stop.load(std::sync::atomic::Ordering::Acquire) {
                t0 + Duration::from_secs(10)
            } else {
                t0
            }
        };

        let result = std::thread::scope(|sc| {
            let _cleanup = ScopeCleanup { lock: &lock, wait: &wait, stop: &stop };
            let waiter = sc.spawn(|| {
                let res = acquire_local_lock_inner(&lock, &wait, &lp, deadline, now, None, Some(&post_wait_now));
                wait.mark_done();
                res
            });
            assert!(wait.wait_until(Duration::from_secs(2), |s| s.waiters == 1), "waiter must reach wait_timeout");
            let mut held = lock.lock().unwrap();
            expired.store(true, std::sync::atomic::Ordering::Release);
            *held = false;
            wait.notify_one();
            drop(held);
            assert!(wait.wait_until(Duration::from_secs(2), |s| s.done == 1), "waiter must finish in bound");
            waiter.join().unwrap()
        });

        let err = result.expect_err("waiter whose deadline expired must time out even if held became false");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn expired_waiter_passes_wakeup_to_next_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let lp = dir.path().join("crash-guard.lock");
        let lock = std::sync::Mutex::new(true);
        let wait = FakeWait::new();
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(5);
        let first_consumed = std::sync::atomic::AtomicBool::new(false);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let now = || if stop.load(std::sync::atomic::Ordering::Acquire) { t0 + Duration::from_secs(10) } else { t0 };
        let post_wait_now = || {
            if !first_consumed.swap(true, std::sync::atomic::Ordering::AcqRel)
                || stop.load(std::sync::atomic::Ordering::Acquire)
            {
                // Whichever waiter consumes the holder's single notify_one() observes an expired
                // clock and must pass the wakeup on via FakeWait::notify_one().
                t0 + Duration::from_secs(10)
            } else {
                t0
            }
        };

        let (r1, r2) = std::thread::scope(|sc| {
            let _cleanup = ScopeCleanup { lock: &lock, wait: &wait, stop: &stop };
            let spawn_waiter = || {
                sc.spawn(|| {
                    let res = acquire_local_lock_inner(&lock, &wait, &lp, deadline, now, None, Some(&post_wait_now));
                    wait.mark_done();
                    res
                })
            };
            let w1 = spawn_waiter();
            let w2 = spawn_waiter();
            assert!(
                wait.wait_until(Duration::from_secs(2), |s| s.waiters == 2),
                "both waiters must reach wait_timeout"
            );

            let mut held = lock.lock().unwrap();
            *held = false;
            wait.notify_one();
            drop(held);

            assert!(
                wait.wait_until(Duration::from_secs(2), |s| s.done == 2),
                "second waiter must receive the passed-on wakeup within bound"
            );
            (w1.join().unwrap(), w2.join().unwrap())
        });

        let results = [r1, r2];
        assert_eq!(
            results.iter().filter(|r| r.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::TimedOut)).count(),
            1,
            "the waiter that consumed the wakeup expired must time out: {results:?}"
        );
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "the remaining waiter must receive the passed-on wakeup and acquire the lock: {results:?}"
        );
    }

    #[test]
    fn waiter_scope_exits_within_bound_when_signal_never_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let lp = dir.path().join("crash-guard.lock");
        let lock = std::sync::Mutex::new(true);
        let wait = FakeWait::new();
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_secs(5);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let now = || if stop.load(std::sync::atomic::Ordering::Acquire) { t0 + Duration::from_secs(10) } else { t0 };
        let never_signal_hook = || {};

        let start = Instant::now();
        let timed_out = std::thread::scope(|sc| {
            let _cleanup = ScopeCleanup { lock: &lock, wait: &wait, stop: &stop };
            let waiter =
                sc.spawn(|| acquire_local_lock_inner(&lock, &wait, &lp, deadline, now, Some(&never_signal_hook), None));
            // Wait for an impossible waiter count so the bounded wait fails, triggering ScopeCleanup.
            let got = wait.wait_until(Duration::from_millis(20), |s| s.waiters == 99);
            assert!(!got, "impossible signal count must time out");
            drop(_cleanup);
            let res = waiter.join().unwrap();
            res.is_err()
        });
        assert!(timed_out, "aborted waiter must exit cleanly on ScopeCleanup");
        assert!(start.elapsed() < Duration::from_secs(2), "cleanup must exit within bound");
    }

    #[test]
    fn finish_with_report_and_release_with_report_propagate_quarantine_backup() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");

        begin(&p, "a").unwrap();
        std::fs::write(&p, b"{corrupt-finish").unwrap();
        let backup_f = finish_with_report(&p, "a").unwrap().expect("finish must report quarantine backup");
        assert!(backup_f.exists(), "finish backup must exist");
        assert_eq!(std::fs::read_to_string(&backup_f).unwrap(), "{corrupt-finish");
        assert!(read_state(&p).entries.is_empty(), "clean empty state persisted after finish quarantine");

        begin(&p, "b").unwrap();
        std::fs::write(&p, b"{corrupt-release").unwrap();
        let backup_r = release_with_report(&p, "b").unwrap().expect("release must report quarantine backup");
        assert!(backup_r.exists(), "release backup must exist");
        assert_eq!(std::fs::read_to_string(&backup_r).unwrap(), "{corrupt-release");
        assert!(read_state(&p).entries.is_empty(), "clean empty state persisted after release quarantine");
    }

    #[tokio::test]
    async fn aborted_begin_releases_durable_inflight_mark() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash-guard.json");
        let lp = p.with_extension("lock");

        // 1. Future dropped while blocking worker is still running behind the lock.
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lp).unwrap();
        holder.lock().unwrap();
        let scope = crate::blocking::Scope::default();
        {
            let fut = begin_async(scope.clone(), p.clone(), "fp-running".into());
            tokio::pin!(fut);
            assert!(futures_util::poll!(fut.as_mut()).is_pending(), "begin_async must block on held file lock");
        }
        // Future is now dropped; unlock so the blocking worker completes begin_with_report and immediately releases.
        holder.unlock().unwrap();
        scope.close_and_wait().await;
        let st = read_state(&p);
        assert!(!st.entries["fp-running"].inflight, "cancelled in-flight begin must release durable mark");
        assert_eq!(st.entries["fp-running"].crashes, 0, "cancelled begin must not count as a crash");
        assert_eq!(begin(&p, "fp-running").unwrap(), 0, "subsequent retry must observe 0 prior crashes");

        // 2. Future dropped after blocking worker already committed before returning to Lua.
        assert_eq!(begin(&p, "fp-committed").unwrap(), 0);
        let scope2 = crate::blocking::Scope::default();
        {
            let _guard = BeginCancelGuard {
                state: std::sync::Arc::new(std::sync::Mutex::new(BeginCancelState::Committed)),
                scope: scope2.clone(),
                path: p.clone(),
                fp: "fp-committed".into(),
            };
        }
        scope2.close_and_wait().await;
        let st2 = read_state(&p);
        assert!(!st2.entries["fp-committed"].inflight, "committed-then-dropped begin must spawn durable release");
        assert_eq!(st2.entries["fp-committed"].crashes, 0);
        assert_eq!(begin(&p, "fp-committed").unwrap(), 0);
    }

    #[tokio::test]
    async fn lua_crash_bindings_track_begin_release_finish_and_quarantine() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let (c0, q0): (u32, Option<String>) =
            lua.load(r#"return mare.crash.begin("lua-fp")"#).eval_async().await.unwrap();
        assert_eq!((c0, q0), (0, None));
        let (c1, _): (u32, Option<String>) =
            lua.load(r#"return mare.crash.begin("lua-fp")"#).eval_async().await.unwrap();
        assert_eq!(c1, 1);
        let qr: Option<String> = lua.load(r#"return mare.crash.release("lua-fp")"#).eval_async().await.unwrap();
        assert!(qr.is_none());
        let (c2, _): (u32, Option<String>) =
            lua.load(r#"return mare.crash.begin("lua-fp")"#).eval_async().await.unwrap();
        assert_eq!(c2, 1, "released entry must not count another crash on retry");

        std::fs::write(path(), b"{corrupt-begin").unwrap();
        let (cb, qb): (u32, Option<String>) =
            lua.load(r#"return mare.crash.begin("lua-fp")"#).eval_async().await.unwrap();
        assert_eq!(cb, 0);
        assert!(qb.is_some(), "corrupt state on begin must return quarantine backup path");

        std::fs::write(path(), b"{corrupt-finish").unwrap();
        let qf: Option<String> = lua.load(r#"return mare.crash.finish("lua-fp")"#).eval_async().await.unwrap();
        assert!(qf.is_some(), "corrupt state on finish must return quarantine backup path");

        std::fs::write(path(), b"{corrupt-release").unwrap();
        let qr2: Option<String> = lua.load(r#"return mare.crash.release("lua-fp")"#).eval_async().await.unwrap();
        assert!(qr2.is_some(), "corrupt state on release must return quarantine backup path");

        // Invalid schema under valid version, Condvar expired-deadline wakeup relay, EISDIR begin_async, and all-inflight evict.
        assert!(parse_state(br#"{"version": 1, "entries": "bad"}"#).is_err());
        let dir = tempfile::tempdir().unwrap();
        let lp = dir.path().join("crash-guard.lock");
        let (lock, cvar) = (std::sync::Mutex::new(false), std::sync::Condvar::new());
        let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        assert_eq!(acquire_local_lock(&lock, &cvar, &lp, expired).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        let bad_path = dir.path().join("crash-guard.json");
        std::fs::create_dir_all(bad_path.join("keep")).unwrap();
        assert!(begin_async(crate::blocking::Scope::default(), bad_path, "fp-err".into()).await.is_err());
        let mut s = State { version: VERSION, entries: BTreeMap::new() };
        for i in 0..=MAX_ENTRIES {
            s.entries.insert(format!("inflight-{i}"), Entry { inflight: true, crashes: 1, at: i as u64 });
        }
        evict(&mut s);
        assert_eq!(s.entries.len(), MAX_ENTRIES + 1);
    }
}
