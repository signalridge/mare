//! PTYs retain their launch authority. Policy changes are fenced while an owned tree lives.
use mare_shared::lua::rt_err;
use mlua::Lua;
use std::sync::{Arc, Mutex, Weak};

type ProcessIdentity = Mutex<Option<u32>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyBehavior {
    /// Block `ossandbox::set_current` with `PTY_POLICY_BUSY` while the process tree lives.
    Fence,
    /// Stop the process group automatically when `ossandbox::set_current` commits a policy change.
    AutoStop,
}

struct LeaseEntry {
    pid: Weak<ProcessIdentity>,
    behavior: PolicyBehavior,
}

#[derive(Default, Clone)]
struct Leases(Arc<Mutex<Vec<LeaseEntry>>>);

pub(crate) struct Lease {
    pid: Arc<ProcessIdentity>,
    generation: u64,
    policy: String,
    behavior: PolicyBehavior,
}

fn policy(lua: &Lua) -> mlua::Result<String> {
    if mare_shared::exec_ctx::current(lua).is_some() {
        Ok(format!("{:?}", crate::exec_ctx::active(lua)?.map(|a| a.policy)))
    } else {
        Ok(format!("{:?}", crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua))))
    }
}

impl Lease {
    pub(crate) fn unmanaged() -> Self {
        Self { pid: Arc::new(Mutex::new(None)), generation: 0, policy: String::new(), behavior: PolicyBehavior::Fence }
    }
    pub(crate) fn prepare(lua: &Lua) -> mlua::Result<Self> {
        Self::prepare_with(lua, PolicyBehavior::Fence)
    }
    pub(crate) fn prepare_with(lua: &Lua, behavior: PolicyBehavior) -> mlua::Result<Self> {
        let policy = policy(lua)?;
        let session = format!("{:?}", crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua)));
        // Per-call elevations are not transferable to a persistent input transport.
        if policy != session {
            return Err(rt_err("PTY_POLICY: per-call elevated terminals are not persistent authority"));
        }
        Ok(Self { pid: Arc::new(Mutex::new(None)), generation: crate::ossandbox::generation(lua), policy, behavior })
    }
    pub(crate) fn publish(&mut self, pid: Option<u32>) {
        *self.pid.lock().unwrap() = pid;
    }
    pub(crate) fn check(&self, lua: &Lua) -> mlua::Result<()> {
        if self.generation != crate::ossandbox::generation(lua) || self.policy != policy(lua)? {
            return Err(rt_err("PTY_POLICY_CHANGED: close this terminal and create a new one"));
        }
        Ok(())
    }
    pub(crate) fn register(&self, lua: &Lua) {
        let registry = lua.app_data_ref::<Leases>().map(|l| l.clone()).unwrap_or_else(|| {
            let l = Leases::default();
            lua.set_app_data(l.clone());
            l
        });
        let mut entries = registry.0.lock().unwrap();
        entries.retain(|e| e.pid.strong_count() > 0);
        entries.push(LeaseEntry { pid: Arc::downgrade(&self.pid), behavior: self.behavior });
    }
}

pub(crate) fn fence(lua: &Lua) -> mlua::Result<()> {
    if let Some(registry) = lua.app_data_ref::<Leases>() {
        let mut entries = registry.0.lock().unwrap();
        entries.retain(|e| e.pid.strong_count() > 0);
        if entries
            .iter()
            .filter(|e| e.behavior == PolicyBehavior::Fence)
            .filter_map(|e| e.pid.upgrade())
            .any(|p| crate::platform::tree_alive(*p.lock().unwrap()))
        {
            return Err(rt_err("PTY_POLICY_BUSY: close persistent terminals before changing sandbox policy"));
        }
    }
    Ok(())
}

/// Immediately terminate any `AutoStop` persistent jobs when `ossandbox::set_current` commits a
/// new policy, so they never retain a stale sandbox across policy changes.
pub(crate) fn stop_auto_on_policy_change(lua: &Lua) {
    let pids: Vec<u32> = {
        let Some(registry) = lua.app_data_ref::<Leases>() else { return };
        let mut entries = registry.0.lock().unwrap();
        entries.retain(|e| e.pid.strong_count() > 0);
        entries
            .iter()
            .filter(|e| e.behavior == PolicyBehavior::AutoStop)
            .filter_map(|e| e.pid.upgrade())
            .filter_map(|p| *p.lock().unwrap())
            .collect()
    };
    for pid in pids {
        crate::proc::kill_group(Some(pid), crate::platform::Signal::Kill);
    }
}
