//! Enforcement half of the tool execution context (the tagging lives in
//! [`mare_shared::exec_ctx`]). Primitives call [`check`] (fs, http, net, store, apply_patch) or
//! [`spawn_sandbox`] (proc) with the *calling* thread's context: inside a tool call the
//! effective sandbox policy is enforced by core, whatever the tool plugin does; outside one
//! nothing is checked.
//!
//! Effective policy = the call's own `sandbox` spec (a per-call escalation decided before the
//! call started) or else the session's current policy ([`crate::ossandbox::current`]), with
//! the session's denied reads kept ([`crate::deny_read`]).
//! Every refusal is appended to `ctx.denials` — reliable evidence the permissions layer can
//! build targeted hints on.

use std::path::PathBuf;

use mlua::{Lua, Table, Value};

use crate::ossandbox::{Policy, PolicyUd};
use mare_shared::exec_ctx;
use mare_shared::lua::rt_err;

const FROZEN_KEY: &str = "__mare_frozen_policy";
const SOURCE_KEY: &str = "__mare_denial_sink";
const PARENT_KEY: &str = "__mare_parent_context";

#[derive(Clone)]
struct FrozenPolicy {
    policy: Option<Policy>,
    cwd: PathBuf,
}
impl mlua::UserData for FrozenPolicy {}

pub struct Active {
    pub ctx: Table,
    pub policy: Policy,
    pub cwd: PathBuf,
}

fn ctx_cwd(lua: &Lua, ctx: &Table) -> PathBuf {
    if let Ok(Some(c)) = ctx.get::<Option<String>>("cwd") {
        return PathBuf::from(c);
    }
    lua.globals()
        .get::<Table>("mare")
        .and_then(|m| m.get::<String>("cwd"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
}

/// The calling thread's context and its effective policy; `None` outside tool calls or when no
/// sandbox applies (full access).
pub fn active(lua: &Lua) -> mlua::Result<Option<Active>> {
    let Some(ctx) = exec_ctx::current(lua) else { return Ok(None) };
    if let Some(frozen) = ctx.raw_get::<Option<mlua::AnyUserData>>(FROZEN_KEY)? {
        let frozen = frozen.borrow::<FrozenPolicy>()?.clone();
        return Ok(frozen.policy.map(|policy| Active { ctx, policy, cwd: frozen.cwd }));
    }
    resolve(lua, ctx)
}

fn resolve(lua: &Lua, ctx: Table) -> mlua::Result<Option<Active>> {
    let cwd = ctx_cwd(lua, &ctx);
    // `sandbox = false` in a context never means "off": a vote or tool can't switch
    // the sandbox off by omission or `false`; only an explicit danger-full-access policy (an
    // approved escalation, or the session's own preset) runs unconfined
    let (policy, explicit) = match exec_ctx::field(&ctx, "sandbox") {
        Value::Nil | Value::Boolean(false) => (crate::ossandbox::current(lua), false),
        spec => (Policy::from_lua(lua, &spec, &cwd)?, true),
    };
    // a child call the parent's approval path approved escalates like the main agent's: its own
    // per-call policy, limited by what was approved instead of the inherited ceiling
    let lift = match (explicit, exec_ctx::field(&ctx, "approval_key")) {
        (true, Value::String(key)) => {
            let id = match exec_ctx::field(&ctx, "tool_call_id") {
                Value::String(s) => Some(s.to_string_lossy()),
                _ => None,
            };
            crate::ossandbox::lifted(lua, id.as_deref(), &key.to_string_lossy())
        }
        _ => None,
    };
    let policy = match lift {
        Some(limit) => match (policy, limit) {
            (p, None) => p,
            (Some(p), Some(l)) => Some(l.intersect(&p)),
            (None, Some(l)) => Some(l),
        },
        None => crate::ossandbox::enforce_ceiling(lua, policy),
    };
    // the session's denied reads hold whatever was approved: an escalation stays sandboxed
    //
    let mut read_grants = match exec_ctx::field(&ctx, "read_grants") {
        Value::Nil => vec![],
        Value::Table(paths) => paths
            .sequence_values::<String>()
            .map(|p| {
                p.and_then(|p| {
                    crate::ossandbox::validate_read_grant(&crate::realpath::expand(&p, &cwd)).map_err(rt_err)
                })
            })
            .collect::<mlua::Result<Vec<_>>>()?,
        _ => return Err(rt_err("read_grants must be a list of paths")),
    };
    if crate::ossandbox::ceiling(lua).is_some() {
        let id = ctx.get::<Option<String>>("tool_call_id")?;
        let key = ctx.get::<Option<String>>("approval_key")?;
        let approved: Vec<_> = crate::ossandbox::take_permission_reads(lua, id.as_deref(), key.as_deref())
            .iter()
            .filter_map(|path| crate::ossandbox::validate_read_grant(path).ok())
            .collect();
        read_grants = read_grants
            .iter()
            .flat_map(|requested| {
                approved.iter().filter_map(move |allowed| {
                    if requested.starts_with(allowed) {
                        Some(requested.clone())
                    } else if allowed.starts_with(requested) {
                        Some(allowed.clone())
                    } else {
                        None
                    }
                })
            })
            .collect();
    }
    let policy = crate::deny_read::keep_with_read_grants(lua, policy, &read_grants);
    Ok(policy.map(|policy| Active { ctx, policy, cwd }))
}

/// Copy the caller-owned context and freeze the effective policy before the tool starts.
fn freeze(lua: &Lua, ctx: Table) -> mlua::Result<Table> {
    let cwd = ctx_cwd(lua, &ctx);
    let resolved = resolve(lua, ctx.clone())?;
    let mut policy = resolved.map(|active| active.policy);
    // Declared lifetimes are capability restrictions, never per-call escalation. In
    // particular their default read roots must not replace the selected session whitelist.
    if ctx.get::<Option<String>>("lifetime")?.is_some()
        && let Some(selected) = crate::ossandbox::current(lua)
    {
        policy = Some(match policy {
            Some(requested) => selected.intersect(&requested),
            None => selected,
        });
    }
    if ctx.get::<Option<String>>("lifetime")?.is_some() && policy.as_ref().is_some_and(|p| p.profile.is_some()) {
        return Err(rt_err(
            "EXEC_LIFETIME_INVALID: selected raw sandbox profiles cannot be enforced by an inspection scope",
        ));
    }
    freeze_policy(lua, ctx, cwd, policy, None)
}

/// Freeze a host-query context for `cwd` honoring the session's active sandbox policy
/// (including `deny_read`) and verifying the session is not poisoned.
fn freeze_host_query(lua: &Lua, cwd: &std::path::Path) -> mlua::Result<Table> {
    crate::exec_gate::check_unpoisoned(lua)?;
    let ctx = lua.create_table()?;
    ctx.set("cwd", cwd.to_string_lossy().as_ref())?;
    freeze(lua, ctx)
}

/// Resolve the frozen `(cwd, Option<Active>)` for a host-side query running without a tool
/// execution context, using the exact same freezing logic as [`run_host_query`].
pub(crate) fn host_query_scope(lua: &Lua) -> mlua::Result<(PathBuf, Option<Active>)> {
    let host_cwd = crate::workspace::dirs(lua)
        .into_iter()
        .next()
        .or_else(|| lua.globals().get::<Table>("mare").and_then(|m| m.get::<String>("cwd")).ok().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let frozen = freeze_host_query(lua, &host_cwd)?;
    let fp = frozen
        .raw_get::<Option<mlua::AnyUserData>>(FROZEN_KEY)?
        .ok_or_else(|| rt_err("host query policy was not frozen"))?;
    let fp = fp.borrow::<FrozenPolicy>()?.clone();
    let active = fp.policy.map(|policy| Active { ctx: frozen, policy, cwd: fp.cwd.clone() });
    Ok((fp.cwd, active))
}

/// Check a path against a specific [`Active`] policy using the same check & error mapping as
/// [`check_process_path`].
pub(crate) fn check_active_path(lua: &Lua, active: &Active, op: &str, target: &str) -> mlua::Result<()> {
    reject_nul_path(target)?;
    let path = PathBuf::from(target);
    let path = if path.is_absolute() { path } else { std::env::current_dir().map_err(rt_err)?.join(path) };
    check_with(lua, active, op, &path.to_string_lossy())
}

/// Run `f(args...)` in a frozen execution context for `cwd`, honoring the session's active
/// sandbox policy (including `deny_read`) without requiring a tool-journal call acknowledgement.
pub async fn run_host_query(
    lua: &Lua,
    cwd: &std::path::Path,
    f: mlua::Function,
    args: mlua::MultiValue,
) -> mlua::Result<mlua::MultiValue> {
    let frozen = freeze_host_query(lua, cwd)?;
    exec_ctx::scoped_call(lua, Some(frozen), f, args)?.await
}

/// The documented exception to journal admission (types/mare/proc.lua, `mare.exec.run`): a
/// `lifetime = "ephemeral-read-only"` run whose own sandbox is an explicit read-only policy with
/// every network blocked (and no raw profile) needs no durable start — it can change nothing a
/// journal would have to record. Anything else is admitted only on its acked start.
fn exempt_from_journal(lua: &Lua, ctx: &Table) -> mlua::Result<bool> {
    if !matches!(exec_ctx::field(ctx, "lifetime"), Value::String(s) if s.as_bytes().as_ref() == b"ephemeral-read-only")
    {
        return Ok(false);
    }
    let sandbox = exec_ctx::field(ctx, "sandbox");
    if matches!(sandbox, Value::Nil | Value::Boolean(false)) {
        return Ok(false);
    }
    let cwd = ctx_cwd(lua, ctx);
    Ok(Policy::from_lua(lua, &sandbox, &cwd)?.is_some_and(|p| {
        p.level == crate::ossandbox::Level::ReadOnly
            && p.profile.is_none()
            && matches!(&p.network, crate::ossandbox::Network::Proxy(n) if n.is_deny_all())
    }))
}

/// Journal admission for a run that starts a new call (not one nested in an admitted call).
fn admit(lua: &Lua, ctx: &Table) -> mlua::Result<()> {
    if exempt_from_journal(lua, ctx)? {
        return crate::exec_gate::check_unpoisoned(lua);
    }
    crate::exec_gate::admit(lua, ctx)
}

/// A nested call can narrow the already frozen outer policy; it cannot use its own context or
/// approval key to widen it. In particular, each side that adds deny-read paths must satisfy the
/// in-process `fs_deny_read` check for its own paths.
fn freeze_nested(lua: &Lua, ctx: Table, outer: Table) -> mlua::Result<Table> {
    let inherited = exec_ctx::field(&outer, "lifetime");
    let requested_lifetime = ctx.raw_get::<Value>("lifetime")?;
    if !inherited.is_nil() {
        if (!requested_lifetime.is_nil() && requested_lifetime != inherited)
            || ctx.get::<Option<bool>>("unconfined")?.unwrap_or(false)
        {
            return Err(rt_err(
                "EXEC_LIFETIME_INVALID: execution lifetime cannot be widened or replaced by a nested call",
            ));
        }
    } else if !requested_lifetime.is_nil() {
        admit(lua, &ctx)?;
    }
    let cwd = ctx_cwd(lua, &ctx);
    let outer_policy = match outer.raw_get::<Option<mlua::AnyUserData>>(FROZEN_KEY)? {
        Some(ud) => ud.borrow::<FrozenPolicy>()?.policy.clone(),
        None => resolve(lua, outer.clone())?.map(|a| a.policy),
    };
    let requested = exec_ctx::field(&ctx, "sandbox");
    let inner = match requested {
        Value::Nil | Value::Boolean(false) => None,
        v => Policy::from_lua(lua, &v, &cwd)?,
    };
    if inner.as_ref().is_some_and(|p| p.profile.is_some()) {
        return Err(rt_err("sandbox: raw profile is not allowed in a nested tool call"));
    }
    let policy = match (outer_policy, inner) {
        (Some(a), Some(b)) => Some(a.intersect(&b)),
        (a, b) => a.or(b),
    };
    if (!inherited.is_nil() || !requested_lifetime.is_nil()) && policy.as_ref().is_some_and(|p| p.profile.is_some()) {
        return Err(rt_err(
            "EXEC_LIFETIME_INVALID: selected raw sandbox profiles cannot be enforced by an inspection scope",
        ));
    }
    let frozen = freeze_policy(lua, ctx, cwd, policy, Some(outer))?;
    if !inherited.is_nil() {
        frozen.raw_set("lifetime", inherited)?;
    }
    Ok(frozen)
}

fn freeze_policy(
    lua: &Lua,
    ctx: Table,
    cwd: PathBuf,
    policy: Option<Policy>,
    parent: Option<Table>,
) -> mlua::Result<Table> {
    if missing_deny_read_acceptance(&policy) {
        return Err(denied_err(
            lua,
            format!(
                "{}: in-process tools cannot fully enforce fs_deny_read (including aliases); explicitly accept fs_deny_read as PARTIAL to run",
                crate::ossandbox::SANDBOX_INSUFFICIENT
            ),
        ));
    }
    let snapshot = lua.create_table()?;
    for pair in ctx.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        snapshot.raw_set(key, value)?;
    }
    // Keep the denial sink private: the pipeline reads it only after a refusal occurs.
    snapshot.raw_set(SOURCE_KEY, ctx)?;
    snapshot.raw_set(PARENT_KEY, parent)?;
    snapshot.raw_set(FROZEN_KEY, lua.create_userdata(FrozenPolicy { policy, cwd })?)?;
    Ok(snapshot)
}

fn missing_deny_read_acceptance(policy: &Option<Policy>) -> bool {
    policy
        .as_ref()
        .is_some_and(|p| !p.deny_read.is_empty() && !p.accept_partial.contains(&crate::ossandbox::Cap::FsDenyRead))
}

/// The tool receives a detached read-only view, never the internal execution tag.
fn public_current(lua: &Lua) -> mlua::Result<Option<Table>> {
    let Some(ctx) = exec_ctx::current(lua) else { return Ok(None) };
    let data = lua.create_table()?;
    for key in ["tool", "tool_call_id", "cwd", "unconfined", "lifetime"] {
        data.raw_set(key, ctx.raw_get::<Value>(key)?)?;
    }
    if let Some(ud) = ctx.raw_get::<Option<mlua::AnyUserData>>(FROZEN_KEY)? {
        let frozen = ud.borrow::<FrozenPolicy>()?;
        let value = match &frozen.policy {
            Some(policy) => Value::Table(policy.to_lua(lua)?),
            None => Value::String(lua.create_string("danger-full-access")?),
        };
        data.raw_set("sandbox", value)?;
    }
    let sink = ctx.raw_get::<Option<Table>>(SOURCE_KEY)?.unwrap_or_else(|| ctx.clone());
    if let Some(denials) = sink.raw_get::<Option<Table>>("denials")? {
        let copy = lua.create_table()?;
        for denial in denials.sequence_values::<Table>() {
            let entry = lua.create_table()?;
            for pair in denial?.pairs::<Value, Value>() {
                let (key, value) = pair?;
                entry.raw_set(key, value)?;
            }
            copy.push(entry)?;
        }
        data.raw_set("denials", copy)?;
    }
    let view = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__index", data)?;
    mt.set(
        "__newindex",
        lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(rt_err("execution context is read-only"))
        })?,
    )?;
    mt.set("__metatable", false)?;
    view.set_metatable(Some(mt))?;
    Ok(Some(view))
}

fn record(lua: &Lua, a: &Active, d: &crate::ossandbox::Denial, op: &str) {
    let mut current = Some(a.ctx.clone());
    let (mut nodes, mut sinks, mut lists) =
        (std::collections::HashSet::new(), std::collections::HashSet::new(), std::collections::HashSet::new());
    while let Some(ctx) = current {
        if !nodes.insert(ctx.to_pointer() as usize) {
            break;
        }
        let sink = ctx.raw_get::<Option<Table>>(SOURCE_KEY).ok().flatten().unwrap_or_else(|| ctx.clone());
        if sinks.insert(sink.to_pointer() as usize) {
            let list = match sink.raw_get::<Value>("denials") {
                Ok(Value::Table(l)) => Some(l),
                _ => lua.create_table().ok().filter(|l| sink.raw_set("denials", l.clone()).is_ok()),
            };
            if let Some(list) = list
                && lists.insert(list.to_pointer() as usize)
                && let Ok(t) = d.to_lua(lua)
            {
                let _ = t.set("op", op);
                let _ = list.push(t);
            }
        }
        current = ctx.raw_get::<Option<Table>>(PARENT_KEY).ok().flatten();
    }
}

/// Check one operation for the calling thread. `op` = "write" | "read" | "network".
pub fn check(lua: &Lua, op: &str, target: &str) -> mlua::Result<()> {
    match active(lua)? {
        Some(a) => check_with(lua, &a, op, target),
        None => Ok(()),
    }
}

fn reject_nul_path(target: &str) -> mlua::Result<()> {
    if target.contains('\0') {
        return Err(rt_err("file path contains NUL"));
    }
    Ok(())
}

/// Check a path opened by a native Lua file API, which uses the process cwd.
pub fn check_process_path(lua: &Lua, op: &str, target: &str) -> mlua::Result<()> {
    checked_process_path(lua, op, target).map(|_| ())
}

/// Resolve exactly as a native process-cwd file operation does and use that same path to open.
pub fn checked_process_path(lua: &Lua, op: &str, target: &str) -> mlua::Result<PathBuf> {
    reject_nul_path(target)?;
    let path = PathBuf::from(target);
    let Some(a) = active(lua)? else { return Ok(path) };
    let path = if path.is_absolute() { path } else { std::env::current_dir().map_err(rt_err)?.join(path) };
    check_with(lua, &a, op, &path.to_string_lossy())?;
    Ok(path)
}

/// Anchor a native path to the process cwd while a tool context is active.
pub fn process_path(lua: &Lua, target: &str) -> mlua::Result<PathBuf> {
    reject_nul_path(target)?;
    let path = PathBuf::from(target);
    if active(lua)?.is_none() || path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir().map_err(rt_err)?.join(path))
}

fn denied_err(lua: &Lua, message: String) -> mlua::Error {
    let message = if exec_ctx::is_ephemeral(lua) { format!("ephemeral-read-only: {message}") } else { message };
    mlua::Error::external(mare_shared::errors::Coded { code: "denied", message })
}

/// Like [`check`] with a context captured earlier (use across `.await`: after a suspension
/// the "current thread" is no longer the caller).
pub fn check_with(lua: &Lua, a: &Active, op: &str, target: &str) -> mlua::Result<()> {
    match a.policy.check(op, target, &a.cwd) {
        Ok(()) => Ok(()),
        Err(d) => {
            record(lua, a, &d, op);
            let what = d.path.clone().or(d.host.clone()).unwrap_or_default();
            let msg = match op {
                "network" => format!("sandbox: network access to {what} denied under {} mode", a.policy.level.as_str()),
                _ => format!("sandbox: {op} {what} denied {}", a.policy.denied_marker()),
            };
            Err(denied_err(lua, msg))
        }
    }
}

/// Record a network refusal decided outside [`crate::ossandbox::Policy::check`] (the shared
/// [`crate::netauth`] layer: a redirect hop, a name resolving to a private address) and return
/// the error to raise — the same evidence trail as every other denial.
pub fn deny_network(lua: &Lua, a: &Active, host: &str, reason: &str) -> mlua::Error {
    let d = crate::ossandbox::Denial { kind: "network", path: None, host: Some(host.to_string()) };
    record(lua, a, &d, "network");
    denied_err(
        lua,
        format!("sandbox: network access to {host} denied under {} mode: {reason}", a.policy.level.as_str()),
    )
}

/// Check every path of a multi-path operation (rename, apply_patch) before touching any.
pub fn check_all<'a>(lua: &Lua, op: &str, targets: impl IntoIterator<Item = &'a str>) -> mlua::Result<()> {
    for t in targets {
        check(lua, op, t)?;
    }
    Ok(())
}

/// The `sandbox` value a spawn actually uses. Inside a tool call with an effective policy the
/// context's policy wins over whatever the caller asked for (a tool can't opt out with
/// `sandbox = false`); otherwise the caller's own option stands.
pub fn spawn_sandbox(t: &Table, requested: Value) -> mlua::Result<Value> {
    let Some(lua) = t.weak_lua().try_upgrade() else { return Ok(requested) };
    if exec_ctx::current(&lua).is_some() {
        let has_profile = match &requested {
            Value::Table(spec) => !matches!(spec.get::<Value>("profile")?, Value::Nil),
            Value::UserData(ud) if ud.is::<PolicyUd>() => ud.borrow::<PolicyUd>()?.0.profile.is_some(),
            _ => false,
        };
        if has_profile {
            return Err(rt_err("sandbox: raw profile is not allowed in a tool call"));
        }
    }
    let Some(a) = active(&lua)? else { return Ok(requested) };
    if a.policy.profile.is_some() && !matches!(requested, Value::Nil | Value::Boolean(false)) {
        return Err(rt_err("sandbox: cannot narrow an opaque session profile inside a tool call"));
    }
    let policy = match requested {
        Value::Nil | Value::Boolean(false) => a.policy,
        req => match Policy::from_lua(&lua, &req, &a.cwd)? {
            // A policy reference (such as "current") can also resolve to an opaque profile.
            Some(own) if own.profile.is_some() => {
                return Err(rt_err("sandbox: raw profile is not allowed in a tool call"));
            }
            // the tool's own policy can only narrow the context's: per-dimension intersection
            Some(own) => a.policy.intersect(&own),
            None => a.policy,
        },
    };
    Ok(Value::UserData(lua.create_userdata(PolicyUd(policy))?))
}

/// Refuse a primitive that can't be confined (e.g. a PTY) while a policy is active.
pub fn refuse_unconfinable(lua: &Lua, what: &str) -> mlua::Result<()> {
    match active(lua)? {
        Some(a) => Err(rt_err(format!(
            "sandbox: {what} can't be confined under {} mode; refused (the policy can't be enforced for it)",
            a.policy.level.as_str()
        ))),
        None => Ok(()),
    }
}

/// Refuse a tool operation whose implicit file accesses cannot be checked per path yet.
pub fn refuse_deny_read(lua: &Lua, what: &str) -> mlua::Result<()> {
    if active(lua)?.is_some_and(|a| !a.policy.deny_read.is_empty()) {
        return Err(rt_err(format!("sandbox: {what} cannot be safely confined with deny_read")));
    }
    Ok(())
}

struct BindingEnforcer;

impl exec_ctx::Enforcer for BindingEnforcer {
    fn is_active(&self, lua: &Lua) -> mlua::Result<bool> {
        Ok(active(lua)?.is_some())
    }

    fn check_process_path(&self, lua: &Lua, op: &str, target: &str) -> mlua::Result<()> {
        check_process_path(lua, op, target)
    }

    fn refuse_deny_read(&self, lua: &Lua, what: &str) -> mlua::Result<()> {
        refuse_deny_read(lua, what)
    }
}

/// `mare.exec`: `run(ctx, fn, ...)` runs `fn` in a fresh thread tagged with `ctx` (used by the
/// tools service for every tool call), `current()`, `check(op, target)`; plus context
/// inheritance for `coroutine.create/wrap` and checks on the raw stdlib escape hatches
/// (`io.open`, `io.lines`, `io.output`, `os.remove`, `os.rename`; `io.popen` and
/// `os.execute` are refused while a policy is active — they can't be confined).
// The run thread ends with the function's error exactly as before (the host sees a failed run
// and closes it the same way). A mare.Error the function raised comes back from the thread as its
// text only; the Rust side takes back the value by that text (lua_error) and this wrapper
// raises it.
const RUN: &str = r#"
local raw = ...
local pack, unpack = table.pack, table.unpack
return function(ctx, f, ...)
  local r = pack(raw(ctx, f, ...))
  if not r[1] then error(r[2], 0) end
  return unpack(r, 2, r.n)
end
"#;

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    exec_ctx::install_enforcer(lua, std::rc::Rc::new(BindingEnforcer));
    let t = lua.create_table()?;
    let run = lua.create_async_function(|lua, (ctx, f, args): (Table, mlua::Function, mlua::MultiValue)| {
        // Nested runs narrow the outer policy, and all runs honor the session barrier.
        let unpoisoned = crate::exec_gate::check_unpoisoned(&lua);
        let ctx = match exec_ctx::current(&lua) {
            Some(outer) => freeze_nested(&lua, ctx, outer),
            None => admit(&lua, &ctx).and_then(|()| freeze(&lua, ctx)),
        };
        async move {
            unpoisoned?;
            let ctx = ctx?;
            match exec_ctx::scoped_call(&lua, Some(ctx), f, args)?.await {
                Ok(mut values) => {
                    values.push_front(mlua::Value::Boolean(true));
                    Ok(values)
                }
                Err(error) => match mare_shared::lua_error::recall_mlua_error(&lua, &error) {
                    Some(value) => {
                        Ok(mlua::MultiValue::from_vec(vec![mlua::Value::Boolean(false), mlua::Value::Table(value)]))
                    }
                    None => Err(error),
                },
            }
        }
    })?;
    // A mare.Error the function raised reaches the caller as that value.
    t.set("run", lua.load(RUN).set_name("=mare.exec.run").call::<mlua::Function>(run)?)?;
    t.set("current", lua.create_function(|lua, ()| public_current(lua))?)?;
    // mare.shared(name): one table per name for the life of the VM, the same for every plugin
    // and across plugin reloads and provider replacement (state a contract's providers share,
    // such as the session holds)
    mare.set(
        "shared",
        lua.create_function(|lua, name: String| {
            let key = format!("mare.shared.{name}");
            if let Ok(table) = lua.named_registry_value::<Table>(&key) {
                return Ok(table);
            }
            let table = lua.create_table()?;
            lua.set_named_registry_value(&key, &table)?;
            Ok(table)
        })?,
    )?;
    // mare.error(code, message, data?): a mare.Error value, raised with error(value, 0); here
    // because mare.exec.run is the bridge that has to recall it
    mare.set(
        "error",
        lua.create_function(|lua, (code, message, data): (String, String, mlua::Value)| {
            mare_shared::lua_error::error_value(lua, &code, &message, data)
        })?,
    )?;
    t.set("check", lua.create_function(|lua, (op, target): (String, String)| check(lua, &op, &target))?)?;
    // Lua's native file APIs resolve relative names against the process cwd, not the
    // tool's frozen cwd. Check the same path that the native API will open.
    let check_path =
        lua.create_function(|lua, (op, target): (String, String)| check_process_path(lua, &op, &target))?;
    let reject_nul = lua.create_function(|_, target: String| reject_nul_path(&target))?;
    t.set("refuse_unconfinable", lua.create_function(|lua, what: String| refuse_unconfinable(lua, &what))?)?;
    t.set(
        "check_mutation",
        lua.create_function(|lua, operation: String| mare_shared::exec_ctx::check_mutation(lua, &operation))?,
    )?;
    mare.set("exec", t.clone())?;
    exec_ctx::install_coroutine_inheritance(lua)?;
    lua.load(
        r#"local exec, check_process_path, reject_nul_path = ...
        local open, lines, input, output, remove, rename = io.open, io.lines, io.input, io.output, os.remove, os.rename
        local popen, execute = io.popen, os.execute
        rawset(io, "open", function(path, m)
            reject_nul_path(path)
            -- Update modes can read as well as write. Check before opening, since w+
            -- truncates the file as part of opening it.
            if not m or m:find("r", 1, true) or m:find("+", 1, true) then check_process_path("read", path) end
            if m and (m:find("w", 1, true) or m:find("a", 1, true) or m:find("+", 1, true)) then
                check_process_path("write", path)
            end
            return open(path, m)
        end)
        rawset(io, "lines", function(path, ...) if path then check_process_path("read", path) end return lines(path, ...) end)
        rawset(io, "input", function(f) if type(f) == "string" or type(f) == "number" then check_process_path("read", f) end return input(f) end)
        rawset(io, "output", function(f) if type(f) == "string" or type(f) == "number" then check_process_path("write", f) end return output(f) end)
        rawset(os, "remove", function(path) check_process_path("write", path) return remove(path) end)
        rawset(os, "rename", function(a, b)
            reject_nul_path(a) reject_nul_path(b)
            check_process_path("read", a) check_process_path("read", b)
            check_process_path("write", a) check_process_path("write", b)
            return rename(a, b)
        end)
        if popen then rawset(io, "popen", function(...) exec.refuse_unconfinable("io.popen") return popen(...) end) end
        rawset(os, "execute", function(...) exec.refuse_unconfinable("os.execute") return execute(...) end)"#,
    )
    .set_name("=exec_ctx/stdlib")
    .call::<()>((t, check_path, reject_nul))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Waker};

    use mlua::{Function, Lua, MultiValue};

    fn nested_pending(lua: &Lua) -> Function {
        let globals = lua.globals();
        globals.set("STARTED", false).unwrap();
        globals.set("CLOSED", false).unwrap();
        globals
            .set("pending", lua.create_async_function(|_, ()| std::future::pending::<mlua::Result<()>>()).unwrap())
            .unwrap();
        globals
            .set(
                "closed",
                lua.create_function(|lua, ()| {
                    let ctx = mare_shared::exec_ctx::current(lua);
                    lua.globals().set("CLOSED_CONTEXT", ctx.is_some())?;
                    lua.globals().set("CLOSED_LIFETIME", ctx.and_then(|ctx| ctx.get::<String>("lifetime").ok()))?;
                    lua.globals().set("CLOSED", true)
                })
                .unwrap(),
            )
            .unwrap();
        let inner: Function = lua
            .load(
                r#"return function()
                    local c <close> = setmetatable({}, {__close = function() closed() end})
                    STARTED = true
                    pending()
                end"#,
            )
            .eval()
            .unwrap();
        globals
            .set(
                "nested",
                lua.create_async_function(move |lua, ()| {
                    let inner = inner.clone();
                    async move { mare_shared::lua::call_async::<()>(&lua, &inner, ()).await }
                })
                .unwrap(),
            )
            .unwrap();
        lua.load("return function() nested() end").eval().unwrap()
    }

    fn assert_closed(lua: &Lua, lifetime: Option<&str>) {
        let globals = lua.globals();
        assert!(globals.get::<bool>("CLOSED").unwrap(), "nested scope was not closed on cancellation");
        assert!(globals.get::<bool>("CLOSED_CONTEXT").unwrap());
        assert_eq!(globals.get::<Option<String>>("CLOSED_LIFETIME").unwrap().as_deref(), lifetime);
    }

    #[test]
    fn cancelled_host_query_closes_nested_scope() {
        let lua = Lua::new();
        let f = nested_pending(&lua);
        let mut call = Box::pin(super::run_host_query(&lua, std::path::Path::new("/"), f, MultiValue::new()));
        assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(lua.globals().get::<bool>("STARTED").unwrap());
        assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
        drop(call);
        assert_closed(&lua, None);
    }

    #[test]
    fn cancelled_exec_run_closes_nested_scope() {
        let lua = Lua::new();
        let f = nested_pending(&lua);
        let mare = lua.create_table().unwrap();
        super::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        let run: Function = lua.load("return function(f) mare.exec.run({tool = 'test'}, f) end").eval().unwrap();
        let mut call = Box::pin(run.call_async::<()>(f));
        assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(lua.globals().get::<bool>("STARTED").unwrap());
        assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
        drop(call);
        assert_closed(&lua, None);
    }
}
