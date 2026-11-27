//! `mare.proc.sandbox`: the Lua API.

use std::path::{Path, PathBuf};

use mare_shared::lua::rt_err;
use mlua::{Lua, Table, Value};

use super::backend::backend;
use super::caps::{Cap, caps_override};
use super::level::Level;
use super::msys::{STATUS_DLL_INIT_FAILED, is_msys_program, msys_failure};
use super::report::{capability_summary, temp_isolation_note};
use super::roots::root_missing;
use super::session::{PolicyState, current, generation, set_current};
use super::spec::PolicyUd;
use super::{Backend, Policy, SANDBOX_INSUFFICIENT, SANDBOX_UNAVAILABLE};
use crate::realpath::{expand, resolve};

/// `mare.proc.sandbox`: `check`, `policy`, `current`, `set_current`, `marker`,
/// `capabilities`. The same rules as the spawned sandbox.
pub fn install(lua: &Lua, proc_t: &Table, default_cwd: String) -> mlua::Result<()> {
    if lua.app_data_ref::<PolicyState>().is_none() {
        lua.set_app_data(PolicyState::default());
    }
    let t = lua.create_table()?;
    let cwd_of = move |c: Option<String>| PathBuf::from(c.unwrap_or_else(|| default_cwd.clone()));
    register_policy_queries(lua, &t, &cwd_of)?;
    register_snapshot_fns(lua, &t, &cwd_of)?;
    register_session_fns(lua, &t, &cwd_of)?;
    register_info_fns(lua, &t)?;
    proc_t.set("sandbox", t)?;
    Ok(())
}

fn register_policy_queries<F>(lua: &Lua, t: &Table, cwd_of: &F) -> mlua::Result<()>
where
    F: Fn(Option<String>) -> PathBuf + Clone + 'static,
{
    // check(op, target, policy?, cwd?) -> true | false, reason, denial
    // op: "write"|"read"|"network"|"exec"; policy omitted/nil → the current policy
    let c1 = cwd_of.clone();
    t.set(
        "check",
        lua.create_function(move |lua, (op, target, spec, cwd): (String, String, Value, Option<String>)| {
            handle_check(lua, &op, &target, &spec, &c1(cwd))
        })?,
    )?;
    // policy(spec, cwd?) -> normalized policy table | nil when unconfined
    let c2 = cwd_of.clone();
    t.set(
        "policy",
        lua.create_function(move |lua, (spec, cwd): (Value, Option<String>)| {
            Policy::from_lua(lua, &spec, &c2(cwd))?.map(|p| p.to_lua(lua)).transpose()
        })?,
    )?;
    // Check workspace-specific mount preparation before asking for approval. This can briefly
    // create and remove an owned empty mount point, so it is not part of a status-only query.
    let preflight_cwd = cwd_of.clone();
    t.set(
        "preflight_protection",
        lua.create_function(move |lua, (spec, cwd): (Value, Option<String>)| {
            handle_preflight_protection(lua, &spec, &preflight_cwd(cwd))
        })?,
    )?;
    // intersect(a, b, cwd?) -> policy table | nil: per-dimension intersection (nil = the current
    // policy; unconfined on both sides → nil). An approval answer narrows a request with this.
    let c4 = cwd_of.clone();
    t.set(
        "intersect",
        lua.create_function(move |lua, (a, b, cwd): (Value, Value, Option<String>)| {
            handle_intersect(lua, &a, &b, &c4(cwd))
        })?,
    )?;
    // within(path, roots) -> bool: `path` (resolved; a dangling symlink is outside) lies under
    // one of `roots` — subset checks for approved write scopes
    let c5 = cwd_of.clone();
    t.set(
        "within",
        lua.create_function(move |_, (path, roots, opts): (String, Vec<String>, Option<Table>)| {
            handle_within(&path, &roots, opts, &c5(None))
        })?,
    )?;
    Ok(())
}

fn register_snapshot_fns<F>(lua: &Lua, t: &Table, cwd_of: &F) -> mlua::Result<()>
where
    F: Fn(Option<String>) -> PathBuf + Clone + 'static,
{
    // snapshot_current() -> normalized policy spec, opaque host policy, and omitted missing roots.
    // Session recovery uses the userdata with its captured host setter to restore the exact
    // validated policy without reparsing a replaceable Lua table.
    t.set(
        "snapshot_current",
        lua.create_function(|lua, ()| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(rt_err("sandbox: cannot snapshot the session policy inside a tool call"));
            }
            let policy = super::session::enforce_ceiling(lua, current(lua));
            build_snapshot_response(lua, policy)
        })?,
    )?;
    // snapshot_policy(spec) -> normalized policy spec, opaque host Policy, and omitted missing
    // roots. Permissions uses this public policy snapshot API to set the configured default
    // before applying its preset, even if the session service started first with no current policy.
    let snapshot_cwd = cwd_of.clone();
    t.set(
        "snapshot_policy",
        lua.create_function(move |lua, spec: Value| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(rt_err("sandbox: cannot snapshot a policy inside a tool call"));
            }
            let policy = Policy::from_lua(lua, &spec, &snapshot_cwd(None))?;
            let policy = super::session::enforce_ceiling(lua, policy);
            build_snapshot_response(lua, policy)
        })?,
    )?;
    // restore_snapshot(snapshot) -> normalized policy | false, omitted missing roots. Session
    // recovery passes its opaque PolicyUd, so this host path revalidates/filter roots and applies
    // the policy without calling a replaceable Lua `set_current` function.
    t.set("restore_snapshot", lua.create_function(|lua, snapshot: Value| handle_restore_snapshot(lua, &snapshot))?)?;
    // Session recovery may omit only roots that have actually disappeared.  Do not turn
    // permission errors or other invalid roots into missing paths.
    t.set(
        "root_missing",
        lua.create_function(|lua, path: String| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(mare_shared::lua::rt_err("sandbox: cannot inspect session roots inside a tool call"));
            }
            Ok(root_missing(std::path::Path::new(&path)))
        })?,
    )?;
    Ok(())
}

fn register_session_fns<F>(lua: &Lua, t: &Table, cwd_of: &F) -> mlua::Result<()>
where
    F: Fn(Option<String>) -> PathBuf + Clone + 'static,
{
    // generation() -> integer, changes whenever the current policy changes
    t.set("generation", lua.create_function(|lua, ()| Ok(generation(lua)))?)?;
    // current() -> the session policy enforced inside tool calls | nil (unconfined)
    t.set("current", lua.create_function(|lua, ()| current(lua).map(|p| p.to_lua(lua)).transpose())?)?;
    // check_set(spec|nil, cwd?) -> true; validates against ceiling and persistent PTY fence without mutating state
    let c_check = cwd_of.clone();
    t.set(
        "check_set",
        lua.create_function(move |lua, (spec, cwd): (Value, Option<String>)| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(mare_shared::lua::rt_err("sandbox: cannot change session policy inside a tool call"));
            }
            let p = Policy::from_lua(lua, &spec, &c_check(cwd))?;
            let _ = super::session::check_set_current(lua, p)?;
            Ok(true)
        })?,
    )?;
    // set_current(spec|nil, cwd?) -> normalized policy | nil; validates (errors on a bad root);
    // takes effect from the next call
    let c3 = cwd_of.clone();
    t.set(
        "set_current",
        lua.create_function(move |lua, (spec, cwd): (Value, Option<String>)| {
            if mare_shared::exec_ctx::current(lua).is_some() {
                return Err(mare_shared::lua::rt_err("sandbox: cannot change session policy inside a tool call"));
            }
            let p = Policy::from_lua(lua, &spec, &c3(cwd))?;
            let p = set_current(lua, p)?;
            let out = p.as_ref().map(|p| p.to_lua(lua)).transpose()?;
            Ok(out)
        })?,
    )?;
    Ok(())
}

fn register_info_fns(lua: &Lua, t: &Table) -> mlua::Result<()> {
    t.set("marker", lua.create_function(|_, level: String| Ok(Level::parse(&level)?.marker()))?)?;
    // msys_failure{mechanism, code, program?, output?} -> "SANDBOX_MSYS_UNSUPPORTED: …" | nil
    t.set("msys_failure", lua.create_function(|_, o: Table| handle_msys_failure(&o))?)?;
    t.set("levels", vec!["read-only", "workspace-write", "danger-full-access"])?;
    // capabilities(cwd?) -> {platform, mechanism, enforcement, levels, network_filter,
    //                        network_block, restrict_read, temp?}
    t.set("capabilities", lua.create_function(|lua, cwd: Option<String>| handle_capabilities(lua, cwd))?)?;
    Ok(())
}

fn denial_reason(p: &Policy, d: &super::Denial) -> String {
    let what = d.path.clone().or(d.host.clone()).unwrap_or_default();
    match d.kind {
        "write" => {
            let target = Path::new(&what);
            match p.protected_subpaths().into_iter().find(|s| target.starts_with(s)) {
                Some(s) => format!(
                    "{what} is read-only: {} is protected inside the workspace (request `write:{}`)",
                    s.display(),
                    s.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                ),
                None => format!("{what} is outside the writable roots ({})", p.level.as_str()),
            }
        }
        "read" => format!("{what} is not readable"),
        "network" => format!("network access to {what} is not allowed"),
        "insufficient" => format!("{SANDBOX_INSUFFICIENT}: this backend can't enforce {what}"),
        _ => format!("{SANDBOX_UNAVAILABLE}: no sandbox backend on this host"),
    }
}

fn handle_check(
    lua: &Lua,
    op: &str,
    target: &str,
    spec: &Value,
    cwd: &Path,
) -> mlua::Result<(bool, Option<String>, Option<Table>)> {
    if !matches!(op, "write" | "read" | "network" | "exec") {
        return Err(rt_err(format!("sandbox.check: unknown op `{op}` (write|read|network|exec)")));
    }
    let policy = match spec {
        Value::Nil => current(lua),
        s => Policy::from_lua(lua, s, cwd)?,
    };
    let Some(p) = policy else { return Ok((true, None, None)) };
    match p.check(op, target, cwd) {
        Ok(()) => Ok((true, None, None)),
        Err(d) => {
            let why = denial_reason(&p, &d);
            let dt = d.to_lua(lua)?;
            dt.set("marker", p.denial_marker(&d))?;
            Ok((false, Some(why), Some(dt)))
        }
    }
}

fn handle_preflight_protection(lua: &Lua, spec: &Value, cwd: &Path) -> mlua::Result<(bool, Option<String>)> {
    if mare_shared::exec_ctx::current(lua).is_some() {
        return Err(rt_err("sandbox: protection preflight is unavailable inside a tool call"));
    }
    let p = match spec {
        Value::Nil | Value::Boolean(false) => current(lua),
        v => Policy::from_lua(lua, v, cwd)?,
    };
    let Some(p) = p else { return Ok((true, None)) };
    #[cfg(target_os = "linux")]
    if super::backend::backend() == Backend::Bwrap
        && Backend::Bwrap.caps().contains(&Cap::FsProtect)
        && p.required().contains(&Cap::FsProtect)
    {
        return Ok(match super::bwrap_guard::prepare(&p) {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e)),
        });
    }
    let _ = p;
    Ok((true, None))
}

fn filter_missing_roots(policy: &mut Policy) -> mlua::Result<Vec<String>> {
    let mut omitted = Vec::new();
    policy.extra.retain(|root| {
        if root_missing(root) {
            omitted.push(root.to_string_lossy().into_owned());
            false
        } else {
            true
        }
    });
    policy.writable = policy.compute_writable();
    policy.revalidate().map_err(rt_err)?;
    Ok(omitted)
}

fn build_snapshot_response(lua: &Lua, policy: Option<Policy>) -> mlua::Result<(Value, Value, Table)> {
    let Some(mut policy) = policy else {
        return Ok((Value::Boolean(false), Value::Boolean(false), lua.create_table()?));
    };
    let omitted = filter_missing_roots(&mut policy)?;
    let spec = Value::Table(policy.to_lua(lua)?);
    let snapshot = Value::UserData(lua.create_userdata(PolicyUd(policy))?);
    Ok((spec, snapshot, lua.create_sequence_from(omitted)?))
}

fn handle_restore_snapshot(lua: &Lua, snapshot: &Value) -> mlua::Result<(Value, Table, Value)> {
    if mare_shared::exec_ctx::current(lua).is_some() {
        return Err(rt_err("sandbox: cannot restore the session policy inside a tool call"));
    }
    let Some(mut policy) = Policy::from_lua(lua, snapshot, Path::new("."))? else {
        let policy = set_current(lua, None)?;
        let spec = policy.as_ref().map(|p| p.to_lua(lua)).transpose()?.map(Value::Table).unwrap_or(Value::Nil);
        return Ok((spec, lua.create_table()?, Value::Boolean(false)));
    };
    let omitted = filter_missing_roots(&mut policy)?;
    let policy = set_current(lua, Some(policy))?;
    let spec = policy.as_ref().map(|p| p.to_lua(lua)).transpose()?.map(Value::Table).unwrap_or(Value::Nil);
    let snapshot = match policy {
        Some(policy) => Value::UserData(lua.create_userdata(PolicyUd(policy))?),
        None => Value::Boolean(false),
    };
    Ok((spec, lua.create_sequence_from(omitted)?, snapshot))
}

fn handle_intersect(lua: &Lua, a: &Value, b: &Value, cwd: &Path) -> mlua::Result<Option<Table>> {
    let side = |v: &Value| -> mlua::Result<Option<Policy>> {
        match v {
            Value::Nil | Value::Boolean(false) => Ok(current(lua)),
            v => Policy::from_lua(lua, v, cwd),
        }
    };
    let p = match (side(a)?, side(b)?) {
        (Some(x), Some(y)) if x.profile.is_some() || y.profile.is_some() => {
            return Err(mare_shared::lua::rt_err("sandbox: cannot intersect opaque profiles with other policies"));
        }
        (Some(x), Some(y)) => Some(x.intersect(&y)),
        (x, y) => x.or(y),
    };
    p.map(|p| p.to_lua(lua)).transpose()
}

fn handle_within(path: &str, roots: &[String], opts: Option<Table>, cwd: &Path) -> mlua::Result<bool> {
    let p = expand(path, cwd);
    let Ok(p) = resolve(&p) else { return Ok(false) };
    let stable = opts.map(|t| t.get::<Option<bool>>("stable_roots")).transpose()?.flatten().unwrap_or(false);
    Ok(roots.iter().any(|r| {
        let root = expand(r, cwd);
        if stable {
            super::validate_read_grant(&root).is_ok_and(|root| p.starts_with(root))
        } else {
            resolve(&root).is_ok_and(|root| p.starts_with(root))
        }
    }))
}

fn handle_msys_failure(o: &Table) -> mlua::Result<Option<String>> {
    let mechanism: String = o.get::<Option<String>>("mechanism")?.unwrap_or_default();
    let code: Option<i64> = o.get("code")?;
    let output: String = o.get::<Option<String>>("output")?.unwrap_or_default();
    if !matches!(code, Some(c) if c == STATUS_DLL_INIT_FAILED || c == 0xC000_0142) {
        return Ok(None);
    }
    let program = o.get::<Option<String>>("program")?.map(|p| {
        let p = PathBuf::from(p);
        if p.components().count() > 1 { p } else { which::which(&p).unwrap_or(p) }
    });
    let msys = program.is_some_and(|p| is_msys_program(&p, |f| f.is_file()));
    Ok(msys_failure(&mechanism, code, msys, &output))
}

fn handle_capabilities(lua: &Lua, cwd: Option<String>) -> mlua::Result<Table> {
    let b = backend();
    let c = lua.create_table()?;
    c.set("platform", std::env::consts::OS)?;
    c.set("mechanism", b.name())?;
    c.set(
        "enforcement",
        match b {
            Backend::None => "none",
            Backend::Landlock { partial: true } | Backend::WinAcl => "partial",
            _ => "full",
        },
    )?;
    let levels = if b == Backend::None {
        vec!["danger-full-access"]
    } else {
        vec!["read-only", "workspace-write", "danger-full-access"]
    };
    c.set("levels", levels)?;
    // probed per dimension; the booleans below derive from them
    let caps = b.caps();
    c.set("caps", caps.iter().map(|x| x.name()).collect::<Vec<_>>())?;
    c.set("missing", Cap::ALL.iter().filter(|x| !caps.contains(x)).map(|x| x.name()).collect::<Vec<_>>())?;
    c.set("summary", capability_summary().1)?;
    c.set("network_scope", crate::netauth::GUARANTEE)?;
    // MARE_SANDBOX_CAPS narrowed the probed set (tests simulating another platform's tier)
    c.set("simulated", caps_override().is_some())?;
    c.set("network_filter", caps.contains(&Cap::NetProxy))?;
    c.set("network_block", caps.contains(&Cap::NetBlock))?;
    c.set("restrict_read", caps.contains(&Cap::FsRead))?;
    // an approved escalation under restricted reads: kept sandboxed, or refused here
    c.set("escalation_reads", crate::deny_read::capability_line(b, &caps).1)?;
    c.set("explicit_deny_read", crate::deny_read::explicit_capability_line(b, &caps).1)?;
    // a workspace at `cwd` that contains %TEMP% shares its write access with
    // the private temp directory (Windows)
    if let Some(note) = cwd.and_then(|d| resolve(Path::new(&d)).ok()).and_then(|d| temp_isolation_note(&[d])) {
        c.set("temp", note)?;
    }
    Ok(c)
}
