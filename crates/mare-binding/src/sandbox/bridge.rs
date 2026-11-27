//! Explicit capability transfer: no parent VM references cross the boundary.
use super::limits::Limits;
use mare_shared::lua::{json_to_lua_lossless, strict_lua_to_json};
use mlua::{Function, Lua, MultiValue, Table, Value};

pub(super) fn copy(from: &Lua, to: &Lua, value: Value, limits: &Limits) -> mlua::Result<Value> {
    let value = strict_lua_to_json(from, value)?;
    limits.message_size(super::output::json_size(&value).map_err(mlua::Error::external)?)?;
    json_to_lua_lossless(to, &value)
}

fn function(parent: &Lua, child: &Lua, f: Function, limits: Limits) -> mlua::Result<Function> {
    let ectx = mare_shared::exec_ctx::current(parent);
    let parent = parent.clone();
    child.create_async_function(move |child, args: MultiValue| {
        let (f, parent, ectx, limits) = (f.clone(), parent.clone(), ectx.clone(), limits.clone());
        async move {
            limits.check()?;
            let mut pargs = MultiValue::new();
            for a in args {
                pargs.push_back(copy(&child, &parent, a, &limits)?);
            }
            // Host operations retain the creating tool's confinement. They must implement
            // cooperative cancellation; the VM cannot undo an external side effect.
            let res = mare_shared::exec_ctx::call_in::<MultiValue>(&parent, ectx, f, pargs).await?;
            limits.check()?;
            let mut out = MultiValue::new();
            for r in res {
                out.push_back(copy(&parent, &child, r, &limits)?);
            }
            Ok(out)
        }
    })
}

pub(super) fn expose(parent: &Lua, child: &Lua, src: &Table, limits: &Limits) -> mlua::Result<Table> {
    fn walk(
        parent: &Lua,
        child: &Lua,
        src: &Table,
        limits: &Limits,
        stack: &mut Vec<usize>,
        depth: usize,
    ) -> mlua::Result<Table> {
        if depth > 32 || stack.contains(&(src.to_pointer() as usize)) {
            return Err(mlua::Error::runtime("sandbox: recursive or excessively deep exposure"));
        }
        if src.metatable().is_some() {
            return Err(mlua::Error::runtime("sandbox: exposed table must not have a metatable"));
        }
        stack.push(src.to_pointer() as usize);
        let dst = child.create_table()?;
        for p in src.pairs::<String, Value>() {
            let (k, v) = p?;
            let v = match v {
                Value::Function(f) => Value::Function(function(parent, child, f, limits.clone())?),
                Value::Table(t) => {
                    if strict_lua_to_json(parent, Value::Table(t.clone())).is_ok() {
                        copy(parent, child, Value::Table(t), limits)?
                    } else {
                        Value::Table(walk(parent, child, &t, limits, stack, depth + 1)?)
                    }
                }
                v => copy(parent, child, v, limits)?,
            };
            dst.set(k, v)?;
        }
        stack.pop();
        Ok(dst)
    }
    walk(parent, child, src, limits, &mut vec![], 0)
}
