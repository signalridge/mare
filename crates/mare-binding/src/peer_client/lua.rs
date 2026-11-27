//! Public Lua ownership and validation. No plugin or service implementation is inspected.
use std::sync::{Arc, Mutex, Weak};

use mare_shared::lua::{json_to_lua, lua_to_json};
use mlua::{Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use super::*;

const MAX_LUA_CLIENTS: usize = 128;
static LUA_CLIENTS: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

/// Weak references count live Lua clients across VMs without extending their leases.
fn has_capacity(client: Option<&PeerClient>) -> bool {
    let mut clients = LUA_CLIENTS.lock().unwrap();
    clients.retain(|client| client.upgrade().is_some_and(|inner| !inner.closed.is_cancelled()));
    if clients.len() >= MAX_LUA_CLIENTS {
        return false;
    }
    if let Some(client) = client {
        clients.push(Arc::downgrade(&client.inner));
    }
    true
}

fn check(lua: &Lua, client: &PeerClient) -> super::Result<()> {
    crate::exec_ctx::check(lua, "network", &format!("unix:{}", client.path()))
        .map_err(|e| ClientError::new("permission_denied", e.to_string()))
}

pub fn options(opts: &Option<Table>) -> mlua::Result<WaitOptions> {
    let timeout = match opts.as_ref().map(|t| t.get::<Value>("timeout_ms")).transpose()? {
        None | Some(Value::Nil) => Some(Duration::from_secs(5)),
        Some(Value::Boolean(false)) => None,
        Some(Value::Integer(n)) if (0..=3_600_000).contains(&n) => Some(Duration::from_millis(n as u64)),
        _ => return Err(mlua::Error::runtime("timeout_ms must be an integer from 0 to 3600000, or false")),
    };
    Ok(WaitOptions { timeout, cancel: crate::task::opt_token(opts).unwrap_or_default() })
}
fn output(lua: &Lua, result: super::Result<serde_json::Value>) -> mlua::Result<MultiValue> {
    match result {
        Ok(result) => Ok(MultiValue::from_vec(vec![json_to_lua(lua, &result)?])),
        Err(error) => {
            Ok(MultiValue::from_vec(vec![Value::Nil, json_to_lua(lua, &serde_json::to_value(error).unwrap())?]))
        }
    }
}
impl UserData for PeerClient {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("info", |lua, this, ()| json_to_lua(lua, &this.info));
        m.add_async_method("status", |lua, this, opts: Option<Table>| async move {
            if let Err(error) = check(&lua, &this) {
                return output(&lua, Err(error));
            }
            output(&lua, this.status(options(&opts)?).await)
        });
        m.add_async_method("send", |lua, this, (params, opts): (Table, Option<Table>)| async move {
            if let Err(error) = check(&lua, &this) {
                return output(&lua, Err(error));
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                text: String,
                #[serde(default)]
                queue_only: bool,
            }
            let mut value = lua_to_json(&lua, Value::Table(params))?;
            mare_shared::wire_projection::project_json(&lua, &mut value);
            let params: Params = serde_json::from_value(value).map_err(mlua::Error::external)?;
            output(&lua, this.send(params.text, params.queue_only, options(&opts)?).await)
        });
        m.add_async_method("wait_message", |lua, this, (id, opts): (String, Option<Table>)| async move {
            if let Err(error) = check(&lua, &this) {
                return output(&lua, Err(error));
            }
            output(&lua, this.wait_message(&id, options(&opts)?).await)
        });
        m.add_async_method("wait_idle", |lua, this, opts: Option<Table>| async move {
            if let Err(error) = check(&lua, &this) {
                return output(&lua, Err(error));
            }
            let current =
                opts.as_ref().map(|t| t.get::<Option<bool>>("current_or_next")).transpose()?.flatten().unwrap_or(false);
            output(&lua, this.wait_idle(current, options(&opts)?).await)
        });
        m.add_async_method("close", |_, this, ()| async move {
            this.close().await;
            Ok(())
        });
    }
}

pub fn install(lua: &Lua, peers: &Table) -> mlua::Result<()> {
    peers.set(
        "connect",
        lua.create_async_function(|lua, (target, opts): (Value, Value)| async move {
            // Instance targets remain supported; other tables and userdata are old ctx arguments.
            if matches!(&target, Value::UserData(_))
                || matches!(&target, Value::Table(t) if t.raw_get::<Value>("pid")?.is_nil() || t.raw_get::<Value>("started_at_ms")?.is_nil())
            {
                return Err(mlua::Error::runtime("mare.peers.connect(target, opts): the owner argument is gone"));
            }
            let opts = match opts {
                Value::Nil => None,
                Value::Table(opts) => Some(opts),
                _ => return Err(mlua::Error::runtime("mare.peers.connect opts must be a table or nil")),
            };
            let target: Target = serde_json::from_value(lua_to_json(&lua, target)?).map_err(mlua::Error::external)?;
            if let Err(error) = crate::exec_ctx::refuse_deny_read(&lua, "mare.peers.connect") {
                return output(&lua, Err(ClientError::new("permission_denied", error.to_string())));
            }
            let (target, path) = match PeerClient::pin_target(&target) {
                Ok(target) => target,
                Err(error) => return output(&lua, Err(error)),
            };
            if let Err(error) = crate::exec_ctx::check(&lua, "network", &format!("unix:{path}")) {
                return output(&lua, Err(ClientError::new("permission_denied", error.to_string())));
            }
            if !has_capacity(None) {
                return output(&lua, Err(ClientError::new("capacity", "Lua peer-client limit reached")));
            }
            match PeerClient::connect(target, path, options(&opts)?, crate::peers::session_cell(&lua)).await {
                Ok(client) => {
                    // Another connect may have completed while this one awaited its handshake.
                    if !has_capacity(Some(&client)) {
                        client.close().await;
                        return output(&lua, Err(ClientError::new("capacity", "Lua peer-client limit reached")));
                    }
                    Ok(MultiValue::from_vec(vec![Value::UserData(lua.create_userdata(client)?)]))
                }
                Err(error) => output(&lua, Err(error)),
            }
        })?,
    )?;
    Ok(())
}
