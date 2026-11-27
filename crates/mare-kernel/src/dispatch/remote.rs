//! `prims.rpc`: what the `rpc` and `hooks`
//! host plugins need from Rust. The kernel does not know the transport: a frontend sets, per
//! connection, its grant/capability probe channel, and remote hook eligibility comes from
//! declared contracts + protocol event declarations (`Kernel::declared_remote_hook_mode`).
//!
//!   prims.rpc.remote_mode(event) -> mode | nil
//!   prims.rpc.revoked(conn, event, service, method) -> reason | nil
//!   prims.rpc.callbacks_active(conn) -> boolean
//!   prims.rpc.may_subscribe(conn, event) -> reason | nil
//!   prims.rpc.project(event,...) -> projected_args, token
//!   prims.rpc.restore(token, value) -> restored_value
//!   prims.rpc.with_timeout(ms, fn,...) ->...

use std::cell::RefCell;
use std::collections::HashMap;

use mlua::{Lua, MultiValue, UserData, Value};
use serde_json::Value as Json;

use crate::{Kernel, KernelInner, ServiceCallError};

/// One connection's reverse authorization/capability state, set by its frontend.
pub trait RemoteChannel {
    /// Why the connection's hooks on `event` (registered via `service.method`) are no longer
    /// delivered to (what the session granted is checked again on every delivery: a peer's grant,
    /// `remote_admin`, or the event's publicity withdrawn), or None.
    fn revoked(&self, _service: &str, _method: &str, _event: &str) -> Option<String> {
        None
    }
    /// Whether the connection currently answers `callback.call` (`capabilities.callbacks`).
    fn callbacks_active(&self) -> bool {
        true
    }
    /// Why `hooks.subscribe` must refuse `event` for this connection (e.g. a peer requesting a
    /// non-public event), or None if allowed.
    fn subscribe_refused(&self, _event: &str) -> Option<String> {
        None
    }
}

#[derive(Default)]
struct Remote {
    channels: HashMap<u64, std::rc::Rc<dyn RemoteChannel>>,
}

thread_local! {
    // keyed by kernel (one VM per kernel, all on its thread)
    static REMOTE: RefCell<HashMap<usize, Remote>> = RefCell::new(HashMap::new());
}

fn key(kernel: &std::sync::Weak<KernelInner>) -> usize {
    kernel.as_ptr() as usize
}

struct ProjectedToken(crate::dispatch::runtime_fields::ProjectedEventArgs);

impl UserData for ProjectedToken {}

impl Kernel {
    /// Set (or clear) connection `conn`'s remote hook authorization probe. Clearing the last one
    /// drops the kernel's entry (a frontend sets the hook table again on its next connect).
    pub fn set_remote_channel(&self, conn: u64, channel: Option<std::rc::Rc<dyn RemoteChannel>>) {
        let k = key(&self.weak());
        REMOTE.with(|remote| {
            let mut remote = remote.borrow_mut();
            match channel {
                Some(channel) => {
                    remote.entry(k).or_default().channels.insert(conn, channel);
                }
                None => {
                    if let Some(entry) = remote.get_mut(&k) {
                        entry.channels.remove(&conn);
                        if entry.channels.is_empty() {
                            remote.remove(&k);
                        }
                    }
                }
            }
        });
    }

    /// Drop this kernel's channels (its shutdown): the table is keyed by the
    /// kernel's address, which a later kernel may reuse.
    pub(crate) fn forget_remote(&self) {
        let k = key(&self.weak());
        REMOTE.with(|remote| remote.borrow_mut().remove(&k));
    }
}

fn channel(kernel: &std::sync::Weak<KernelInner>, conn: u64) -> Option<std::rc::Rc<dyn RemoteChannel>> {
    REMOTE.with(|remote| remote.borrow().get(&key(kernel)).and_then(|r| r.channels.get(&conn).cloned()))
}

pub(crate) fn rpc_prim(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("null", Value::NULL)?;
    let k = kernel.clone();
    t.set(
        "revoked",
        lua.create_function(move |_, (conn, event, service, method): (u64, String, String, String)| {
            Ok(channel(&k, conn).and_then(|c| c.revoked(&service, &method, &event)))
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "callbacks_active",
        lua.create_function(move |_, conn: u64| Ok(channel(&k, conn).is_some_and(|c| c.callbacks_active())))?,
    )?;
    let k = kernel.clone();
    t.set(
        "may_subscribe",
        lua.create_function(move |_, (conn, event): (u64, String)| {
            Ok(channel(&k, conn).and_then(|c| c.subscribe_refused(&event)))
        })?,
    )?;
    t.set(
        "project",
        lua.create_function(|lua, (event, args): (String, MultiValue)| {
            let projected = crate::project_event_args(lua, &event, args)
                .map_err(|e| mlua::Error::runtime(format!("`{event}` remote hook: argument conversion failed: {e}")))?;
            let lua_args = mare_shared::lua::json_to_lua_lossless(lua, &Json::Array(projected.json_args.clone()))?;
            let token = lua.create_userdata(ProjectedToken(projected))?;
            Ok((lua_args, token))
        })?,
    )?;
    t.set(
        "restore",
        lua.create_function(|lua, (token, value): (mlua::AnyUserData, Value)| {
            let token = token.borrow::<ProjectedToken>()?;
            let json = mare_shared::lua::strict_lua_to_json(lua, value)?;
            crate::restore_event_result(lua, &token.0, &json)
        })?,
    )?;
    t.set(
        "with_timeout",
        lua.create_async_function(|_, (ms, f, args): (f64, mlua::Function, MultiValue)| async move {
            if !ms.is_finite() || ms < 1.0 {
                return Err(mlua::Error::external(ServiceCallError::Arguments(
                    "timeout_ms must be a positive finite integer".into(),
                )));
            }
            tokio::time::timeout(super::duration_from_positive_ms(ms), f.call_async::<MultiValue>(args)).await.map_err(
                |_| mlua::Error::external(ServiceCallError::coded("timed_out", format!("hook timed out after {ms}ms"))),
            )?
        })?,
    )?;
    let k = kernel;
    t.set(
        "remote_mode",
        lua.create_function(move |_, event: String| {
            Ok(Kernel::from_weak(&k).and_then(|kernel| kernel.declared_remote_hook_mode(&event)).map(str::to_owned))
        })?,
    )?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Null;
    impl RemoteChannel for Null {}

    fn entry(k: &Kernel) -> bool {
        REMOTE.with(|remote| remote.borrow().contains_key(&key(&k.weak())))
    }

    /// audit B4 #9: a kernel's entry in the thread's channel table goes with its last
    /// connection, and with the kernel's shutdown.
    #[tokio::test]
    async fn a_kernels_remote_entry_goes_with_its_last_connection_and_its_shutdown() {
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        k.set_remote_channel(1, Some(std::rc::Rc::new(Null)));
        k.set_remote_channel(2, Some(std::rc::Rc::new(Null)));
        k.set_remote_channel(1, None);
        assert!(entry(&k), "a connection is still open");
        k.set_remote_channel(2, None);
        assert!(!entry(&k), "the last connection was revoked");
        k.set_remote_channel(3, Some(std::rc::Rc::new(Null)));
        k.shutdown().await;
        assert!(!entry(&k), "the kernel shut down");
    }
}
