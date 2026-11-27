//! The Lua `sandbox` spec: parsing it into a [`Policy`] and the round-trippable table.

use std::path::{Path, PathBuf};

use mare_shared::lua::rt_err;
use mlua::{Lua, Table, Value};

use super::level::Level;
use super::session::current;
use super::{Network, Policy};

/// A resolved policy as a Lua value: accepted wherever a `sandbox` spec is (spawn options), so
/// core can force the context's policy onto a process without re-resolving paths.
#[derive(Clone)]
pub struct PolicyUd(pub Policy);
impl mlua::UserData for PolicyUd {}

impl Policy {
    /// Parse the Lua `sandbox` option. `None` = unconfined (absent, `false`,
    /// `danger-full-access`). `"current"` = the session's current policy.
    pub fn from_lua(lua: &Lua, v: &Value, cwd: &Path) -> mlua::Result<Option<Self>> {
        let cfg_fake_ip = crate::netauth::fake_ip_mode_from_lua(lua);
        let apply_fresh = |mut policy: Self| -> Self {
            if let Network::Proxy(np) = &mut policy.network {
                np.fake_ip = cfg_fake_ip;
            }
            policy
        };
        let t = match v {
            Value::UserData(ud) if ud.is::<PolicyUd>() => {
                let mut p = ud.borrow::<PolicyUd>()?.0.clone();
                if let Network::Proxy(np) = &mut p.network {
                    np.tighten_fake_ip(cfg_fake_ip);
                }
                return Ok(Some(p));
            }
            Value::Nil | Value::Boolean(false) => return Ok(None),
            // every dimension at its strictest default: files read-only, network restricted
            Value::Boolean(true) => {
                return Self::build(Level::ReadOnly, cwd, None, &Value::Nil).map(|p| Some(apply_fresh(p)));
            }
            Value::String(s) => {
                let s = s.to_str()?;
                if &*s == "current" {
                    let mut cur = current(lua);
                    if let Some(p) = &mut cur
                        && let Network::Proxy(np) = &mut p.network
                    {
                        np.tighten_fake_ip(cfg_fake_ip);
                    }
                    return Ok(cur);
                }
                return match Level::parse(&s)? {
                    Level::FullAccess => Ok(None),
                    level => Self::build(level, cwd, None, &Value::Nil).map(|p| Some(apply_fresh(p))),
                };
            }
            Value::Table(t) => t,
            _ => return Err(rt_err("sandbox: expected false, a level string, \"current\" or a table")),
        };
        // a missing dimension is its strictest value: files read-only
        let level = match t.get::<Option<String>>("level")? {
            Some(l) => Level::parse(&l)?,
            None => Level::ReadOnly,
        };
        if level == Level::FullAccess {
            if !t.get::<Option<Vec<String>>>("deny_read")?.unwrap_or_default().is_empty() {
                return Err(rt_err("sandbox: danger-full-access cannot enforce deny_read"));
            }
            return Ok(None);
        }
        let root = t.get::<Option<String>>("cwd")?.map(PathBuf::from).unwrap_or_else(|| cwd.to_path_buf());
        let mut policy = apply_fresh(Self::build(level, &root, Some(t), &t.get::<Value>("network")?)?);
        policy.reads_pinned = t.get::<Option<bool>>("reads_pinned")?.unwrap_or(false);
        Ok(Some(policy))
    }

    /// Round-trippable table: feeding it back to `from_lua` / `set_current` gives the same
    /// policy. `roots` lists every effective writable root.
    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        let s = |v: &[PathBuf]| v.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>();
        t.set("level", self.level.as_str())?;
        t.set("cwd", self.root.to_string_lossy().into_owned())?;
        t.set("exclude_tmpdir_env_var", self.exclude_tmpdir_env_var)?;
        t.set("exclude_slash_tmp", self.exclude_slash_tmp)?;
        if self.root_writable {
            t.set("writable", s(&self.extra))?;
        } else {
            // `write` replaces the root set; serializing it as `writable` would add cwd back.
            t.set("write", s(&self.extra))?;
        }
        t.set("roots", s(&self.writable))?;
        t.set("readable", s(&self.readable))?;
        t.set("deny_read", self.deny_read.iter().map(|d| d.input.to_string_lossy().into_owned()).collect::<Vec<_>>())?;
        t.set("restrict_read", self.restrict_read)?;
        if self.reads_pinned {
            t.set("reads_pinned", true)?;
        }
        if let Some(profile) = &self.profile {
            t.set("profile", profile.clone())?;
        }
        match &self.network {
            Network::Open => t.set("network", true)?,
            Network::Proxy(p) if p.is_deny_all() => t.set("network", false)?,
            Network::Proxy(p) => t.set("network", p.to_lua(lua)?)?,
        }
        if !self.unix_sockets.is_empty() {
            t.set("unix_sockets", s(&self.unix_sockets))?;
        }
        if !self.accept_partial.is_empty() {
            t.set("accept_partial", self.accept_partial.iter().map(|c| c.name()).collect::<Vec<_>>())?;
        }
        t.set("unavailable", self.missing(super::backend::backend()).iter().map(|c| c.name()).collect::<Vec<_>>())?;
        t.set("marker", self.level.marker())?;
        Ok(t)
    }
}
