//! Connection fiber & remote-hook authorization probe on the cordis tree.
//! Every connection is a fiber of the `rpc` host plugin (`mare_kernel/plugins/rpc.lua`), and
//! services/hooks invoked through [`connection_ctx`] or `service_rpc` belong to that connection's
//! lifetime.

use super::*;

/// The `rpc` host plugin's service, once the tree is booted.
fn rpc_service(host: &Host) -> Option<Table> {
    match host.kernel.service("rpc") {
        Some(mlua::Value::Table(t)) => Some(t),
        _ => None,
    }
}

/// `svc[m](svc, ...)`: the service is cordis's traceable proxy, whose methods are callable tables;
/// settle any returned Cordis Future so disconnect awaits async effect cleanups.
fn invoker(host: &Host) -> mlua::Result<Function> {
    host.lua()
        .load(
            r#"local svc, m = ...
local r = svc[m](svc, select(3, ...))
local ok, cordis = pcall(require, "cordis")
if ok and cordis.future.isThenable(r) then cordis.await(r) end
return r"#,
        )
        .set_name("=rpc-invoke")
        .into_function()
}

struct ConnRemote {
    conn: std::rc::Weak<Conn>,
    host: std::sync::Weak<Host>,
}

impl mare_kernel::RemoteChannel for ConnRemote {
    fn revoked(&self, service: &str, method: &str, _event: &str) -> Option<String> {
        let conn = self.conn.upgrade()?;
        let fallback = conn.rpc.anchor_host();
        let host = self.host.upgrade();
        let host = host.as_deref().or(fallback.as_deref()).unwrap_or(&conn.host);
        match access::authorize(&conn, host, access::Op::HookActive { service, method })
            .into_service_error(service, method)
        {
            Ok(()) => None,
            Err(err) => Some(err.message().to_owned()),
        }
    }

    fn callbacks_active(&self) -> bool {
        self.conn.upgrade().is_some_and(|conn| conn.caps.borrow().callbacks)
    }

    fn subscribe_refused(&self, event: &str) -> Option<String> {
        let conn = self.conn.upgrade()?;
        let fallback = conn.rpc.anchor_host();
        let host = self.host.upgrade();
        let host = host.as_deref().or(fallback.as_deref()).unwrap_or(&conn.host);
        match access::authorize(&conn, host, access::Op::HookEvent(event)).into_error(event) {
            Ok(()) => None,
            Err(err) => Some(err.message().to_owned()),
        }
    }
}

/// On the tree: the connection's fiber and its remote-hook probe on `host`. Returns this host's
/// fiber `uid` (per-VM, matching `mare_lib.caller_id` on `host`). No-op on the Rust engine.
pub(super) async fn connect_host(conn: &Rc<Conn>, _session_id: &str, host: &Arc<Host>, ctx_id: u64) -> Option<u64> {
    let kernel = &host.kernel;
    kernel.set_remote_channel(
        ctx_id,
        Some(Rc::new(ConnRemote { conn: Rc::downgrade(conn), host: Arc::downgrade(host) })),
    );
    if let Some(rpc) = rpc_service(host)
        && let Ok(invoke) = invoker(host)
    {
        match invoke.call_async::<mlua::Value>((rpc, "connect", ctx_id)).await {
            Ok(mlua::Value::Table(fiber)) => return fiber.get::<u64>("uid").ok(),
            Ok(_) => {}
            Err(error) => tracing::warn!("rpc connection fiber: {error}"),
        }
    }
    None
}

/// The connection is gone from `host`: dispose its fiber (and every hook on it) and its probe.
pub(super) async fn disconnect_host(host: &Arc<Host>, ctx_id: u64) {
    if let Some(rpc) = rpc_service(host)
        && let Ok(invoke) = invoker(host)
        && let Err(error) = invoke.call_async::<mlua::Value>((rpc, "disconnect", ctx_id)).await
    {
        tracing::warn!("rpc connection fiber dispose: {error}");
    }
    host.kernel.set_remote_channel(ctx_id, None);
}

/// The connection fiber's cordis ctx on `host` (`rpc:context(id)`), or None.
pub(super) fn connection_ctx_on(host: &Host, ctx_id: u64) -> Option<mlua::Value> {
    let rpc = rpc_service(host)?;
    match invoker(host).ok()?.call::<mlua::Value>((rpc, "context", ctx_id)) {
        Ok(mlua::Value::Nil) | Err(_) => None,
        Ok(ctx) => Some(ctx),
    }
}

/// [`connection_ctx_on`] as the table a host registration takes (`Kernel::host_on` / `host_voter`):
/// an error once the connection is closed.
pub(super) fn connection_table_on(host: &Host, ctx_id: u64) -> mlua::Result<Table> {
    connection_ctx_on(host, ctx_id)
        .and_then(|v| v.as_table().cloned())
        .ok_or_else(|| mlua::Error::runtime("the connection is closed"))
}

/// `services.register` on the tree for `host`/`ctx_id`.
pub(super) async fn provide_on(
    host: &Host,
    ctx_id: u64,
    name: &str,
    implementation: mlua::Value,
    options: Table,
) -> mlua::Result<mlua::Value> {
    let rpc = rpc_service(host).ok_or_else(|| mlua::Error::runtime("the rpc host plugin is not running"))?;
    mare_shared::lua::call_async(
        host.lua(),
        &invoker(host)?,
        (rpc, "provide", ctx_id, name.to_string(), implementation, options),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn release_fn_preserves_a_callable_disposers_future() {
        let lua = mlua::Lua::new();
        let disposer: mlua::Value = lua
            .load(
                r#"
                CLEANED = false
                package.loaded.cordis = {
                    future = { isThenable = function(v) return type(v) == 'table' and v.await ~= nil end },
                    await = function(v) v:await() end,
                }
                return setmetatable({}, { __call = function()
                    return { await = function() CLEANED = true end }
                end })
            "#,
            )
            .eval()
            .unwrap();
        let release: Function =
            lua.load("local r = ... return function() return r() end").set_name("=release").call(disposer).unwrap();
        mare_kernel::release_disposer(&lua, release).await.unwrap();
        assert!(lua.globals().get::<bool>("CLEANED").unwrap(), "the disposer future was not awaited");
    }
}
