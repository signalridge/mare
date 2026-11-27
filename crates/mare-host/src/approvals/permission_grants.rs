//! Child permission increments are recorded by the host, never granted by a child Lua VM.
use mare_binding::ossandbox::{Network, Policy};
use mare_shared::lua::{json_to_lua, lua_to_json};
use mlua::{Lua, Table, Value};
use serde_json::{Value as J, json};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

#[derive(Clone, Default)]
struct Profile {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    network: J,
}

impl Profile {
    fn parse(lua: &Lua, value: &J, cwd: &std::path::Path) -> mlua::Result<Self> {
        Self::parse_paths(lua, value, cwd, true)
    }
    fn parse_answer(lua: &Lua, value: &J, cwd: &std::path::Path) -> mlua::Result<Self> {
        Self::parse_paths(lua, value, cwd, false)
    }
    fn parse_paths(lua: &Lua, value: &J, cwd: &std::path::Path, physical: bool) -> mlua::Result<Self> {
        let object = value.as_object().ok_or_else(|| mlua::Error::runtime("permission profile must be an object"))?;
        if object.keys().any(|k| !matches!(k.as_str(), "read" | "write" | "network")) {
            return Err(mlua::Error::runtime("unknown permission dimension"));
        }
        let paths = |kind: &str| -> mlua::Result<Vec<String>> {
            match object.get(kind) {
                None => Ok(vec![]),
                Some(J::Object(map)) if map.is_empty() => Ok(vec![]),
                Some(J::Array(items)) => items
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .filter(|s| !s.trim().is_empty())
                            .map(str::to_owned)
                            .ok_or_else(|| mlua::Error::runtime("permission paths must be nonempty strings"))
                    })
                    .collect(),
                _ => Err(mlua::Error::runtime("permission paths must be arrays")),
            }
        };
        let mut network = object.get("network").cloned().unwrap_or(J::Bool(false));
        if network.as_object().is_some_and(|o| o.is_empty()) {
            network = J::Array(vec![]);
        }
        let net = net_spec(&network)?;
        let read = paths("read")?;
        let write = paths("write")?;
        let spec = json!({ "level": "workspace-write", "write": if physical { write.clone() } else { vec![] },
            "readable": if physical { read.clone() } else { vec![] }, "network": net });
        let policy = Policy::from_lua(lua, &json_to_lua(lua, &spec)?, cwd)?.unwrap();
        if physical {
            return Ok(Self { read: policy.readable, write: policy.extra, network });
        }
        // The frozen result already carries physical paths. Filesystem replacements must
        // not change an approved target during the final scope commit.
        let canonical = |paths: Vec<String>| -> mlua::Result<Vec<PathBuf>> {
            paths
                .into_iter()
                .map(|path| {
                    let path = PathBuf::from(path);
                    if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                        return Err(mlua::Error::runtime("permission answer paths must be canonical and absolute"));
                    }
                    Ok(path)
                })
                .collect()
        };
        Ok(Self { read: canonical(read)?, write: canonical(write)?, network })
    }
    fn union(&self, other: &Self) -> Self {
        let mut out = self.clone();
        out.read.extend(other.read.iter().cloned());
        out.write.extend(other.write.iter().cloned());
        out.network = match (&self.network, &other.network) {
            (J::Bool(true), _) | (_, J::Bool(true)) => J::Bool(true),
            (a, b) => J::Array(
                a.as_array().into_iter().flatten().chain(b.as_array().into_iter().flatten()).cloned().collect(),
            ),
        };
        out
    }
    fn narrow(&self, other: &Self, lua: &Lua, cwd: &std::path::Path) -> mlua::Result<Self> {
        let paths = |a: &[PathBuf], b: &[PathBuf]| -> Vec<PathBuf> {
            a.iter()
                .flat_map(|a| {
                    b.iter().filter_map(move |b| {
                        if a.starts_with(b) {
                            Some(a.clone())
                        } else if b.starts_with(a) {
                            Some(b.clone())
                        } else {
                            None
                        }
                    })
                })
                .collect()
        };
        let network = match (&self.network, &other.network) {
            (J::Bool(true), b) => b.clone(),
            (a, J::Bool(true)) => a.clone(),
            (J::Array(a), J::Array(b)) => {
                let covers = |patterns: &Vec<J>, host: &J| -> mlua::Result<bool> {
                    let spec = json!({ "level": "read-only", "network": net_spec(&J::Array(patterns.clone()))? });
                    let p = Policy::from_lua(lua, &json_to_lua(lua, &spec)?, cwd)?.unwrap();
                    Ok(p.check("network", host.as_str().unwrap_or_default(), cwd).is_ok())
                };
                let mut hosts = vec![];
                for h in a {
                    if covers(b, h)? {
                        hosts.push(h.clone());
                    }
                }
                for h in b {
                    if covers(a, h)? {
                        hosts.push(h.clone());
                    }
                }
                J::Array(hosts)
            }
            _ => J::Bool(false),
        };
        Ok(Self { read: paths(&self.read, &other.read), write: paths(&self.write, &other.write), network })
    }
    fn apply(&self, lua: &Lua, base: &Policy) -> mlua::Result<Policy> {
        let valid = Self {
            read: self.read.iter().filter_map(|path| mare_binding::ossandbox::validate_read_grant(path).ok()).collect(),
            write: self
                .write
                .iter()
                .filter(|path| path.is_dir())
                .filter_map(|path| mare_binding::ossandbox::validate_read_grant(path).ok())
                .collect(),
            network: self.network.clone(),
        };
        let t = base.to_lua(lua)?;
        let mut paths = base.writable.clone();
        paths.extend(valid.write.iter().cloned());
        if !valid.write.is_empty() {
            t.set("level", "workspace-write")?;
            if base.level == mare_binding::ossandbox::Level::ReadOnly {
                t.set("exclude_tmpdir_env_var", true)?;
                t.set("exclude_slash_tmp", true)?;
            }
        }
        t.set("writable", Value::Nil)?;
        t.set("write", paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>())?;
        let mut read = if base.restrict_read { base.read_roots().unwrap_or_default() } else { base.readable.clone() };
        if base.restrict_read {
            t.set("reads_pinned", true)?;
        }
        read.extend(valid.read.iter().cloned());
        t.set("readable", read.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>())?;
        if valid.network == J::Bool(true) {
            t.set("network", true)?;
        } else if let J::Array(ref hosts) = valid.network
            && !matches!(base.network, Network::Open)
        {
            let mut net = lua_to_json(lua, t.get("network")?)?;
            if !net.is_object() {
                net = json!({ "default": "deny", "allow": [] });
            }
            let mut allow = net["allow"].as_array().cloned().unwrap_or_default();
            allow.extend(hosts.iter().cloned());
            net["allow"] = J::Array(allow);
            t.set("network", json_to_lua(lua, &net)?)?;
        }
        Policy::from_lua(lua, &Value::Table(t), &base.root)?
            .ok_or_else(|| mlua::Error::runtime("permission grant removed sandbox"))
    }
}
fn net_spec(network: &J) -> mlua::Result<J> {
    match network {
        J::Bool(_) => Ok(network.clone()),
        J::Array(hosts) if hosts.iter().all(|h| h.as_str().is_some_and(|s| !s.trim().is_empty())) => {
            Ok(json!({ "default": "deny", "allow": hosts }))
        }
        _ => Err(mlua::Error::runtime("network must be a boolean or host list")),
    }
}
#[derive(Default)]
pub(crate) struct Grants {
    session: Profile,
    turn: Profile,
    pending: std::collections::HashMap<String, (Profile, bool)>,
}
pub(crate) type Store = Rc<RefCell<Grants>>;

pub(crate) fn install(lua: &Lua, kernel: &mare_kernel::Kernel) -> mlua::Result<Store> {
    // listeners of the host's fiber
    let (ctx, owner) = (kernel.host_ctx()?, kernel.host_owner()?);
    let on = |event: &str, f: mlua::Function| kernel.host_on(&ctx, owner, event, f, mare_kernel::HostOn::default());
    let store = Rc::new(RefCell::new(Grants::default()));
    let s = store.clone();
    let end = lua.create_function(move |lua, _: Value| {
        mare_binding::ossandbox::revoke_permission_lifts(lua);
        let mut g = s.borrow_mut();
        g.turn = Profile::default();
        g.pending.clear();
        mare_binding::ossandbox::set_permission_reads(lua, g.session.read.clone());
        Ok(())
    })?;
    on("turn/completed", end)?;
    let s = store.clone();
    let reset = lua.create_function(move |lua, _: Value| {
        *s.borrow_mut() = Grants::default();
        mare_binding::ossandbox::set_permission_reads(lua, vec![]);
        mare_binding::ossandbox::revoke_permission_lifts(lua);
        Ok(())
    })?;
    on("permission/changed", reset.clone())?;
    on("approval/changed", reset.clone())?;
    on("sandbox/changed", reset)?;
    let s = store.clone();
    let lift = lua.create_function(move |lua, call: Table| {
        let Some(base) = mare_binding::ossandbox::ceiling(lua) else { return Ok(false) };
        let g = s.borrow();
        let any = !g.session.read.is_empty()
            || !g.session.write.is_empty()
            || g.session.network == J::Bool(true)
            || g.session.network.as_array().is_some_and(|a| !a.is_empty())
            || !g.turn.read.is_empty()
            || !g.turn.write.is_empty()
            || g.turn.network == J::Bool(true)
            || g.turn.network.as_array().is_some_and(|a| !a.is_empty());
        if !any {
            return Ok(false);
        }
        let limit = g.session.union(&g.turn).apply(lua, &base)?;
        let (Some(id), Some(key)) = (call.get::<Option<String>>("id")?, call.get::<Option<String>>("approval_key")?)
        else {
            return Ok(false);
        };
        if mare_binding::ossandbox::lifted(lua, Some(&id), &key).is_some() {
            return Ok(false);
        }
        mare_binding::ossandbox::lift_permission_ceiling(lua, &id, &key, limit);
        Ok(true)
    })?;
    crate::agent::subagent_table(lua)?.set("permission_lift", lift)?;
    let s = store.clone();
    let commit = lua.create_function(move |lua, (call, permissions, scope): (Table, Value, Option<String>)| {
        let permissions = lua_to_json(lua, permissions)?;
        let Some(id) = call.get::<Option<String>>("id")? else { return Ok(()) };
        let mut g = s.borrow_mut();
        if let Some((granted, session)) = g.pending.remove(&id) {
            let Some(base) = mare_binding::ossandbox::ceiling(lua) else { return Ok(()) };
            let granted = granted.narrow(&Profile::parse_answer(lua, &permissions, &base.root)?, lua, &base.root)?;
            if session && scope.as_deref() == Some(mare_shared::SESSION_SCOPE) {
                g.session = g.session.union(&granted);
            } else if scope.is_some() {
                g.turn = g.turn.union(&granted);
            }
            mare_binding::ossandbox::set_permission_reads(
                lua,
                g.session.read.iter().chain(&g.turn.read).cloned().collect(),
            );
        }
        Ok(())
    })?;
    crate::agent::subagent_table(lua)?.set("permission_commit", commit)?;
    Ok(store)
}

pub(crate) fn record(lua: &Lua, store: &Store, call: &J, info: &J, vote: &J) -> mlua::Result<()> {
    if call.get("arguments").and_then(|a| a.get("additional_permissions")).is_none()
        || vote.get("decision").and_then(J::as_str) != Some("allow")
    {
        return Ok(());
    }
    let Some(base) = mare_binding::ossandbox::ceiling(lua) else { return Ok(()) };
    let args = &call["arguments"];
    let mut granted = Profile::parse(lua, &args["additional_permissions"], &base.root)?;
    for constraint in vote.get("permission_constraints").and_then(J::as_array).into_iter().flatten() {
        granted = granted.narrow(&Profile::parse(lua, constraint, &base.root)?, lua, &base.root)?;
    }
    let session_cap = args["scope"] == mare_shared::SESSION_SCOPE
        || (args.get("scope").is_none() && info.get("permission_request") != Some(&J::Bool(true)));
    let session = session_cap
        && (vote["scope"] == mare_shared::SESSION_SCOPE
            || (vote.get("scope").is_none()
                && matches!(vote["grant"].as_str(), Some("allow_session" | "allow_amend"))));
    let mut g = store.borrow_mut();
    if g.pending.len() >= 64 {
        g.pending.clear();
    }
    if let Some(id) = call.get("id").and_then(J::as_str) {
        if let Some(key) = call.get("approval_key").and_then(J::as_str) {
            mare_binding::ossandbox::stage_permission_reads(lua, id, key, granted.read.clone());
        }
        g.pending.insert(id.to_string(), (granted, session));
    }
    Ok(())
}
