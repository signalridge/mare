//! `mare.mcp`: the Model Context Protocol client as a core primitive (builtin
//! `mcp`), on the official Rust SDK `rmcp` — the transport and protocol layer only (Apache-2.0). The
//! Lua `mcp` plugin does configuration, tool registration (`mcp__<server>__<tool>`) and `/mcp`.
//!
//! Lua:
//! - `mare.mcp.connect(spec)` → client: initialize, then tools / resources / templates / prompts
//!   listed in parallel (each at most 16 pages), all within `timeout_ms` (default 60 s, at most
//!   300 s). `spec = { name, command, args?, env?, cwd?, env_include?, env_exclude?, roots? }`
//!   (stdio: own process group, scrubbed env, stderr to `<state>/mcp.log` + a tail) or
//!   `{ name, url, headers?, bearer_token?, oauth? = {key?, scopes?, client_id?,...}, tls? =
//!   {ca_bundle} }` (streamable HTTP). `type = "sse"` is a clear error (rmcp has no SSE client).
//! - `client:tools()`, `client:resources()`, `client:resource_templates()`, `client:prompts()`:
//!   the prefetched lists; `client:tools()` also returns its notification generation, and
//!   `client:tools_generation()` returns the current generation before notification debounce.
//!   `client:refresh()` lists them again (bounded by the startup timeout).
//! - `client:call(name, args, {timeout_ms})`: tools/call; progress resets the active idle timeout; human-input callbacks pause idle and total budgets; on
//!   timeout or when the calling task is dropped `notifications/cancelled` is sent. A connection
//!   that went away (stdio exit, HTTP session lost) is reopened before the call.
//! - `client:read_resource(uri, {timeout_ms})`, `client:get_prompt(name, args, {timeout_ms})`.
//! - `client:next_event()` → `{type = "tools_changed" | "prompts_changed" | "resources_changed"
//!   | "reconnected" | "elicit", id?, params?, cancel?}` or nil once closed; `client:respond(id, result)`
//!   answers an elicitation (`{action = "accept"|"decline"|"cancel", content?}`).
//! - `client:info()`, `client:status()`, `client:stderr()`, `client:reconnect()`,
//!   `client:close()`.
//! - `mare.mcp.oauth_begin(url, key, opts?)` → `{url, redirect, finish(timeout_ms),
//!   paste(url)}`; `mare.mcp.logout(key, url?)`; `mare.mcp.spill(data, ext?)` → path of a private
//!   file holding a long result; `mare.mcp.log(server, level, msg)`; `mare.mcp.log_path`.

mod conn;
mod conn_handler;
mod conn_transport;
mod oauth;

use std::sync::Arc;
use std::time::Duration;

use mare_shared::lua::{json_to_lua, lua_to_json};
use mlua::{Lua, LuaSerdeExt, Table, UserData, UserDataMethods, Value};

pub use oauth::{FileCredentialStore, OAuthLogin, oauth_file};

fn rt_err(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(mare_shared::redact::redact_str(&e.to_string()))
}

fn opt_timeout(opts: &Option<Table>, default: u64) -> Duration {
    Duration::from_millis(
        opts.as_ref().and_then(|o| o.get::<Option<u64>>("timeout_ms").ok().flatten()).unwrap_or(default),
    )
}

fn allow_reconnect(lua: &Lua) -> mlua::Result<bool> {
    Ok(!crate::exec_ctx::active(lua)?.is_some_and(|a| !a.policy.deny_read.is_empty()))
}

pub struct McpClient(Arc<conn::Inner>);

impl UserData for McpClient {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("info", |lua, this, ()| async move { json_to_lua(&lua, &this.0.info().await) });
        m.add_async_method("status", |_, this, ()| async move { Ok(this.0.status().await) });
        m.add_method("tools_generation", |_, this, ()| Ok(this.0.tools_generation()));
        m.add_method("stderr", |_, this, ()| Ok(this.0.stderr_tail()));
        m.add_async_method("tools", |lua, this, ()| async move {
            let listing = this.0.listings().await;
            let tools = listing.tools.as_array().ok_or_else(|| rt_err("MCP tool listing must be an array"))?;
            let result = lua.create_table()?;
            result.set_metatable(Some(lua.array_metatable()))?;
            for (index, tool) in tools.iter().enumerate() {
                let value = match mare_shared::lua::json_to_lua_lossless(&lua, tool) {
                    Ok(value) => value,
                    Err(error) => {
                        // Keep the name so the Lua adapter can diagnose this one tool, while
                        // other tools from the same server remain usable.
                        let item = lua.create_table()?;
                        if let Some(name) = tool.get("name").and_then(serde_json::Value::as_str) {
                            item.set("name", name)?;
                        }
                        item.set("schema_error", error.to_string())?;
                        Value::Table(item)
                    }
                };
                result.raw_set(index + 1, value)?;
            }
            Ok((result, listing.tools_generation))
        });
        // refresh(): list tools, resources and prompts again (after a list_changed
        // notification), bounded by the startup timeout
        m.add_async_method("refresh", |lua, this, ()| async move {
            // Prefetch may itself write host logs, even when the connection is alive.
            crate::exec_ctx::refuse_deny_read(&lua, "mare.mcp.refresh")?;
            this.0.refresh().await.map_err(rt_err)
        });
        m.add_async_method("resources", |lua, this, ()| async move {
            json_to_lua(&lua, &this.0.listings().await.resources)
        });
        m.add_async_method("resource_templates", |lua, this, ()| async move {
            json_to_lua(&lua, &this.0.listings().await.templates)
        });
        m.add_async_method(
            "prompts",
            |lua, this, ()| async move { json_to_lua(&lua, &this.0.listings().await.prompts) },
        );
        m.add_async_method("call", |lua, this, (name, args, opts): (String, Value, Option<Table>)| async move {
            let timeout = opt_timeout(&opts, 120_000);
            let args = match mare_shared::lua::strict_lua_to_json(&lua, args)? {
                serde_json::Value::Object(o) => o,
                _ => return Err(rt_err("MCP tool arguments must be an object")),
            };
            let expected = opts.as_ref().and_then(|o| o.get::<Option<u64>>("tools_generation").ok().flatten());
            let on_progress = opts.as_ref().and_then(|o| o.get::<Option<mlua::Function>>("progress").ok().flatten());
            let peer = this.0.peer(allow_reconnect(&lua)?).await.map_err(rt_err)?;
            let res = if let Some(f) = on_progress {
                let forward = |p: &conn::ProgressUpdate| {
                    if let Ok(t) = lua.create_table() {
                        let _ = t.set("progress", p.progress);
                        if let Some(total) = p.total {
                            let _ = t.set("total", total);
                        }
                        if let Some(msg) = &p.message {
                            let _ = t.set("message", msg.as_str());
                        }
                        let _ = f.call::<()>(t);
                    }
                };
                let (ptx, mut prx) =
                    tokio::sync::mpsc::channel::<conn::ProgressUpdate>(conn::PROGRESS_CAPACITY.min(64));
                let call_fut = conn::call_tool_with_progress(peer, name, args, timeout, &this.0, expected, Some(ptx));
                tokio::pin!(call_fut);
                let out = loop {
                    tokio::select! {
                        biased;
                        Some(p) = prx.recv() => forward(&p),
                        r = &mut call_fut => break r.map_err(rt_err)?,
                    }
                };
                while let Ok(p) = prx.try_recv() {
                    forward(&p);
                }
                out
            } else {
                conn::call_tool(peer, name, args, timeout, &this.0, expected).await.map_err(rt_err)?
            };
            json_to_lua(&lua, &res)
        });
        m.add_async_method("read_resource", |lua, this, (uri, opts): (String, Option<Table>)| async move {
            let timeout = opt_timeout(&opts, 120_000);
            let peer = this.0.peer(allow_reconnect(&lua)?).await.map_err(rt_err)?;
            let params = serde_json::from_value(serde_json::json!({ "uri": uri })).map_err(rt_err)?;
            let r = tokio::time::timeout(timeout, peer.read_resource(params))
                .await
                .map_err(|_| rt_err(format!("resources/read timed out after {}ms", timeout.as_millis())))?
                .map_err(rt_err)?;
            json_to_lua(&lua, &serde_json::to_value(r).map_err(rt_err)?)
        });
        m.add_async_method("get_prompt", |lua, this, (name, args, opts): (String, Value, Option<Table>)| async move {
            let timeout = opt_timeout(&opts, 120_000);
            let args = match lua_to_json(&lua, args)? {
                serde_json::Value::Object(o) => serde_json::Value::Object(o),
                _ => serde_json::Value::Null,
            };
            let peer = this.0.peer(allow_reconnect(&lua)?).await.map_err(rt_err)?;
            let params =
                serde_json::from_value(serde_json::json!({ "name": name, "arguments": args })).map_err(rt_err)?;
            let r = tokio::time::timeout(timeout, peer.get_prompt(params))
                .await
                .map_err(|_| rt_err(format!("prompts/get timed out after {}ms", timeout.as_millis())))?
                .map_err(rt_err)?;
            json_to_lua(&lua, &serde_json::to_value(r).map_err(rt_err)?)
        });
        m.add_async_method("next_event", |lua, this, ()| async move {
            let Some(ev) = this.0.next_event().await else { return Ok(Value::Nil) };
            let v = match ev {
                conn::Event::ToolsChanged => serde_json::json!({ "type": "tools_changed" }),
                conn::Event::PromptsChanged => serde_json::json!({ "type": "prompts_changed" }),
                conn::Event::ResourcesChanged => serde_json::json!({ "type": "resources_changed" }),
                conn::Event::Reconnected => serde_json::json!({ "type": "reconnected" }),
                conn::Event::Elicit { id, params, cancel } => {
                    let event =
                        json_to_lua(&lua, &serde_json::json!({ "type": "elicit", "id": id, "params": params }))?;
                    if let Value::Table(table) = &event {
                        table.set("cancel", crate::task::Token(cancel))?;
                    }
                    return Ok(event);
                }
            };
            json_to_lua(&lua, &v)
        });
        m.add_method("respond", |lua, this, (id, result): (u64, Value)| {
            Ok(this.0.respond(id, lua_to_json(lua, result)?))
        });
        m.add_async_method("reconnect", |lua, this, ()| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.mcp.reconnect")?;
            this.0.reconnect().await.map_err(rt_err)
        });
        m.add_async_method("close", |_, this, ()| async move {
            this.0.close().await;
            Ok(())
        });
    }
}

impl UserData for OAuthLogin {
    fn add_fields<F: mlua::UserDataFields<Self>>(f: &mut F) {
        f.add_field_method_get("url", |_, this| Ok(this.url.clone()));
        f.add_field_method_get("redirect", |_, this| Ok(this.redirect.clone()));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("finish", |lua, this, timeout_ms: Option<u64>| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.mcp.oauth.finish")?;
            this.finish(Duration::from_millis(timeout_ms.unwrap_or(300_000))).await.map_err(rt_err)
        });
        m.add_method("paste", |_, this, url: String| {
            this.paste(url);
            Ok(())
        });
    }
}

/// A private file under `<state>/mcp-output/` holding `text` (the full form of a truncated
/// result; written by mare itself, outside the tool sandbox like bash spill files).
fn spill(data: &[u8], ext: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = mare_shared::paths::state_dir().join("mcp-output");
    let name = format!(
        "mcp-{}-{}-{:08x}.{ext}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0),
        rand::random::<u32>()
    );
    let path = dir.join(name);
    oauth::write_private(&path, data)?;
    Ok(path)
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "connect",
        lua.create_async_function(|lua, (spec, cancel): (Value, Option<Value>)| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.mcp.connect")?;
            let cancel_tok = match cancel {
                Some(Value::UserData(ud)) => Some(ud.borrow::<mare_shared::lua::Token>()?.0.clone()),
                Some(Value::Nil) | None => None,
                Some(_) => return Err(rt_err("connect cancel must be a cancel token")),
            };
            let spec = conn::Spec::parse(lua_to_json(&lua, spec)?).map_err(rt_err)?;
            let inner = conn::Inner::new(spec, Some(crate::trees::Owner::of(&lua)));
            if let Err(e) = inner.start_with_cancel(cancel_tok.as_ref()).await {
                inner.close().await;
                return Err(rt_err(e));
            }
            Ok(McpClient(inner))
        })?,
    )?;
    t.set(
        "oauth_begin",
        lua.create_async_function(|lua, (url, key, opts): (String, String, Value)| async move {
            crate::exec_ctx::refuse_deny_read(&lua, "mare.mcp.oauth_begin")?;
            let opts = oauth::OAuthOpts::from_json(Some(&lua_to_json(&lua, opts)?));
            oauth::begin(url, key, opts).await.map_err(rt_err)
        })?,
    )?;
    t.set(
        "logout",
        lua.create_function(|lua, (key, url): (String, Option<String>)| {
            crate::exec_ctx::refuse_deny_read(lua, "mare.mcp.logout")?;
            oauth::logout(&key, url.as_deref()).map_err(rt_err)
        })?,
    )?;
    t.set(
        "has_credentials",
        lua.create_function(|lua, (key, url): (String, String)| {
            crate::exec_ctx::refuse_deny_read(lua, "mare.mcp.has_credentials")?;
            Ok(oauth::oauth_file(&key, &url).exists())
        })?,
    )?;
    t.set(
        "spill",
        lua.create_function(|lua, (data, ext): (mlua::LuaString, Option<String>)| {
            crate::exec_ctx::refuse_deny_read(lua, "mare.mcp.spill")?;
            let ext: String =
                ext.unwrap_or_else(|| "txt".into()).chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
            spill(&data.as_bytes(), if ext.is_empty() { "bin" } else { &ext })
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(rt_err)
        })?,
    )?;
    t.set(
        "log",
        lua.create_function(|lua, (server, level, msg): (String, String, String)| {
            crate::exec_ctx::refuse_deny_read(lua, "mare.mcp.log")?;
            conn::mcp_log(&server, &level, &msg);
            Ok(())
        })?,
    )?;
    t.set("log_path", conn::log_path().to_string_lossy().into_owned())?;
    t.set("redact", lua.create_function(|_, s: String| Ok(mare_shared::redact::redact_str(&s)))?)?;
    mare.set("mcp", t)?;
    Ok(())
}
