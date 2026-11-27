//! The session's current policy, its inherited ceiling and the calls approved above it.
//!
// ---- the session's current policy ---------------------------------------------
// Set by the permissions plugin (`mare.proc.sandbox.set_current`, persisted per session through
// the session plugin); enforced by core for every primitive called inside a tool execution
// context ([`crate::exec_ctx`]). `None` = no sandbox (mare without a permissions plugin
// behaves like `yolo`). Calls in flight keep the policy they started with.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use mlua::Lua;

use super::{Denial, Policy};

/// Each Lua VM owns its policy and generation. Child VMs run on separate threads but must never
/// change their parent's or a sibling's state.
#[derive(Default)]
pub(super) struct PolicyState(Arc<RwLock<PolicySnapshot>>);

/// Thread-safe handle to a VM's [`PolicyState`], allowing a parent host to tighten a running
/// child VM's inherited ceiling and clear stale approval lifts when parent permissions narrow.
#[derive(Clone)]
pub struct PolicyHandle(Arc<RwLock<PolicySnapshot>>);

impl PolicyHandle {
    /// Tighten this VM's inherited ceiling and effective current policy to stay within
    /// `parent_policy`, clearing any stale approval lifts and bumping the policy generation.
    pub fn tighten_ceiling(&self, parent_policy: &Policy) -> bool {
        let mut g = self.0.write().unwrap_or_else(|e| e.into_inner());
        let new_ceiling = match &g.ceiling {
            Some(existing) => existing.intersect(parent_policy),
            None => parent_policy.clone(),
        };
        let new_current = match &g.current {
            Some(cur) => Some(new_ceiling.intersect(cur)),
            None => Some(new_ceiling.clone()),
        };
        let changed = format!("{:?}", g.ceiling) != format!("{:?}", Some(&new_ceiling))
            || format!("{:?}", g.current) != format!("{:?}", new_current)
            || !g.lifts.is_empty()
            || !g.permission_reads.is_empty()
            || !g.read_receipts.is_empty();
        if changed {
            g.ceiling = Some(new_ceiling);
            g.current = new_current;
            g.lifts.clear();
            g.permission_reads.clear();
            g.read_receipts.clear();
            g.generation = g.generation.wrapping_add(1);
        }
        changed
    }

    /// Replace this VM's inherited ceiling and effective current policy with `parent_policy`
    /// when resynchronizing an unpinned child to its parent's current preset.
    pub fn resync_ceiling(&self, parent_policy: Option<Policy>) -> bool {
        let mut g = self.0.write().unwrap_or_else(|e| e.into_inner());
        let changed = format!("{:?}", g.ceiling) != format!("{:?}", parent_policy)
            || format!("{:?}", g.current) != format!("{:?}", parent_policy)
            || !g.lifts.is_empty()
            || !g.permission_reads.is_empty()
            || !g.read_receipts.is_empty();
        if changed {
            g.ceiling = parent_policy.clone();
            g.current = parent_policy;
            g.lifts.clear();
            g.permission_reads.clear();
            g.read_receipts.clear();
            g.generation = g.generation.wrapping_add(1);
        }
        changed
    }
}

pub type PolicyChangeFn = Arc<dyn Fn(Option<&Policy>) + Send + Sync>;

struct PolicyChangeHook(PolicyChangeFn);

/// Obtain a thread-safe handle to this VM's sandbox policy state.
pub fn policy_handle(lua: &Lua) -> Option<PolicyHandle> {
    lua.app_data_ref::<PolicyState>().map(|state| PolicyHandle(state.0.clone()))
}

/// Register a callback invoked whenever this VM's current policy or inherited ceiling changes.
pub fn set_policy_change_hook(lua: &Lua, hook: PolicyChangeFn) {
    lua.set_app_data(PolicyChangeHook(hook));
}

fn notify_policy_change(lua: &Lua, policy: Option<&Policy>) {
    let hook = lua.app_data_ref::<PolicyChangeHook>().map(|h| h.0.clone());
    if let Some(hook) = hook {
        hook(policy);
    }
}

#[derive(Default)]
struct PolicySnapshot {
    current: Option<Policy>,
    generation: u64,
    ceiling: Option<Policy>,
    /// calls the parent's approval path approved above the ceiling (newest last, bounded)
    lifts: Vec<Lift>,
    permission_reads: Vec<PathBuf>,
    read_receipts: Vec<(String, String, Vec<PathBuf>)>,
}

/// it was approved under, not clipped to the inherited ceiling — the same escalation the main
/// agent gets. `call_id = None` covers every identical call (an "allow for this session" or
/// "remember" answer); `policy = None` = approved to run unconfined.
#[derive(Clone)]
struct Lift {
    call_id: Option<String>,
    key: String,
    policy: Option<Policy>,
    permission_scope: bool,
}

const MAX_LIFTS: usize = 64;

pub fn current(lua: &Lua) -> Option<Policy> {
    lua.app_data_ref::<PolicyState>()
        .and_then(|state| state.0.read().unwrap_or_else(|e| e.into_inner()).current.clone())
}

/// The current policy generation (changes whenever `set_current` runs).
pub fn generation(lua: &Lua) -> u64 {
    lua.app_data_ref::<PolicyState>()
        .map(|state| state.0.read().unwrap_or_else(|e| e.into_inner()).generation)
        .unwrap_or_default()
}

/// Replace the current policy; takes effect from the next call.
pub fn ceiling(lua: &Lua) -> Option<Policy> {
    lua.app_data_ref::<PolicyState>()
        .and_then(|state| state.0.read().unwrap_or_else(|e| e.into_inner()).ceiling.clone())
}

/// Install an immutable inherited ceiling before any child plugins are loaded.
pub fn set_inherited_ceiling(lua: &Lua, policy: Option<Policy>) {
    if let Some(state) = lua.app_data_ref::<PolicyState>() {
        let mut g = state.0.write().unwrap_or_else(|e| e.into_inner());
        g.current = policy.clone();
        g.ceiling = policy.clone();
        g.lifts.clear();
        g.permission_reads.clear();
        g.read_receipts.clear();
        g.generation = g.generation.wrapping_add(1);
    }
    notify_policy_change(lua, policy.as_ref());
}

/// Record an approval answered by the parent for a child call (only meaningful under an
/// inherited ceiling; host-side, so nothing in the child VM can grant itself one). `key` is the
/// call's approval key (tool, arguments, cwd); `call_id = None` approves every identical call.
pub fn lift_ceiling(lua: &Lua, call_id: Option<&str>, key: &str, policy: Option<Policy>) {
    if let Some(state) = lua.app_data_ref::<PolicyState>() {
        let mut g = state.0.write().unwrap_or_else(|e| e.into_inner());
        if g.ceiling.is_none() {
            return;
        }
        let call_id = call_id.map(str::to_string);
        g.lifts.retain(|l| !(l.key == key && l.call_id == call_id));
        g.lifts.push(Lift { call_id, key: key.to_string(), policy, permission_scope: false });
        if g.lifts.len() > MAX_LIFTS {
            g.lifts.remove(0);
        }
    }
}

/// Read increments approved through the parent host, separate from legacy command lifts.
/// Lua contexts cannot turn a legacy write/network approval into a read grant.
pub fn set_permission_reads(lua: &Lua, paths: Vec<PathBuf>) {
    if let Some(state) = lua.app_data_ref::<PolicyState>() {
        state.0.write().unwrap_or_else(|e| e.into_inner()).permission_reads = paths;
    }
}

/// Single-call read receipts permit the approved call itself before it commits scope grants.
pub fn stage_permission_reads(lua: &Lua, id: &str, key: &str, paths: Vec<PathBuf>) {
    if let Some(state) = lua.app_data_ref::<PolicyState>() {
        let mut state = state.0.write().unwrap_or_else(|e| e.into_inner());
        state.read_receipts.retain(|(call, hash, _)| call != id || hash != key);
        state.read_receipts.push((id.to_string(), key.to_string(), paths));
        if state.read_receipts.len() > MAX_LIFTS {
            state.read_receipts.remove(0);
        }
    }
}
pub fn take_permission_reads(lua: &Lua, id: Option<&str>, key: Option<&str>) -> Vec<PathBuf> {
    let Some(state) = lua.app_data_ref::<PolicyState>() else { return vec![] };
    let mut state = state.0.write().unwrap_or_else(|e| e.into_inner());
    let mut paths = state.permission_reads.clone();
    if let Some(index) =
        state.read_receipts.iter().position(|(call, hash, _)| Some(call.as_str()) == id && Some(hash.as_str()) == key)
    {
        paths.extend(state.read_receipts.remove(index).2);
    }
    paths
}

/// Scope-based child lifts are rebuilt for each call and revoked at scope boundaries.
pub fn lift_permission_ceiling(lua: &Lua, call_id: &str, key: &str, policy: Policy) {
    lift_ceiling(lua, Some(call_id), key, Some(policy));
    if let Some(state) = lua.app_data_ref::<PolicyState>()
        && let Some(last) = state.0.write().unwrap_or_else(|e| e.into_inner()).lifts.last_mut()
    {
        last.permission_scope = true;
    }
}

pub fn revoke_permission_lifts(lua: &Lua) {
    if let Some(state) = lua.app_data_ref::<PolicyState>() {
        let mut state = state.0.write().unwrap_or_else(|e| e.into_inner());
        state.lifts.retain(|lift| !lift.permission_scope);
        state.read_receipts.clear();
    }
}

/// The approved limit for a call running under a lifted ceiling: `Some(limit)` (where `None`
/// inside means unconfined) or `None` when the ceiling applies as usual.
pub fn lifted(lua: &Lua, call_id: Option<&str>, key: &str) -> Option<Option<Policy>> {
    let state = lua.app_data_ref::<PolicyState>()?;
    let g = state.0.read().unwrap_or_else(|e| e.into_inner());
    let index =
        g.lifts.iter().rposition(|l| l.key == key && (l.call_id.is_none() || l.call_id.as_deref() == call_id))?;
    Some(g.lifts[index].policy.clone())
}

fn enforce_ceiling_with(
    policy: Option<Policy>,
    limit: Option<&Policy>,
    fake_ip: crate::netauth::FakeIpMode,
) -> Option<Policy> {
    let mut effective = match (policy, limit) {
        (Some(p), Some(limit)) => Some(limit.intersect(&p)),
        (None, Some(limit)) => Some(limit.clone()),
        (p, None) => p,
    };
    if let Some(p) = &mut effective
        && let super::Network::Proxy(np) = &mut p.network
    {
        np.tighten_fake_ip(fake_ip);
    }
    effective
}

pub fn enforce_ceiling(lua: &Lua, policy: Option<Policy>) -> Option<Policy> {
    let limit = ceiling(lua);
    let fake_ip = crate::netauth::fake_ip_mode_from_lua(lua);
    enforce_ceiling_with(policy, limit.as_ref(), fake_ip)
}

/// Validate `p` against active policy ceiling and persistent PTY fence without mutating state.
pub fn check_set_current(lua: &Lua, p: Option<Policy>) -> mlua::Result<Option<Policy>> {
    let p = enforce_ceiling(lua, p);
    if format!("{:?}", current(lua)) != format!("{:?}", p) {
        crate::pty::lease::fence(lua)?;
    }
    Ok(p)
}

#[cfg(test)]
std::thread_local! {
    static BEFORE_SET_CURRENT_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_current(lua: &Lua, p: Option<Policy>) -> mlua::Result<Option<Policy>> {
    let checked = check_set_current(lua, p.clone())?;
    #[cfg(test)]
    if let Some(hook) = BEFORE_SET_CURRENT_COMMIT.with(|cell| cell.borrow_mut().take()) {
        hook();
    }
    let fake_ip = crate::netauth::fake_ip_mode_from_lua(lua);
    let (committed, changed) = if let Some(state) = lua.app_data_ref::<PolicyState>() {
        let mut g = state.0.write().unwrap_or_else(|e| e.into_inner());
        let effective = enforce_ceiling_with(p, g.ceiling.as_ref(), fake_ip);
        if format!("{:?}", g.current) == format!("{:?}", effective) {
            (effective, false)
        } else {
            g.current = effective.clone();
            g.generation = g.generation.wrapping_add(1);
            (effective, true)
        }
    } else {
        (checked, false)
    };
    if changed {
        notify_policy_change(lua, committed.as_ref());
        crate::pty::lease::stop_auto_on_policy_change(lua);
    }
    Ok(committed)
}

/// Check against the current policy (always allowed when unconfined).
pub fn check_current(lua: &Lua, op: &str, target: &str, cwd: &Path) -> Result<(), Denial> {
    match current(lua) {
        Some(p) => p.check(op, target, cwd),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn ro_policy(cwd: &Path) -> Policy {
        Policy {
            level: super::super::Level::ReadOnly,
            root: cwd.to_path_buf(),
            extra: vec![],
            root_writable: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            writable: vec![],
            readable: vec![],
            deny_read: vec![],
            restrict_read: false,
            reads_pinned: false,
            network: super::super::Network::Open,
            profile: None,
            unix_sockets: vec![],
            accept_partial: vec![],
            project_protected: vec![],
        }
    }

    #[test]
    fn set_current_reapplies_ceiling_atomically_when_ancestor_tightens_before_commit() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        let proc = lua.create_table().unwrap();
        let cwd = std::env::temp_dir();
        super::super::install(&lua, &proc, cwd.to_string_lossy().to_string()).unwrap();
        mare.set("proc", proc).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let handle = policy_handle(&lua).unwrap();
        let ro = ro_policy(&cwd);
        let at_commit = Arc::new(Barrier::new(2));
        let tightened = Arc::new(Barrier::new(2));

        let b1 = at_commit.clone();
        let b2 = tightened.clone();
        BEFORE_SET_CURRENT_COMMIT.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move || {
                b1.wait();
                b2.wait();
            }));
        });

        let t = std::thread::spawn(move || {
            at_commit.wait();
            assert!(handle.tighten_ceiling(&ro));
            tightened.wait();
        });

        // Attempt to commit FULL (`None`); ancestor tightens to read-only after check_set_current
        // and before the write lock in set_current. The write lock must re-clip against the latest
        // ceiling instead of overwriting g.current with `None`.
        let committed = set_current(&lua, None).unwrap();
        t.join().unwrap();

        assert_eq!(
            committed.as_ref().map(|p| p.level),
            Some(super::super::Level::ReadOnly),
            "set_current must return the re-clipped read-only policy"
        );
        assert_eq!(
            current(&lua).as_ref().map(|p| p.level),
            Some(super::super::Level::ReadOnly),
            "g.current must never revert to None after concurrent ancestor tightening"
        );
        assert_eq!(ceiling(&lua).as_ref().map(|p| p.level), Some(super::super::Level::ReadOnly),);
    }
}
