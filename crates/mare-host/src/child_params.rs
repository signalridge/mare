//! A sub-agent's model parameters: the parent's parameter rules travel
//! to the child VM with their Lua functions, and the selections the child starts with (the
//! agent layer) come from the parent's session and `start{params=}`.
//!
//! Rules are config, so the child holds the same ones rebuilt from the same config layers;
//! only selections cross as values.

use mare_shared::carry::Carried;
use mare_shared::lua::{json_to_lua, lua_to_json};
use mlua::{Function, Lua, Table, Value};

use crate::agent::ParentHost;

/// Replace the JSON copies of `param_rules` and `models.*.params` (functions dropped) with ones
/// whose functions are carried to the child; [`Carried::install`] rebuilds them there. A function
/// that can't be carried is named once per VM (notice, or stderr without a UI); `mare doctor`
/// lists the same through `mare.subagent.uncarried`.
pub(crate) fn carry_param_rules(
    lua: &Lua,
    parent_config: Option<Value>,
    config: &mut serde_json::Value,
) -> mlua::Result<Carried> {
    let mut carried = Carried::default();
    let Some(Value::Table(c)) = parent_config else { return Ok(carried) };
    let label = |carried: &mut Carried, from: usize, what: &str| {
        for p in &mut carried.problems[from..] {
            *p = format!("{what}: {p}");
        }
    };
    if let Value::Table(rules) = c.get::<Value>("param_rules")? {
        let mut out = vec![];
        for (i, rule) in rules.sequence_values::<Value>().enumerate() {
            let rule = rule?;
            let at = match &rule {
                Value::Table(r) => r.get::<Option<String>>("_where")?,
                _ => None,
            };
            let from = carried.problems.len();
            out.push(carried.config_to_json(lua, rule)?);
            label(&mut carried, from, &format!("mare.params ({})", at.unwrap_or_else(|| format!("#{}", i + 1))));
        }
        config["param_rules"] = serde_json::Value::Array(out);
    }
    if let Value::Table(models) = c.get::<Value>("models")? {
        for kv in models.pairs::<Value, Value>() {
            let (Value::String(name), Value::Table(m)) = kv? else { continue };
            let name = name.to_str()?.to_string();
            if let Value::Table(params) = m.get::<Value>("params")?
                && let Some(entry) = config.get_mut("models").and_then(|ms| ms.get_mut(&name))
                && entry.is_object()
            {
                let from = carried.problems.len();
                entry["params"] = carried.config_to_json(lua, Value::Table(params))?;
                label(&mut carried, from, &format!("mare.model {name}"));
            }
        }
    }
    notify_uncarried(lua, &carried.problems);
    Ok(carried)
}

/// Once per problem and VM: the sub-agent runs without that mapping, so its requests can differ
/// from this session's — never silently (a notice in the UI, stderr without one, like the
/// startup plugin warnings).
fn notify_uncarried(lua: &Lua, problems: &[String]) {
    #[derive(Default)]
    struct Shown(std::collections::HashSet<String>);
    if lua.app_data_ref::<std::cell::RefCell<Shown>>().is_none() {
        lua.set_app_data(std::cell::RefCell::new(Shown::default()));
    }
    for p in problems {
        let fresh =
            lua.app_data_ref::<std::cell::RefCell<Shown>>().map(|s| s.borrow_mut().0.insert(p.clone())).unwrap_or(true);
        if !fresh {
            continue;
        }
        let msg = format!("sub-agents run without this parameter mapping: {p}");
        tracing::warn!("{msg}");
        let ui = lua.globals().get::<Table>("mare").and_then(|m| m.get::<Table>(mare_shared::UI_NAMESPACE_KEY)).ok();
        let connected = ui
            .as_ref()
            .and_then(|ui| ui.get::<Function>("connected").ok())
            .and_then(|f| f.call::<bool>(()).ok())
            .unwrap_or(false);
        match ui.and_then(|ui| ui.get::<Function>("notify").ok()) {
            Some(notify) if connected => {
                let _ = notify.call::<()>((msg, "warning"));
            }
            // a sub-agent's own stderr is its parent's terminal: the log keeps it
            _ if mare_mode(lua).as_deref() == Some("child") => {}
            _ => eprintln!("mare: warning: {msg}"),
        }
    }
}

fn mare_mode(lua: &Lua) -> Option<String> {
    lua.globals().get::<Table>("mare").and_then(|m| m.get::<String>("mode")).ok()
}

/// The child's agent-layer selections (`spec.params`, read by the child's llm as
/// `mare.args.params`): what the parent's `llm:subagent_params(model)` hands down (its session's
/// values when the child runs on the parent's current model), then `start{effort=}`, then
/// `start{params=}`. Without an `llm` service offering `subagent_params` only the explicit ones.
pub(crate) fn inherit_params(lua: &Lua, spec: &mut serde_json::Value, config: &serde_json::Value) {
    let mut params = parent_selection(lua, config.get("model")).unwrap_or_else(|| serde_json::json!({}));
    if let Some(e) = spec.get("effort").filter(|e| e.is_string()) {
        params["effort"] = e.clone();
    }
    if let Some(serde_json::Value::Object(own)) = spec.get("params") {
        for (k, v) in own {
            params[k] = v.clone();
        }
    }
    if params.as_object().is_some_and(|o| !o.is_empty()) {
        spec["params"] = params;
    }
}

fn parent_selection(lua: &Lua, model: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let host = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade())?;
    let svc: Table = host.service_table(mare_shared::LLM_SERVICE_NAME)?;
    let f: Function = svc.get("subagent_params").ok()?;
    let model = model.map(|m| json_to_lua(lua, m)).transpose().ok()?.unwrap_or(Value::Nil);
    let r: Value = f.call((svc, model)).ok()?;
    lua_to_json(lua, r).ok().filter(|v| v.is_object())
}
