use mare_shared::lua::{HISTORY_LOSSLESS_MAX_NODES, json_to_lua_lossless_bounded, strict_lua_to_json_bounded};
use mare_shared::schema::json_equal;
use mlua::{Lua, LuaSerdeExt, MultiValue, Table, Value};
use tokio_util::sync::CancellationToken;

/// Projected runtime-only fields and table identities captured before converting an event's
/// table argument to strict JSON, so a hook's JSON return value can restore runtime fields
/// and preserve Lua table identity for unchanged `model` / `tools` declarations.
pub struct ProjectedTable {
    fields: Vec<(String, Value)>,
    operation_fields: Vec<(String, Value)>,
    model: Option<(Value, serde_json::Value)>,
    /// the `model` field lost function-valued parameter rules in the projection
    model_lossy: bool,
    /// the whole argument is a model that lost function-valued rules: the original and what was sent
    whole: Option<(Value, serde_json::Value)>,
    tools: Vec<(Value, serde_json::Value)>,
}

const LOSSY_MODEL: &str = "hook return value changed a model whose parameter rules hold functions: \
                           the functions cannot be sent to the hook, so its change cannot be merged; \
                           return the model unchanged";

/// Event arguments projected to strict JSON alongside any extracted cancellation token and
/// runtime-only table metadata needed to restore a hook's return value.
pub struct ProjectedEventArgs {
    pub json_args: Vec<serde_json::Value>,
    pub cancel_token: Option<CancellationToken>,
    first_table: Option<ProjectedTable>,
}

fn is_task_token(value: &Value) -> bool {
    matches!(value, Value::UserData(ud) if ud.borrow::<mare_shared::lua::Token>().is_ok())
}

fn is_runtime_field(key: &str, value: &Value) -> bool {
    is_task_token(value)
        || (matches!(key, "_rebuild_tool_declarations" | "on_attempt") && matches!(value, Value::Function(_)))
}

fn collect_runtime_fields(table: &Table) -> mlua::Result<Vec<(String, Value)>> {
    let mut fields = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (k, value) = pair?;
        if let Value::String(s) = k
            && let Ok(key) = s.to_str()
            && is_runtime_field(&key, &value)
        {
            fields.push((key.to_owned(), value));
        }
    }
    Ok(fields)
}

fn copy_table_with_overrides(
    lua: &Lua,
    table: &Table,
    metatable: Option<Table>,
    mut override_entry: impl FnMut(&Value, Value) -> mlua::Result<Option<Value>>,
) -> mlua::Result<Table> {
    let copy = lua.create_table()?;
    for pair in table.pairs::<Value, Value>() {
        let (k, v) = pair?;
        if let Some(new_v) = override_entry(&k, v)? {
            copy.raw_set(k, new_v)?;
        }
    }
    if let Some(mt) = metatable {
        copy.set_metatable(Some(mt))?;
    }
    Ok(copy)
}

fn is_bool_wire_key(key: &Value) -> bool {
    match key {
        Value::Boolean(_) => true,
        Value::String(s) => s.to_str().is_ok_and(|k| k == "true" || k == "false"),
        _ => false,
    }
}

/// A bool parameter's per-route mapping is keyed by the value: `[true]` / `[false]` (or their
/// string forms, which `P.mapping` falls back to). Project boolean keys to their string form and
/// drop function mappings; a boolean key wins over its string twin, as it does in `P.mapping`.
fn project_bool_wire_map(lua: &Lua, map: &Table) -> mlua::Result<Option<Table>> {
    if map.metatable().is_some() {
        return Ok(None);
    }
    let mut changed = false;
    let mut bool_keys = [false; 2];
    for pair in map.pairs::<Value, Value>() {
        let (k, v) = pair?;
        if let Value::Boolean(b) = k {
            bool_keys[usize::from(b)] = true;
            changed = true;
        } else if is_bool_wire_key(&k) && matches!(v, Value::Function(_)) {
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    let copy = lua.create_table()?;
    for pair in map.pairs::<Value, Value>() {
        let (k, v) = pair?;
        if is_bool_wire_key(&k) && matches!(v, Value::Function(_)) {
            continue;
        }
        match k {
            Value::Boolean(b) => copy.raw_set(if b { "true" } else { "false" }, v)?,
            Value::String(ref s)
                if s.to_str().is_ok_and(|k| bool_keys[usize::from(k == "true")] && (k == "true" || k == "false")) => {}
            k => copy.raw_set(k, v)?,
        }
    }
    Ok(Some(copy))
}

fn project_wire_table(lua: &Lua, wire: &Table, allow_bool_map: bool) -> mlua::Result<Option<Table>> {
    if wire.metatable().is_some() {
        return Ok(None);
    }
    let mut overrides: Vec<(String, Option<Value>)> = Vec::new();
    for pair in wire.pairs::<Value, Value>() {
        let (k, v) = pair?;
        let Value::String(ref s) = k else {
            continue;
        };
        let Ok(route) = s.to_str() else {
            continue;
        };
        if matches!(v, Value::Function(_)) {
            overrides.push((route.to_string(), None));
        } else if allow_bool_map
            && let Value::Table(ref bool_map) = v
            && let Some(projected) = project_bool_wire_map(lua, bool_map)?
        {
            overrides.push((route.to_string(), Some(Value::Table(projected))));
        }
    }
    if overrides.is_empty() {
        return Ok(None);
    }
    copy_table_with_overrides(lua, wire, None, |k, v| {
        if let Value::String(s) = k
            && let Ok(route) = s.to_str()
            && let Some((_, replacement)) = overrides.iter().find(|(name, _)| name == &*route)
        {
            return Ok(replacement.clone());
        }
        Ok(Some(v))
    })
    .map(Some)
}

fn project_options_table(lua: &Lua, options: &Table) -> mlua::Result<Option<Table>> {
    let mt = match options.metatable() {
        None => None,
        Some(mt) if mt == lua.array_metatable() => Some(mt),
        Some(_) => return Ok(None),
    };
    let mut updates: Vec<(usize, Table)> = Vec::new();
    for pair in options.pairs::<Value, Value>() {
        let (k, v) = pair?;
        let (Value::Integer(idx), Value::Table(opt_table)) = (k, v) else {
            continue;
        };
        if opt_table.metatable().is_some() {
            continue;
        }
        if let Value::Table(opt_wire) = opt_table.raw_get::<Value>("wire")?
            && let Some(projected_wire) = project_wire_table(lua, &opt_wire, false)?
        {
            let projected_opt = copy_table_with_overrides(lua, &opt_table, None, |ok, ov| {
                if let Value::String(s) = ok
                    && s.to_str().is_ok_and(|name| name == "wire")
                {
                    return Ok(Some(Value::Table(projected_wire.clone())));
                }
                Ok(Some(ov))
            })?;
            updates.push((idx as usize, projected_opt));
        }
    }
    if updates.is_empty() {
        return Ok(None);
    }
    copy_table_with_overrides(lua, options, mt, |k, v| {
        if let Value::Integer(idx) = k
            && let Some((_, updated)) = updates.iter().find(|(i, _)| *i == *idx as usize)
        {
            return Ok(Some(Value::Table(updated.clone())));
        }
        Ok(Some(v))
    })
    .map(Some)
}

fn project_param_def_table(lua: &Lua, def: &Table) -> mlua::Result<Option<Table>> {
    if def.metatable().is_some() {
        return Ok(None);
    }
    let projected_wire = match def.raw_get::<Value>("wire")? {
        Value::Table(wire) => project_wire_table(lua, &wire, true)?,
        _ => None,
    };
    let projected_options = match def.raw_get::<Value>(mare_shared::OPTIONS_KEY)? {
        Value::Table(options) => project_options_table(lua, &options)?,
        _ => None,
    };
    if projected_wire.is_none() && projected_options.is_none() {
        return Ok(None);
    }
    copy_table_with_overrides(lua, def, None, |k, v| {
        if let Value::String(s) = k
            && let Ok(name) = s.to_str()
        {
            if name == "wire"
                && let Some(ref w) = projected_wire
            {
                return Ok(Some(Value::Table(w.clone())));
            }
            if name == mare_shared::OPTIONS_KEY
                && let Some(ref o) = projected_options
            {
                return Ok(Some(Value::Table(o.clone())));
            }
        }
        Ok(Some(v))
    })
    .map(Some)
}

/// Whether a model's parameter rules hold functions (`params.*.wire.*`, `options[*].wire.*`, a
/// bool map's values): what a projection leaves out. A projection may also only rename a bool
/// map's keys for JSON, which loses nothing.
fn param_rules_hold_functions(model: &Table) -> mlua::Result<bool> {
    fn wire_holds(wire: &Table) -> mlua::Result<bool> {
        for pair in wire.pairs::<Value, Value>() {
            match pair?.1 {
                Value::Function(_) => return Ok(true),
                Value::Table(map) => {
                    for entry in map.pairs::<Value, Value>() {
                        if matches!(entry?.1, Value::Function(_)) {
                            return Ok(true);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }
    let Value::Table(params) = model.raw_get::<Value>("params")? else { return Ok(false) };
    for pair in params.pairs::<Value, Value>() {
        let Value::Table(def) = pair?.1 else { continue };
        if let Value::Table(wire) = def.raw_get::<Value>("wire")?
            && wire_holds(&wire)?
        {
            return Ok(true);
        }
        if let Value::Table(options) = def.raw_get::<Value>(mare_shared::OPTIONS_KEY)? {
            for opt in options.pairs::<Value, Value>() {
                if let Value::Table(opt) = opt?.1
                    && let Value::Table(wire) = opt.raw_get::<Value>("wire")?
                    && wire_holds(&wire)?
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn project_model_table(lua: &Lua, model: &Table) -> mlua::Result<Option<Table>> {
    if model.metatable().is_some() {
        return Ok(None);
    }
    let Value::Table(params) = model.raw_get::<Value>("params")? else {
        return Ok(None);
    };
    if params.metatable().is_some() {
        return Ok(None);
    }
    let mut updated_params: Vec<(String, Table)> = Vec::new();
    for pair in params.pairs::<Value, Value>() {
        let (k, v) = pair?;
        let (Value::String(s), Value::Table(def)) = (k, v) else {
            continue;
        };
        let Ok(name) = s.to_str() else {
            continue;
        };
        if let Some(projected_def) = project_param_def_table(lua, &def)? {
            updated_params.push((name.to_string(), projected_def));
        }
    }
    if updated_params.is_empty() {
        return Ok(None);
    }
    let projected_params = copy_table_with_overrides(lua, &params, None, |k, v| {
        if let Value::String(s) = k
            && let Ok(name) = s.to_str()
            && let Some((_, updated)) = updated_params.iter().find(|(n, _)| n == &*name)
        {
            return Ok(Some(Value::Table(updated.clone())));
        }
        Ok(Some(v))
    })?;
    copy_table_with_overrides(lua, model, None, |k, v| {
        if let Value::String(s) = k
            && s.to_str().is_ok_and(|name| name == "params")
        {
            return Ok(Some(Value::Table(projected_params.clone())));
        }
        Ok(Some(v))
    })
    .map(Some)
}

/// Project runtime-only fields (any top-level `Token` userdata identified by type, plus
/// `_rebuild_tool_declarations` / `on_attempt` functions and supported functional parameter rules
/// in `model.params`) out of a Lua value before strict bounded JSON conversion. Any other
/// function, nested `Token`, non-Token userdata, or unsupported metatable is rejected by
/// `strict_lua_to_json_bounded`.
pub(crate) fn project_value_for_strict_json(
    lua: &Lua,
    value: Value,
    max_nodes: usize,
) -> mlua::Result<(serde_json::Value, Option<ProjectedTable>)> {
    match value {
        Value::Nil => Ok((serde_json::Value::Null, None)),
        Value::Table(table) if table.metatable().is_none() => {
            let fields = collect_runtime_fields(&table)?;
            let orig_model = table.raw_get::<Value>("model")?;
            let orig_tools = table.raw_get::<Value>(mare_shared::TOOLS_KEY)?;
            let (projected_operation, operation_fields) = match table.raw_get::<Value>("operation")? {
                Value::Table(op_table) if op_table.metatable().is_none() => {
                    let op_fields = collect_runtime_fields(&op_table)?;
                    if op_fields.is_empty() {
                        (None, Vec::new())
                    } else {
                        let proj_op = copy_table_with_overrides(lua, &op_table, None, |k, v| {
                            if let Value::String(s) = k
                                && let Ok(name) = s.to_str()
                                && op_fields.iter().any(|(field, _)| *field == *name)
                            {
                                return Ok(None);
                            }
                            Ok(Some(v))
                        })?;
                        (Some(proj_op), op_fields)
                    }
                }
                _ => (None, Vec::new()),
            };
            let projected_model = match &orig_model {
                Value::Table(model_table) => project_model_table(lua, model_table)?,
                _ => None,
            };
            let projected_self_model = if matches!(orig_model, Value::Nil)
                && matches!(table.raw_get::<Value>("id")?, Value::String(_))
                && matches!(table.raw_get::<Value>("provider")?, Value::String(_))
            {
                project_model_table(lua, &table)?
            } else {
                None
            };
            let model_lossy = match (&projected_model, &orig_model) {
                (Some(_), Value::Table(m)) => param_rules_hold_functions(m)?,
                _ => false,
            };
            let mut whole = None;
            let json = if let Some(self_model) = projected_self_model {
                let json = strict_lua_to_json_bounded(lua, Value::Table(self_model), max_nodes)?;
                if param_rules_hold_functions(&table)? {
                    whole = Some((Value::Table(table.clone()), json.clone()));
                }
                json
            } else if fields.is_empty() && projected_model.is_none() && projected_operation.is_none() {
                strict_lua_to_json_bounded(lua, Value::Table(table), max_nodes)?
            } else {
                let copy = lua.create_table()?;
                for pair in table.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    if let Value::String(ref s) = k
                        && let Ok(key) = s.to_str()
                    {
                        if fields.iter().any(|(name, _)| name == &*key) {
                            continue;
                        }
                        if key == "model"
                            && let Some(ref proj_model) = projected_model
                        {
                            copy.raw_set(k, proj_model.clone())?;
                            continue;
                        }
                        if key == "operation"
                            && let Some(ref proj_op) = projected_operation
                        {
                            copy.raw_set(k, proj_op.clone())?;
                            continue;
                        }
                    }
                    copy.raw_set(k, v)?;
                }
                strict_lua_to_json_bounded(lua, Value::Table(copy), max_nodes)?
            };
            let model = match (orig_model, json.get("model")) {
                (orig @ Value::Table(_), Some(model_json)) => Some((orig, model_json.clone())),
                _ => None,
            };
            let mut tools = Vec::new();
            if let (Value::Table(tools_table), Some(serde_json::Value::Array(tools_json))) =
                (orig_tools, json.get(mare_shared::TOOLS_KEY))
            {
                for (idx, tool_json) in tools_json.iter().enumerate() {
                    if let Ok(orig_tool @ Value::Table(_)) = tools_table.raw_get::<Value>(idx + 1) {
                        tools.push((orig_tool, tool_json.clone()));
                    }
                }
            }
            let projected = if fields.is_empty()
                && operation_fields.is_empty()
                && model.is_none()
                && tools.is_empty()
                && whole.is_none()
            {
                None
            } else {
                Some(ProjectedTable { fields, operation_fields, model, model_lossy, whole, tools })
            };
            Ok((json, projected))
        }
        other => Ok((strict_lua_to_json_bounded(lua, other, max_nodes)?, None)),
    }
}

/// Project the first argument of a tapped event to strict bounded JSON using the unified
/// runtime-only field rules and full history node budget.
pub(crate) fn project_tap_arg(lua: &Lua, args: &MultiValue) -> mlua::Result<serde_json::Value> {
    match args.iter().next() {
        None | Some(Value::Nil) => Ok(serde_json::Value::Null),
        Some(value) => project_value_for_strict_json(lua, value.clone(), HISTORY_LOSSLESS_MAX_NODES).map(|(j, _)| j),
    }
}

/// Project event arguments to strict bounded JSON for RPC hooks and subagent bridges.
/// Handles `tool/approval`'s optional 4th cancellation token argument as well as runtime-only
/// fields on event tables.
pub fn project_event_args(lua: &Lua, event: &str, args: MultiValue) -> mlua::Result<ProjectedEventArgs> {
    let mut values: Vec<Value> = args.into_iter().collect();
    let mut cancel_token = None;
    if event == "tool/approval" {
        if values.len() > 4 {
            return Err(mlua::Error::runtime("tool/approval accepts at most 4 arguments"));
        }
        if values.len() == 4 {
            let cancel = values.pop().unwrap();
            match cancel {
                Value::Nil => {}
                Value::UserData(ud) => match ud.borrow::<mare_shared::lua::Token>() {
                    Ok(token) => cancel_token = Some(token.0.clone()),
                    Err(_) => return Err(mlua::Error::runtime("tool/approval 4th argument must be nil or task token")),
                },
                _ => return Err(mlua::Error::runtime("tool/approval 4th argument must be nil or task token")),
            }
        }
    }
    let mut json_args = Vec::with_capacity(values.len());
    let mut first_table = None;
    for (idx, value) in values.into_iter().enumerate() {
        let (json, projected) = project_value_for_strict_json(lua, value, HISTORY_LOSSLESS_MAX_NODES)?;
        if idx == 0 {
            first_table = projected;
        }
        json_args.push(json);
    }
    Ok(ProjectedEventArgs { json_args, cancel_token, first_table })
}

/// A hook's answer that is not a replacement of its argument (a policy vote, a bail result):
/// converted on its own, never measured against the projected argument.
pub fn answer_to_lua(lua: &Lua, returned: &serde_json::Value) -> mlua::Result<Value> {
    if returned.is_null() {
        return Ok(Value::Nil);
    }
    json_to_lua_lossless_bounded(lua, returned, HISTORY_LOSSLESS_MAX_NODES)
}

/// Restore a hook's JSON return value back to Lua using lossless bounded conversion, reattaching
/// projected runtime-only fields and preserving the original `model` table identity when the
/// returned `model` JSON is unchanged.
pub fn restore_event_result(
    lua: &Lua,
    projected: &ProjectedEventArgs,
    returned: &serde_json::Value,
) -> mlua::Result<Value> {
    if returned.is_null() {
        return Ok(Value::Nil);
    }
    // Every projected original is restorable or the replacement is refused: never a lossy merge.
    if let Some(pt) = &projected.first_table {
        if let Some((original, sent)) = &pt.whole {
            return if json_equal(returned, sent) {
                Ok(original.clone())
            } else {
                Err(mlua::Error::runtime(LOSSY_MODEL))
            };
        }
        if pt.model_lossy && !returned.is_object() {
            return Err(mlua::Error::runtime(LOSSY_MODEL));
        }
    }
    let value = json_to_lua_lossless_bounded(lua, returned, HISTORY_LOSSLESS_MAX_NODES)?;
    if let Some(pt) = &projected.first_table
        && let Value::Table(ref ret_table) = value
    {
        if !pt.fields.is_empty() && ret_table.metatable().is_some() {
            return Err(mlua::Error::runtime("hook return value cannot replace runtime-field object with an array"));
        }
        for (key, orig_val) in &pt.fields {
            if !matches!(ret_table.raw_get::<Value>(key.as_str())?, Value::Nil) {
                return Err(mlua::Error::runtime(format!(
                    "hook return value cannot overwrite runtime-only field `{key}`"
                )));
            }
            ret_table.raw_set(key.as_str(), orig_val.clone())?;
        }
        if !pt.operation_fields.is_empty()
            && let Value::Table(ret_op) = ret_table.raw_get::<Value>("operation")?
        {
            for (key, orig_val) in &pt.operation_fields {
                if !matches!(ret_op.raw_get::<Value>(key.as_str())?, Value::Nil) {
                    return Err(mlua::Error::runtime(format!(
                        "hook return value cannot overwrite runtime-only field `operation.{key}`"
                    )));
                }
                ret_op.raw_set(key.as_str(), orig_val.clone())?;
            }
        }
        if let Some((orig_model_val, orig_model_json)) = &pt.model {
            if returned.get("model").is_some_and(|model| json_equal(model, orig_model_json)) {
                ret_table.raw_set("model", orig_model_val.clone())?;
            } else if pt.model_lossy {
                return Err(mlua::Error::runtime(LOSSY_MODEL));
            }
        }
        if !pt.tools.is_empty()
            && let Some(serde_json::Value::Array(ret_tools_json)) = returned.get(mare_shared::TOOLS_KEY)
            && let Value::Table(ret_tools_table) = ret_table.raw_get::<Value>(mare_shared::TOOLS_KEY)?
        {
            for (idx, ret_tool_json) in ret_tools_json.iter().enumerate() {
                if let Some((orig_tool_val, _)) =
                    pt.tools.iter().find(|(_, orig_tool_json)| json_equal(orig_tool_json, ret_tool_json))
                {
                    ret_tools_table.raw_set(idx + 1, orig_tool_val.clone())?;
                }
            }
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_supported_model_param_functions_and_restores_identity() {
        let lua = Lua::new();
        let req: Table = lua
            .load(
                r#"
                local arr_mt = ...
                local model = {
                    id = "m",
                    provider = "p",
                    ref = "p/m",
                    cost = { input = 1.0 },
                    extra_array = setmetatable({}, arr_mt),
                    params = {
                        custom = {
                            default = true,
                            wire = {
                                responses = function(r) r.test = true end,
                                chat = { ["reasoning.effort"] = "$value" },
                            },
                        },
                        mode = {
                            type = "enum",
                            default = "fast",
                            options = setmetatable({
                                { value = "fast", wire = { responses = function(r) r.fast = true end } },
                                "deep",
                            }, arr_mt),
                        },
                        flag = {
                            type = "bool",
                            default = true,
                            wire = {
                                responses = {
                                    [true] = function(r) r.flag = true end,
                                    [false] = function(r) r.flag = false end,
                                },
                            },
                        },
                        toggle = {
                            type = "bool",
                            default = false,
                            wire = {
                                chat = { [true] = { ["a.b"] = true }, [false] = false, ["true"] = "shadowed" },
                            },
                        },
                    },
                }
                return {
                    model = model,
                    system = "sys",
                    _rebuild_tool_declarations = function() end,
                }
                "#,
            )
            .call(lua.array_metatable())
            .unwrap();

        let orig_model_ptr = req.get::<Table>("model").unwrap().to_pointer();
        let projected =
            project_event_args(&lua, "request/build", MultiValue::from_vec(vec![Value::Table(req.clone())])).unwrap();

        let mut returned = projected.json_args[0].clone();
        assert_eq!(returned["model"]["params"]["custom"]["wire"]["chat"]["reasoning.effort"], "$value");
        assert!(returned["model"]["params"]["custom"]["wire"].get("responses").is_none());
        assert!(returned["model"]["params"]["mode"]["options"][0]["wire"].get("responses").is_none());
        assert_eq!(returned["model"]["params"]["flag"]["wire"]["responses"], serde_json::json!({}));
        assert_eq!(
            returned["model"]["params"]["toggle"]["wire"]["chat"],
            serde_json::json!({"true": {"a.b": true}, "false": false})
        );

        returned["system"] = serde_json::json!("edited");
        let restored = restore_event_result(&lua, &projected, &returned).unwrap();
        let Value::Table(restored_req) = restored else {
            panic!("expected table");
        };
        assert_eq!(restored_req.get::<String>("system").unwrap(), "edited");
        let restored_model: Table = restored_req.get("model").unwrap();
        assert_eq!(restored_model.to_pointer(), orig_model_ptr);
        assert!(
            lua.load("local m = ...; return type(m.params.custom.wire.responses) == 'function' and type(m.params.mode.options[1].wire.responses) == 'function' and type(m.params.flag.wire.responses[true]) == 'function' and m.params.toggle.wire.chat[true]['a.b'] == true")
                .call::<bool>(restored_model)
                .unwrap()
        );

        // A JavaScript hook re-encodes 1.0 as 1: numerically equal, so identity is restored.
        let mut reencoded = projected.json_args[0].clone();
        assert_eq!(reencoded["model"]["cost"]["input"].to_string(), "1.0");
        reencoded["model"]["cost"]["input"] = serde_json::json!(1);
        let Value::Table(reencoded_req) = restore_event_result(&lua, &projected, &reencoded).unwrap() else {
            panic!("expected table");
        };
        assert_eq!(reencoded_req.get::<Table>("model").unwrap().to_pointer(), orig_model_ptr);

        // A model whose function-valued rules were left out cannot take remote changes: merging
        // them would drop the functions, so the hook's answer is refused, never applied lossily.
        let mut mutated = projected.json_args[0].clone();
        mutated["model"]["id"] = serde_json::json!("tampered");
        let err = restore_event_result(&lua, &projected, &mutated).unwrap_err().to_string();
        assert!(err.contains("model") && err.contains("function"), "{err}");
    }

    /// The first argument may itself be a model (a plugin's own waterfall over a model table):
    /// returned as is, the original table (functions included) comes back; changed, the answer is
    /// refused instead of losing the functions. A model with no functions takes changes.
    #[test]
    fn a_model_as_the_whole_argument_round_trips_or_fails() {
        let lua = Lua::new();
        let model: Table = lua
            .load(
                r#"return { id = "m", provider = "p", name = "M",
                    params = { custom = { default = true, wire = { responses = function(r) r.x = true end } } } }"#,
            )
            .eval()
            .unwrap();
        let ptr = model.to_pointer();
        let projected =
            project_event_args(&lua, "probe/model", MultiValue::from_vec(vec![Value::Table(model)])).unwrap();
        let sent = projected.json_args[0].clone();
        assert!(sent["params"]["custom"]["wire"].get("responses").is_none(), "{sent}");

        let Value::Table(back) = restore_event_result(&lua, &projected, &sent).unwrap() else { panic!("table") };
        assert_eq!(back.to_pointer(), ptr, "returned unchanged: the original model");
        assert!(
            lua.load("local m = ... return type(m.params.custom.wire.responses) == 'function'")
                .call::<bool>(back)
                .unwrap()
        );

        let mut renamed = sent.clone();
        renamed["name"] = serde_json::json!("other");
        let err = restore_event_result(&lua, &projected, &renamed).unwrap_err().to_string();
        assert!(err.contains("model") && err.contains("function"), "{err}");

        // nothing was left out: a change is applied as an ordinary lossless conversion
        let plain: Table = lua.load(r#"return { id = "m", provider = "p", name = "M" }"#).eval().unwrap();
        let projected =
            project_event_args(&lua, "probe/model", MultiValue::from_vec(vec![Value::Table(plain)])).unwrap();
        let mut renamed = projected.json_args[0].clone();
        renamed["name"] = serde_json::json!("other");
        let Value::Table(back) = restore_event_result(&lua, &projected, &renamed).unwrap() else { panic!("table") };
        assert_eq!(back.get::<String>("name").unwrap(), "other");
    }

    #[test]
    fn rejects_unsupported_functions_inside_model() {
        let lua = Lua::new();
        for script in [
            "return { model = { id = 'm', provider = 'p', bad_fn = function() end } }",
            "return { model = { id = 'm', provider = 'p', params = { custom = { bad_fn = function() end } } } }",
            "return { model = { id = 'm', provider = 'p', params = { custom = { wire = { responses = { nested = function() end } } } } } }",
        ] {
            let req: Table = lua.load(script).eval().unwrap();
            let err = project_event_args(&lua, "request/build", MultiValue::from_vec(vec![Value::Table(req)]))
                .err()
                .expect("unsupported function on model must still be rejected");
            assert!(err.to_string().contains("unsupported JSON value"), "unexpected error: {err}");
        }
    }

    /// A bool parameter's map with no function (`[true] = { flag = true }`) only has its keys
    /// renamed for JSON: nothing is lost, so a change to that model is an ordinary lossless
    /// conversion — as the whole argument and as a request's `model` field.
    #[test]
    fn a_model_whose_bool_map_holds_no_function_takes_changes() {
        let lua = Lua::new();
        let src = r#"return { id = "m", provider = "p", name = "M",
            params = { flag = { type = "bool", default = true, wire = { chat = { [true] = { flag = true } } } } } }"#;
        let model: Table = lua.load(src).eval().unwrap();
        let projected =
            project_event_args(&lua, "probe/model", MultiValue::from_vec(vec![Value::Table(model)])).unwrap();
        let mut renamed = projected.json_args[0].clone();
        renamed["name"] = serde_json::json!("other");
        let Value::Table(back) = restore_event_result(&lua, &projected, &renamed).unwrap() else { panic!("table") };
        assert_eq!(back.get::<String>("name").unwrap(), "other");

        let req: Table =
            lua.load(format!("return {{ system = 's', model = (function() {src} end)() }}")).eval().unwrap();
        let projected =
            project_event_args(&lua, "request/build", MultiValue::from_vec(vec![Value::Table(req)])).unwrap();
        let mut renamed = projected.json_args[0].clone();
        renamed["model"]["name"] = serde_json::json!("other");
        let Value::Table(back) = restore_event_result(&lua, &projected, &renamed).unwrap() else { panic!("table") };
        assert_eq!(back.get::<Table>("model").unwrap().get::<String>("name").unwrap(), "other");
    }

    /// A replacement that is not a table cannot hold the request's model either: refused when
    /// that model lost functions (null stays “no change”, handled by the caller).
    #[test]
    fn a_scalar_replacement_of_a_request_with_a_lossy_model_is_refused() {
        let lua = Lua::new();
        let req: Table = lua
            .load(
                r#"return { system = "s", model = { id = "m", provider = "p",
                params = { custom = { wire = { responses = function() end } } } } }"#,
            )
            .eval()
            .unwrap();
        let projected =
            project_event_args(&lua, "request/build", MultiValue::from_vec(vec![Value::Table(req)])).unwrap();
        for scalar in [serde_json::json!("x"), serde_json::json!(5), serde_json::json!(true)] {
            let err = restore_event_result(&lua, &projected, &scalar).unwrap_err().to_string();
            assert!(err.contains("functions"), "{scalar}: {err}");
        }
    }

    #[test]
    fn nested_operation_cancel_token_is_projected_and_restored() {
        let lua = Lua::new();
        let token = mare_shared::lua::Token(tokio_util::sync::CancellationToken::new());
        let run_token = mare_shared::lua::Token(tokio_util::sync::CancellationToken::new());
        let payload = lua.create_table().unwrap();
        let operation = lua.create_table().unwrap();
        operation.set("tool", "read").unwrap();
        operation.set("cancel", lua.create_userdata(token).unwrap()).unwrap();
        operation.set("run_cancel", lua.create_userdata(run_token).unwrap()).unwrap();
        payload.set("result", lua.create_table().unwrap()).unwrap();
        payload.set("operation", operation).unwrap();

        let projected =
            project_event_args(&lua, "tool/result", MultiValue::from_vec(vec![Value::Table(payload)])).unwrap();
        assert_eq!(projected.json_args[0]["operation"]["tool"], "read");
        assert!(projected.json_args[0]["operation"].get("cancel").is_none());
        assert!(projected.json_args[0]["operation"].get("run_cancel").is_none());

        let mut modified = projected.json_args[0].clone();
        modified["operation"]["extra"] = serde_json::json!(true);
        let Value::Table(restored) = restore_event_result(&lua, &projected, &modified).unwrap() else {
            panic!("table");
        };
        let restored_op: Table = restored.get("operation").unwrap();
        assert!(is_task_token(&restored_op.get::<Value>("cancel").unwrap()));
        assert!(is_task_token(&restored_op.get::<Value>("run_cancel").unwrap()));
        assert!(restored_op.get::<bool>("extra").unwrap());

        let mut forged = projected.json_args[0].clone();
        forged["operation"]["run_cancel"] = serde_json::json!("forged");
        let err = restore_event_result(&lua, &projected, &forged).unwrap_err().to_string();
        assert!(err.contains("operation.run_cancel"), "{err}");
    }

    #[test]
    fn projects_arbitrary_top_level_tokens_by_type_and_restores_them() {
        #[derive(Clone)]
        struct OpaqueUd;
        impl mlua::UserData for OpaqueUd {}

        let lua = Lua::new();
        let tok_a = mare_shared::lua::Token(CancellationToken::new());
        let tok_b = mare_shared::lua::Token(CancellationToken::new());
        lua.globals().set("tok_a", tok_a).unwrap();
        lua.globals().set("tok_b", tok_b).unwrap();
        lua.globals().set("opaque_ud", OpaqueUd).unwrap();

        let arg0: Table = lua
            .load("return { payload = 'ok', run_cancel = tok_a, on_attempt = function() return true end }")
            .eval()
            .unwrap();
        let arg2: Table =
            lua.load("return { max_bytes = 1024, run_cancel = tok_b, custom_tok = tok_a }").eval().unwrap();

        let projected = project_event_args(
            &lua,
            "tool/result",
            MultiValue::from_vec(vec![
                Value::Table(arg0),
                Value::String(lua.create_string("call").unwrap()),
                Value::Table(arg2),
            ]),
        )
        .unwrap();

        assert_eq!(projected.json_args[0], serde_json::json!({ "payload": "ok" }));
        assert_eq!(projected.json_args[2], serde_json::json!({ "max_bytes": 1024 }));

        let mut returned = projected.json_args[0].clone();
        returned["payload"] = serde_json::json!("rewritten");
        let Value::Table(restored) = restore_event_result(&lua, &projected, &returned).unwrap() else {
            panic!("expected table");
        };
        assert_eq!(restored.get::<String>("payload").unwrap(), "rewritten");
        assert!(is_task_token(&restored.get::<Value>("run_cancel").unwrap()));
        assert!(matches!(restored.get::<Value>("on_attempt").unwrap(), Value::Function(_)));

        let mut spoofed = returned.clone();
        spoofed["run_cancel"] = serde_json::json!("forged");
        let err = restore_event_result(&lua, &projected, &spoofed).unwrap_err().to_string();
        assert!(err.contains("run_cancel"), "{err}");

        // Nested Token / arbitrary Function / non-Token UserData must still be rejected.
        for bad_script in [
            "return { nested = { run_cancel = tok_a } }",
            "return { custom_cb = function() end }",
            "return { nested = { cb = function() end } }",
            "return { opaque = opaque_ud }",
        ] {
            let tbl: Table = lua.load(bad_script).eval().unwrap();
            let err = project_event_args(&lua, "tool/result", MultiValue::from_vec(vec![Value::Table(tbl)]))
                .err()
                .expect("expected non-JSON rejection");
            assert!(err.to_string().contains("unsupported JSON value"), "{bad_script}: {err}");
        }
    }

    #[test]
    fn tool_approval_fourth_arg_validation_and_answer_to_lua() {
        struct NotAToken;
        impl mlua::UserData for NotAToken {}

        let lua = Lua::new();
        assert!(answer_to_lua(&lua, &serde_json::Value::Null).unwrap().is_nil());
        let ok_val = answer_to_lua(&lua, &serde_json::json!({ "decision": "allow" })).unwrap();
        assert!(matches!(ok_val, Value::Table(_)));

        let projected_nil = project_event_args(
            &lua,
            "tool/approval",
            MultiValue::from_vec(vec![Value::Nil, Value::Nil, Value::Nil, Value::Nil]),
        )
        .unwrap();
        assert!(projected_nil.cancel_token.is_none());

        let ud = lua.create_userdata(NotAToken).unwrap();
        let err_ud = project_event_args(
            &lua,
            "tool/approval",
            MultiValue::from_vec(vec![Value::Nil, Value::Nil, Value::Nil, Value::UserData(ud)]),
        )
        .err()
        .expect("non-token userdata must fail");
        assert!(err_ud.to_string().contains("4th argument must be nil or task token"), "{err_ud}");

        let err_int = project_event_args(
            &lua,
            "tool/approval",
            MultiValue::from_vec(vec![Value::Nil, Value::Nil, Value::Nil, Value::Integer(7)]),
        )
        .err()
        .expect("integer 4th arg must fail");
        assert!(err_int.to_string().contains("4th argument must be nil or task token"), "{err_int}");
    }
}
