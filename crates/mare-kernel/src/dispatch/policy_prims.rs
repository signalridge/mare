//! `prims.policy`: the fail-closed fold
//! over the voters the `policy` host plugin keeps (mare_kernel/plugins/policy.lua).
//!
//!   prims.policy.decide(point, options,...) -> decision table (Kernel::policy_with)
//!   prims.policy.rewrite(point, call) -> true, arguments | false, reason (Kernel::rewrite)
//!   prims.policy.changed()      a voter or rewriter came or went: the policy epoch moves
//!   prims.policy.attach(lists) the service's `lists(kind, point)` -> its live voters/rewriters of a
//!                               point; `Kernel::policy_with` / `rewrite` fold them
//!   prims.policy.points / prims.policy.rewrite_points the service's points (dispatch/policy.rs)
//!
//! A list from `lists` = `{ {fn = f, owner = name, timeout_ms = n?}, ... }` in the order to run
//! (`timeout_ms`: nil = the default policy timeout, 0 = none); `options` =
//! `{ require_explicit_approval = bool }` (or nil). One decision path: the service's `decide` /
//! `rewrite` and the Rust emitters' `Kernel::policy_with` / `rewrite` are the same fold.

use mlua::{Lua, MultiValue, Table, Value};

use super::policy::{PolicyOptions, Voter};
use crate::{Kernel, KernelInner};

/// `timeout_ms`: nil = the default policy timeout (a voter that never answers still ends in a
/// deny), `0` = none (one that waits for a person), else milliseconds.
fn voters(kernel: &Kernel, list: Table) -> mlua::Result<Vec<Voter>> {
    let mut out = vec![];
    for entry in list.sequence_values::<Table>() {
        let entry = entry?;
        let timeout = match entry.get::<Option<f64>>("timeout_ms")? {
            None => Some(kernel.policy_timeout()),
            Some(0.0) => None,
            Some(ms) if ms.is_finite() && ms > 0.0 => Some(super::duration_from_positive_ms(ms)),
            Some(bad) => {
                return Err(mlua::Error::runtime(format!(
                    "invalid_arguments: policy timeout_ms must be 0 or a positive finite number, got {bad}"
                )));
            }
        };
        out.push(Voter {
            f: entry.get("fn")?,
            owner: entry.get::<Option<String>>("owner")?.unwrap_or_default(),
            timeout,
        });
    }
    Ok(out)
}

const LISTS: &str = "mare.policy.lists";
const POINTS_KEY: &str = "mare.policy.points";
const REWRITE_POINTS_KEY: &str = "mare.policy.rewrite_points";

fn replace_seq(tbl: &Table, items: &[String]) -> mlua::Result<()> {
    let old_len = tbl.raw_len();
    for (idx, item) in items.iter().enumerate() {
        tbl.raw_set(idx + 1, item.as_str())?;
    }
    for idx in (items.len() + 1)..=old_len {
        tbl.raw_set(idx, Value::Nil)?;
    }
    Ok(())
}

/// Refresh `prims.policy.points` and `prims.policy.rewrite_points` in place from loaded contracts.
pub(crate) fn sync_points(kernel: &Kernel) {
    let lua = kernel.lua();
    let (points, rewrite_points) = {
        let points = kernel.policy_points();
        let rewrite_points = kernel.rewrite_points();
        (points, rewrite_points)
    };
    if let Ok(tbl) = lua.named_registry_value::<Table>(POINTS_KEY) {
        let _ = replace_seq(&tbl, &points);
    }
    if let Ok(tbl) = lua.named_registry_value::<Table>(REWRITE_POINTS_KEY) {
        let _ = replace_seq(&tbl, &rewrite_points);
    }
}

/// The policy service's live `kind` ("voters" / "rewriters") of `point`, in the order to run;
/// empty before the service is up or when the point is not listed.
pub(crate) fn service_list(kernel: &Kernel, kind: &str, point: &str) -> mlua::Result<Vec<Voter>> {
    let listed = match kind {
        "voters" => kernel.is_policy_point(point),
        _ => kernel.is_rewrite_point(point),
    };
    if !listed {
        return Ok(vec![]);
    }
    let Ok(lists) = kernel.lua().named_registry_value::<mlua::Function>(LISTS) else { return Ok(vec![]) };
    match lists.call::<Option<Table>>((kind, point))? {
        Some(list) => voters(kernel, list),
        None => Ok(vec![]),
    }
}

pub(crate) fn policy_prim(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "attach",
        lua.create_function(|lua, lists: Option<mlua::Function>| match lists {
            Some(f) => lua.set_named_registry_value(LISTS, f),
            None => lua.unset_named_registry_value(LISTS),
        })?,
    )?;
    let points = lua.create_table()?;
    let rewrite_points = lua.create_table()?;
    lua.set_named_registry_value(POINTS_KEY, points.clone())?;
    lua.set_named_registry_value(REWRITE_POINTS_KEY, rewrite_points.clone())?;
    t.set("points", points)?;
    let k = kernel.clone();
    t.set(
        "timeout_ms",
        lua.create_function(move |_, ()| {
            let ms = Kernel::from_weak(&k)
                .map(|kernel| u64::try_from(kernel.policy_timeout().as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(30_000);
            Ok(ms)
        })?,
    )?;
    // `unconfined(what)`: a voter / rewriter takes part in every later decision, so a sandboxed
    // execution (a model tool's `mare.exec.run`: an active exec_ctx) may not register one
    // . A restricted connection's work is the contract guard's (a grant decides per
    // method, so a peer's permitted remote voter still registers); the sandbox carries no grant, so
    // the guard cannot see it: hence exec_ctx here. Raises a coded `denied`.
    t.set(
        "unconfined",
        lua.create_function(|lua, what: String| {
            if mare_shared::exec_ctx::is_active(lua)? {
                return Err(mlua::Error::external(mare_shared::errors::Coded {
                    code: "denied",
                    message: format!("{what} cannot be registered from a sandboxed execution"),
                }));
            }
            Ok(())
        })?,
    )?;
    t.set("rewrite_points", rewrite_points)?;
    if let Some(k) = Kernel::from_weak(&kernel) {
        sync_points(&k);
    }
    let k = kernel.clone();
    t.set(
        "changed",
        lua.create_function(move |_, ()| {
            if let Some(kernel) = Kernel::from_weak(&k) {
                kernel.bump_policy_epoch();
            }
            Ok(())
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "decide",
        lua.create_async_function(move |_, (point, options, args): (String, Option<Table>, MultiValue)| {
            let k = k.clone();
            async move {
                let kernel = Kernel::from_weak(&k).ok_or_else(|| mlua::Error::runtime("kernel is gone"))?;
                let options = PolicyOptions {
                    require_explicit_approval: options
                        .map(|o| o.get::<Option<bool>>("require_explicit_approval"))
                        .transpose()?
                        .flatten()
                        .unwrap_or(false),
                };
                // the one decision path folds the policy service's voters
                kernel.policy_with(&point, args, options).await
            }
        })?,
    )?;
    t.set(
        "rewrite",
        lua.create_async_function(move |_, (point, call): (String, Table)| {
            let k = kernel.clone();
            async move {
                let kernel = Kernel::from_weak(&k).ok_or_else(|| mlua::Error::runtime("kernel is gone"))?;
                Ok(match kernel.rewrite(&point, call).await? {
                    Ok(args) => (true, args),
                    Err(reason) => (false, Value::String(kernel.lua().create_string(reason)?)),
                })
            }
        })?,
    )?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn voters_only_treats_exact_zero_as_none_and_rejects_negative_or_nan() {
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let make_list = |ms: Value| -> mlua::Result<Vec<Voter>> {
            let entry = k.lua().create_table()?;
            entry.set("fn", k.lua().create_function(|_, ()| Ok("allow"))?)?;
            entry.set("timeout_ms", ms)?;
            let list = k.lua().create_table()?;
            list.set(1, entry)?;
            voters(&k, list)
        };
        assert_eq!(make_list(Value::Integer(0)).unwrap()[0].timeout, None);
        assert_eq!(make_list(Value::Integer(25)).unwrap()[0].timeout, Some(std::time::Duration::from_millis(25)));
        assert!(make_list(Value::Integer(-1)).is_err());
        assert!(make_list(Value::Number(f64::NAN)).is_err());
        assert!(make_list(Value::Number(f64::INFINITY)).is_err());
    }
}
