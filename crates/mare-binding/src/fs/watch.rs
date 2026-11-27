//! Connection/owner-scoped policy-filtered filesystem watcher backing `mare.fs.watch`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use mlua::{Function, Lua, Table};

use crate::ossandbox;

use super::resolve;

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(25);
const WATCH_MAX_ENTRIES: usize = 8192;
pub const MAX_WATCHES_PER_CONN: usize = 32;

static ACTIVE_WATCHES: AtomicUsize = AtomicUsize::new(0);

/// Number of live background filesystem watcher tasks (used by RPC teardown/limit tests).
pub fn active_watch_count() -> usize {
    ACTIVE_WATCHES.load(Ordering::SeqCst)
}

struct WatchDropGuard;

impl WatchDropGuard {
    fn new() -> Self {
        ACTIVE_WATCHES.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for WatchDropGuard {
    fn drop(&mut self) {
        ACTIVE_WATCHES.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, PartialEq, Eq)]
struct EntryMeta {
    is_dir: bool,
    len: u64,
    modified_ns: u128,
}

fn meta_fingerprint(m: &std::fs::Metadata) -> EntryMeta {
    let modified_ns = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    EntryMeta { is_dir: m.is_dir(), len: m.len(), modified_ns }
}

#[derive(Default)]
struct TreeSnapshot {
    entries: BTreeMap<PathBuf, EntryMeta>,
    truncated: bool,
}

fn active_policy(lua: &Lua) -> Option<ossandbox::Policy> {
    ossandbox::enforce_ceiling(lua, ossandbox::current(lua))
}

pub fn check_policy(policy: Option<&ossandbox::Policy>, op: &str, path: &Path, cwd: &Path) -> Result<(), String> {
    let Some(policy) = policy else { return Ok(()) };
    policy.check(op, &path.to_string_lossy(), cwd).map_err(|d| {
        let marker = policy.denial_marker(&d);
        let detail = d.path.as_deref().unwrap_or("");
        if detail.is_empty() { marker } else { format!("{marker}: {detail}") }
    })
}

fn snapshot_tree(root: &Path, policy: Option<&ossandbox::Policy>) -> std::io::Result<TreeSnapshot> {
    let mut map = BTreeMap::new();
    if policy.is_some_and(|p| !p.can_read(root)) {
        return Ok(TreeSnapshot::default());
    }
    let meta = std::fs::symlink_metadata(root)?;
    map.insert(root.to_path_buf(), meta_fingerprint(&meta));
    if !meta.is_dir() {
        return Ok(TreeSnapshot { entries: map, truncated: false });
    }
    let mut truncated = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if dir == root => return Err(e),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                truncated = true;
                continue;
            }
        };
        let mut entries = Vec::new();
        for res in rd {
            match res {
                Ok(e) => entries.push(e),
                Err(_) => truncated = true,
            }
        }
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let p = entry.path();
            if policy.is_some_and(|pol| !pol.can_read(&p)) {
                continue;
            }
            let m = match entry.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            if map.len() >= WATCH_MAX_ENTRIES {
                return Ok(TreeSnapshot { entries: map, truncated: true });
            }
            let is_dir = m.is_dir();
            map.insert(p.clone(), meta_fingerprint(&m));
            if is_dir {
                stack.push(p);
            }
        }
    }
    Ok(TreeSnapshot { entries: map, truncated })
}

fn diff_snapshots(prev: &BTreeMap<PathBuf, EntryMeta>, next: &BTreeMap<PathBuf, EntryMeta>) -> Vec<String> {
    let mut changed = Vec::new();
    for (p, m) in next {
        if prev.get(p) != Some(m) {
            changed.push(p.to_string_lossy().into_owned());
        }
    }
    for p in prev.keys() {
        if !next.contains_key(p) {
            changed.push(p.to_string_lossy().into_owned());
        }
    }
    changed.sort();
    changed
}

/// Start a policy-filtered polling watch on `raw_path`, returning a Lua handle `{ truncated, stop }`.
pub(super) async fn start_watch(lua: &Lua, cwd: &Path, raw_path: &str, on_change: Function) -> mlua::Result<Table> {
    let path = resolve(cwd, raw_path);
    crate::exec_ctx::check(lua, "read", &path.to_string_lossy())?;
    let policy = active_policy(lua);
    check_policy(policy.as_ref(), "read", &path, cwd).map_err(mlua::Error::runtime)?;
    let physical = ossandbox::resolve(&path).map_err(mlua::Error::external)?;
    crate::exec_ctx::check(lua, "read", &physical.to_string_lossy())?;
    check_policy(policy.as_ref(), "read", &physical, cwd).map_err(mlua::Error::runtime)?;

    let watch_target = physical.clone();
    let policy_snap = policy.clone();
    let initial = tokio::task::spawn_blocking(move || snapshot_tree(&watch_target, policy_snap.as_ref()))
        .await
        .map_err(mlua::Error::external)?
        .map_err(|e| mlua::Error::runtime(format!("fs.watch `{raw_path}`: {e}")))?;

    let weak_lua = lua.weak();
    let cwd_owned = cwd.to_path_buf();
    let initial_truncated = initial.truncated;
    let task = tokio::task::spawn_local(async move {
        let _guard = WatchDropGuard::new();
        let mut prev = initial.entries;
        // Report initial truncation on the first poll tick after `_bind_id` and the RPC reply complete.
        let mut prev_truncated = false;
        let mut ticker = tokio::time::interval(WATCH_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(lua) = weak_lua.try_upgrade() else {
                break;
            };
            let current_policy = active_policy(&lua);
            let target = physical.clone();
            let next = match tokio::task::spawn_blocking(move || snapshot_tree(&target, current_policy.as_ref())).await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    TreeSnapshot { entries: BTreeMap::new(), truncated: e.kind() != std::io::ErrorKind::NotFound }
                }
                Err(_) => break,
            };
            let changed_paths = diff_snapshots(&prev, &next.entries);
            let became_truncated = next.truncated && !prev_truncated;
            prev = next.entries;
            prev_truncated = next.truncated;
            let _ = &cwd_owned;
            if !changed_paths.is_empty() || became_truncated {
                let Ok(ev) = lua.create_table() else { break };
                let Ok(paths) = lua.create_sequence_from(changed_paths) else { break };
                if ev.set("changed_paths", paths).is_err() {
                    break;
                }
                if ev.set("truncated", next.truncated).is_err() {
                    break;
                }
                if let Err(err) = on_change.call_async::<()>(ev).await {
                    tracing::warn!("fs.watch callback error: {err}");
                    break;
                }
            }
        }
    });

    let task_slot: Rc<RefCell<Option<tokio::task::JoinHandle<()>>>> = Rc::new(RefCell::new(Some(task)));
    let handle = lua.create_table()?;
    handle.set("truncated", initial_truncated)?;
    let stop_slot = Rc::clone(&task_slot);
    handle.set(
        "stop",
        lua.create_async_function(move |_, ()| {
            let slot = Rc::clone(&stop_slot);
            async move {
                let handle = slot.borrow_mut().take();
                if let Some(handle) = handle {
                    handle.abort();
                    let _ = handle.await;
                }
                Ok(())
            }
        })?,
    )?;
    Ok(handle)
}
