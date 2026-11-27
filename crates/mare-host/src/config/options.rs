//! Host-provided `options` service (`ctx.options:get`, `ctx.options:set`) and shared
//! option-reading / transactional-writing coordinator used by both Lua plugins and RPC
//! (`options.get`, `options.set`, `config.describe`).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use anyhow::{Context, Result};
use mlua::{Function, MultiValue, Table, Value};
use serde_json::{Map, Value as J, json};

use mare_kernel::Kernel;
use mare_shared::lua::{json_to_lua, json_to_lua_with_null, lua_to_json};
use mare_shared::{
    APPROVALS_SERVICE_NAME, LLM_SERVICE_NAME, LLM_SET_EFFORT_METHOD, LLM_SET_MODEL_METHOD, OPTIONS_SERVICE_NAME,
};

/// Options backed by a service contract operation: `(option_key, service_name, method_name)`.
pub fn service_backed_options() -> &'static [(&'static str, &'static str, &'static str)] {
    &[("model", LLM_SERVICE_NAME, LLM_SET_MODEL_METHOD), ("effort", LLM_SERVICE_NAME, LLM_SET_EFFORT_METHOD)]
}

/// Whether `key` is backed by a service operation rather than a plain config option.
pub fn is_service_backed_option(key: &str) -> bool {
    service_backed_options().iter().any(|(k, _, _)| *k == key)
}

/// Apply runtime default overrides (service-backed `model`/`effort` and plain options) to a newly
/// created session runtime without persisting them to disk.
pub async fn apply_runtime_defaults(
    host: &crate::Host,
    session_id: &str,
    model: Option<J>,
    effort: Option<J>,
    options: Map<String, J>,
) {
    let call_host_service = |service: &'static str, method: &'static str, args: Vec<J>| async move {
        let mut mv = MultiValue::new();
        for arg in args {
            mv.push_back(json_to_lua(host.lua(), &arg)?);
        }
        host.call_service(service, method, mv).await.map(|_| ())
    };
    if let Some(model) = model
        && let Err(e) =
            call_host_service(LLM_SERVICE_NAME, LLM_SET_MODEL_METHOD, vec![model, json!({ "save": false })]).await
    {
        tracing::warn!("applying runtime default model to new session `{session_id}` failed: {e}");
    }
    if let Some(effort) = effort
        && let Err(e) =
            call_host_service(LLM_SERVICE_NAME, LLM_SET_EFFORT_METHOD, vec![effort, json!({ "save": false })]).await
    {
        tracing::warn!("applying runtime default effort to new session `{session_id}` failed: {e}");
    }
    for (k, v) in options {
        let mut single = Map::new();
        single.insert(k.clone(), v);
        if let Err(err) = set_with_service_caller(
            &host.kernel,
            &single,
            false,
            |service, method, value| call_host_service(service, method, vec![value]),
            |e: &anyhow::Error| e.to_string(),
        )
        .await
        {
            let msg = match err {
                SetError::InvalidArguments(m) => m,
                SetError::Internal(e) | SetError::Service(e) => e.to_string(),
            };
            tracing::warn!("applying runtime default option `{k}` to new session `{session_id}` failed: {msg}");
        }
    }
}

/// Errors produced by [`set_with_service_caller`].
pub enum SetError<E> {
    /// Validation or owner refusal (already redacted for safe external reporting).
    InvalidArguments(String),
    /// Unexpected internal error.
    Internal(anyhow::Error),
    /// Error returned by the backing service operation (e.g. `llm.set_model` / `llm.set_effort`).
    Service(E),
}

/// Partitioned writes committed by [`set_with_service_caller`], returned so callers such as RPC
/// can sync session-spawn base config without duplicating option parsing.
pub struct SetOutcome {
    pub generic: Map<String, J>,
    pub plugin_writes: BTreeMap<String, Map<String, J>>,
}

/// Read effective options (or option descriptors when `describe` is true) with live service
/// overrides (`llm` model/effort, `approvals` policy/constraints) and shared sensitive redaction.
pub async fn get(kernel: &Kernel, keys: Option<Vec<String>>, describe: bool) -> Result<J> {
    let lua = kernel.lua();
    let resolve_key = mare_shared::lua::core::config::resolve_config_key_fn(lua)?;
    let f: Function = lua
        .load(
            r#"
            local keys, describe, llm, approvals, resolve_key = ...
            local overrides = {}
            if approvals then
                overrides["plugins.approvals.policy"] = approvals:policy()
                overrides.approval_constraints = {
                    policies = approvals:inheritance(),
                    review_all = approvals:review_required(),
                }
            end
            if llm then
                local m = llm:current()
                if m and m.ref ~= nil then overrides.model = m.ref end
                local eff = llm:raw_effort()
                if eff ~= nil then overrides.effort = eff end
            end
            if describe then
                return mare.config_describe(keys, { overrides = overrides })
            end
            local out = {}
            local function put(k, v)
                local safe = mare.config_redact(k, v)
                if safe ~= nil then out[k] = safe end
            end
            if type(keys) == "table" and #keys > 0 then
                for _, k in ipairs(keys) do
                    local v = resolve_key(k, overrides)
                    put(k, v)
                end
            else
                for k, v in pairs(mare.config or {}) do
                    put(k, v)
                end
                for k, v in pairs(overrides) do
                    put(k, v)
                end
            end
            return out
        "#,
        )
        .set_name("@core/options_get")
        .into_function()?;
    let keys_lua = match keys {
        Some(k) => json_to_lua(lua, &json!(k))?,
        None => Value::Nil,
    };
    let llm_val =
        crate::ui_wait::service_table_for_kernel(kernel, LLM_SERVICE_NAME).map(Value::Table).unwrap_or(Value::Nil);
    let approvals_val = crate::ui_wait::service_table_for_kernel(kernel, APPROVALS_SERVICE_NAME)
        .map(Value::Table)
        .unwrap_or(Value::Nil);
    let v: Value = f.call_async((keys_lua, describe, llm_val, approvals_val, resolve_key)).await?;
    Ok(lua_to_json(lua, v)?)
}

/// Read config warnings with shared secret redaction.
pub async fn redacted_warnings(kernel: &Kernel) -> Result<Vec<String>> {
    let lua = kernel.lua();
    let warnings_fn: Function = lua
        .load("return (mare.config_warnings and mare.config_warnings()) or {}")
        .set_name("@core/options_warnings")
        .into_function()?;
    let warnings_val: Value = warnings_fn.call_async(()).await?;
    let warnings = match lua_to_json(lua, warnings_val)? {
        J::Array(items) => items.into_iter().filter_map(|v| v.as_str().map(mare_shared::redact::redact_str)).collect(),
        _ => vec![],
    };
    Ok(warnings)
}

/// Dry-run validation for `model` / `effort` when `validate_only = true`, matching `llm.set_model`
/// and `llm.select_effort` admission rules without mutating state.
async fn precheck_llm_options(
    kernel: &Kernel,
    model_ref: Option<&str>,
    effort: Option<&str>,
) -> std::result::Result<(), String> {
    let lua = kernel.lua();
    let Some(llm_tbl) = crate::ui_wait::service_table_for_kernel(kernel, LLM_SERVICE_NAME) else {
        return Err("service `llm` is not available".into());
    };
    let check_fn: Function = lua
        .load(
            r#"
            local llm, model_ref, effort = ...
            local target
            if model_ref ~= nil then
                local m = llm:resolve(model_ref)
                if not m then
                    return "unknown model: " .. tostring(model_ref)
                end
                local desc, err = llm:capabilities(m)
                if not desc then
                    return tostring(err or ("model unavailable: " .. tostring(model_ref)))
                end
                target = m
            end
            if effort ~= nil then
                local m = target or llm:current()
                if not m then
                    return "no model selected"
                end
                local opts = llm:effort_options(m)
                if not opts then
                    return tostring(m.ref) .. " has no effort parameter"
                end
                local known = {
                    none = true, minimal = true, low = true, medium = true, high = true, xhigh = true, max = true,
                }
                local ok = known[effort] == true
                if not ok then
                    for _, item in ipairs(opts) do
                        if item == effort then
                            ok = true
                            break
                        end
                    end
                end
                if not ok then
                    return "unknown effort: " .. tostring(effort)
                end
            end
            return nil
            "#,
        )
        .set_name("@core/options_precheck_llm")
        .into_function()
        .map_err(|e| e.to_string())?;
    let err: Option<String> = check_fn
        .call_async((Value::Table(llm_tbl), model_ref.map(|s| s.to_owned()), effort.map(|s| s.to_owned())))
        .await
        .map_err(|e| e.to_string())?;
    match err {
        Some(msg) => Err(msg),
        None => Ok(()),
    }
}

/// Reset transient and remembered effort back to unset on rollback when `effort` had no prior value.
async fn reset_unset_effort(kernel: &Kernel, model_ref: Option<&str>) -> Result<()> {
    let lua = kernel.lua();
    let Some(llm_tbl) = crate::ui_wait::service_table_for_kernel(kernel, LLM_SERVICE_NAME) else {
        return Ok(());
    };
    let reset_fn: Function = lua
        .load(
            r#"
            local llm, model_ref = ...
            if type(llm.reset_session_effort) == "function" then
                llm:reset_session_effort()
            end
            if model_ref ~= nil and type(llm.remember_param) == "function" then
                llm:remember_param(model_ref, "effort", nil)
            end
            "#,
        )
        .set_name("@core/options_reset_effort")
        .into_function()?;
    reset_fn.call_async::<()>((Value::Table(llm_tbl), model_ref.map(|s| s.to_owned()))).await?;
    Ok(())
}

/// Validate (and, unless `validate_only`, transactionally commit with full rollback) a batch of
/// option updates. `service_set` executes service-backed options (`model`, `effort`).
pub async fn set_with_service_caller<E, F, Fut>(
    kernel: &Kernel,
    values: &Map<String, J>,
    validate_only: bool,
    mut service_set: F,
    format_service_error: fn(&E) -> String,
) -> std::result::Result<SetOutcome, SetError<E>>
where
    F: FnMut(&'static str, &'static str, J) -> Fut,
    Fut: Future<Output = std::result::Result<(), E>>,
{
    let invalid = |m: String| SetError::InvalidArguments(mare_shared::redact::redact_str(&m));
    let internal = |e: anyhow::Error| SetError::Internal(e);

    if values.contains_key("approval_constraints") {
        return Err(invalid("approval_constraints is read-only".into()));
    }

    let mut plugin_writes: BTreeMap<String, Map<String, J>> = BTreeMap::new();
    let mut manager_writes: BTreeMap<String, J> = BTreeMap::new();
    let mut dotted: BTreeSet<String> = BTreeSet::new();
    let mut generic = Map::new();

    let lua = kernel.lua();
    let is_manager_option: Function = lua
        .load(
            r#"local key = ...
            local def = mare.options and mare.options.plugins
            return type(def) == "table" and type(def.default) == "table" and def.default[key] ~= nil"#,
        )
        .set_name("@core/options_is_manager")
        .into_function()
        .map_err(|e| internal(e.into()))?;

    for (k, v) in values {
        if matches!(k.as_str(), "model" | "effort") {
            continue;
        }
        // the root `plugins` table would replace every plugin's committed options and skip their
        // owners' apply: a plugin option is written as `plugins.<plugin>.<option>`
        if k == mare_shared::PLUGINS_KEY {
            return Err(invalid(
                "set a plugin option as plugins.<plugin>.<option>, not the whole plugins table".into(),
            ));
        }
        let Some(rest) = k.strip_prefix("plugins.") else {
            generic.insert(k.clone(), v.clone());
            continue;
        };
        if !rest.contains('.') && is_manager_option.call::<bool>(rest).map_err(|e| internal(e.into()))? {
            manager_writes.insert(rest.to_string(), v.clone());
            continue;
        }
        let mut parts = rest.split('.');
        let (Some(plugin), Some(first)) = (parts.next(), parts.next()) else {
            return Err(invalid(format!("option `{k}`: name a plugin option as plugins.<plugin>.<option>")));
        };
        let deeper: Vec<&str> = rest.split('.').skip(2).collect();
        if !deeper.is_empty() {
            dotted.insert(format!("plugins.{plugin}.{first}"));
        }
        let mut value = v.clone();
        for part in deeper.into_iter().rev() {
            value = json!({ part: value });
        }
        let own = plugin_writes.entry(plugin.to_string()).or_default();
        merge_leaves(own, first, value);
    }

    if let Some(name) = manager_writes.keys().find(|name| plugin_writes.contains_key(*name)) {
        return Err(invalid(format!("plugin manager option `{name}` cannot also be a plugin name in one batch")));
    }

    if let Some(v) = values.get("model")
        && !v.is_string()
    {
        return Err(invalid("option `model` expects a string".into()));
    }
    if let Some(v) = values.get("effort")
        && !v.is_string()
    {
        return Err(invalid("option `effort` expects a string".into()));
    }

    let problems: Vec<String> = {
        let f = mare_shared::lua::core::config::validate_options_fn(lua).map_err(|e| internal(e.into()))?;
        let mut all = generic.clone();
        if !plugin_writes.is_empty() || !manager_writes.is_empty() {
            let mut plugins: Map<String, J> =
                plugin_writes.iter().map(|(key, value)| (key.clone(), J::Object(value.clone()))).collect();
            plugins.extend(manager_writes.iter().map(|(key, value)| (key.clone(), value.clone())));
            all.insert(mare_shared::PLUGINS_KEY.into(), J::Object(plugins));
        }
        let t = json_to_lua_with_null(lua, &J::Object(all)).map_err(|e| internal(e.into()))?;
        f.call_async(t).await.map_err(|e| internal(e.into()))?
    };
    if !problems.is_empty() {
        return Err(invalid(problems.join("; ")));
    }

    if validate_only {
        if values.contains_key("model") || values.contains_key("effort") {
            precheck_llm_options(
                kernel,
                values.get("model").and_then(J::as_str),
                values.get("effort").and_then(J::as_str),
            )
            .await
            .map_err(invalid)?;
        }
        return Ok(SetOutcome { generic, plugin_writes });
    }

    let mutates_config = !generic.is_empty() || !manager_writes.is_empty() || !plugin_writes.is_empty();
    if mutates_config && mare_shared::lua::core::config::is_staging(lua).map_err(|e| internal(e.into()))? {
        return Err(invalid("option update unavailable during config reload; retry afterwards".into()));
    }

    let config_write_fn = if plugin_writes.is_empty() {
        None
    } else {
        match mare_shared::lua::core::config::opt_config_write_fn(lua).map_err(|e| internal(e.into()))? {
            Some(f) => Some(f),
            None => return Err(invalid("option update unavailable during config reload; retry afterwards".into())),
        }
    };

    let previous = get(kernel, Some(vec!["model".into(), "effort".into()]), false).await.map_err(internal)?;
    let touches_llm = values.contains_key("model") || values.contains_key("effort");
    let state_path = touches_llm.then(mare_shared::paths::state_file);
    let prev_state_bytes = state_path.as_ref().and_then(|p| std::fs::read(p).ok());

    let deepcopy_fn = mare_shared::lua::core::config::deepcopy_fn(lua).map_err(|e| internal(e.into()))?;
    let option_changed_fn = mare_shared::lua::core::config::option_changed(lua)
        .map_err(|e| internal(e.into()))?
        .map(Value::Function)
        .unwrap_or(Value::Nil);
    let config_write_val = config_write_fn.map(Value::Function).unwrap_or(Value::Nil);
    let raw: Function = lua
        .load(
            r#"local deepcopy_fn, option_changed_fn, config_write_fn = ...
            return function(op, k, v)
                if op == "get" then return deepcopy_fn(mare.config[k]) end
                if op == "restore" then
                    mare.config[k] = v
                    if option_changed_fn then option_changed_fn(k, v) end
                    return
                end
                if op == "plugin_writes" then
                    local ok, err = pcall(config_write_fn, k, v)
                    if not ok then return false, tostring(err) end
                    return true
                end
                local o = rawget(mare, "o")
                if op == "manager" then o.plugins[k] = v; return end
                if o then o[k] = v else mare.config[k] = v end
            end"#,
        )
        .set_name("@core/options_set")
        .call((deepcopy_fn, option_changed_fn, config_write_val))
        .map_err(|e| internal(e.into()))?;

    enum Undo {
        Service(&'static str, &'static str, J),
        ResetEffort(Option<String>),
        Option(String, Value),
    }

    let mut done: Vec<Undo> = vec![];
    let result: std::result::Result<(), SetError<E>> = async {
        // Model admission can compact or fail: it goes first, so nothing else is applied yet.
        for (key, service, method) in
            [("model", LLM_SERVICE_NAME, LLM_SET_MODEL_METHOD), ("effort", LLM_SERVICE_NAME, LLM_SET_EFFORT_METHOD)]
        {
            if let Some(v) = values.get(key) {
                service_set(service, method, v.clone()).await.map_err(SetError::Service)?;
                if let Some(old) = previous.get(key).filter(|old| !old.is_null()) {
                    done.push(Undo::Service(service, method, old.clone()));
                } else if key == "effort" {
                    let target_model = values
                        .get("model")
                        .and_then(J::as_str)
                        .or_else(|| previous.get("model").and_then(J::as_str))
                        .map(str::to_owned);
                    done.push(Undo::ResetEffort(target_model));
                }
            }
        }
        for (k, v) in &generic {
            let old: Value = raw.call_async(("get", k.as_str())).await.map_err(|e| internal(e.into()))?;
            raw.call_async::<()>(("set", k.as_str(), json_to_lua_with_null(lua, v).map_err(|e| internal(e.into()))?))
                .await
                .map_err(|e| internal(e.into()))?;
            done.push(Undo::Option(k.clone(), old));
        }
        for (k, v) in &manager_writes {
            let old: Value = raw.call_async(("get", mare_shared::PLUGINS_KEY)).await.map_err(|e| internal(e.into()))?;
            raw.call_async::<()>((
                "manager",
                k.as_str(),
                json_to_lua_with_null(lua, v).map_err(|e| internal(e.into()))?,
            ))
            .await
            .map_err(|e| internal(e.into()))?;
            done.push(Undo::Option(mare_shared::PLUGINS_KEY.into(), old));
        }
        if !plugin_writes.is_empty() {
            let batch =
                json_to_lua_with_null(lua, &serde_json::to_value(&plugin_writes).map_err(|e| internal(e.into()))?)
                    .map_err(|e| internal(e.into()))?;
            let mut marks = Map::new();
            for path in &dotted {
                let mut parts = path.splitn(3, '.').skip(1);
                if let (Some(p), Some(k)) = (parts.next(), parts.next()) {
                    let own = marks.entry(p.to_string()).or_insert_with(|| json!({}));
                    own[k] = json!(true);
                }
            }
            let marks = json_to_lua(lua, &J::Object(marks)).map_err(|e| internal(e.into()))?;
            let (ok, err): (bool, Option<String>) =
                raw.call_async(("plugin_writes", batch, marks)).await.map_err(|e| internal(e.into()))?;
            if !ok {
                return Err(invalid(err.unwrap_or_else(|| "config write failed".into())));
            }
        }
        Ok(())
    }
    .await;

    if let Err(e) = result {
        let mut rolled_back_llm = false;
        for undo in done.into_iter().rev() {
            let restored: Result<()> = match undo {
                Undo::Service(service, method, old) => {
                    rolled_back_llm = true;
                    service_set(service, method, old)
                        .await
                        .map_err(|err| anyhow::anyhow!("{}", format_service_error(&err)))
                }
                Undo::ResetEffort(model_ref) => {
                    rolled_back_llm = true;
                    reset_unset_effort(kernel, model_ref.as_deref()).await
                }
                Undo::Option(k, old) => raw.call_async::<()>(("restore", k, old)).await.map_err(Into::into),
            };
            if let Err(why) = restored {
                tracing::warn!("options.set rollback failed: {why}");
            }
        }
        if rolled_back_llm && let Some(path) = state_path {
            let res = match prev_state_bytes {
                Some(bytes) => std::fs::write(&path, bytes),
                None => match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(err) => Err(err),
                },
            };
            if let Err(why) = res {
                tracing::warn!("options.set state.json rollback failed: {why}");
            }
        }
        return Err(e);
    }

    Ok(SetOutcome { generic, plugin_writes })
}

async fn call_host_service_set(
    kernel: &Kernel,
    caller_ctx: Option<Table>,
    service: &str,
    method: &str,
    value: J,
) -> mlua::Result<()> {
    let lua = kernel.lua();
    let svc_from_caller: Option<Table> = match caller_ctx {
        Some(ctx) => {
            if let Ok(get_fn) = ctx.get::<Function>("get") {
                match get_fn.call::<Value>((ctx, service))? {
                    Value::Table(t) => Some(t),
                    _ => None,
                }
            } else {
                None
            }
        }
        None => None,
    };
    let Some(svc) = svc_from_caller.or_else(|| match kernel.service(service) {
        Some(Value::Table(t)) => Some(t),
        _ => None,
    }) else {
        return Err(mlua::Error::runtime(format!("service `{service}` is not available")));
    };
    let func: Function = svc.get(method)?;
    let mut args = MultiValue::new();
    args.push_back(Value::Table(svc));
    args.push_back(json_to_lua(lua, &value)?);
    func.call_async::<MultiValue>(args).await?;
    Ok(())
}

/// Install the host-provided `options` service (`ctx.options:get`, `ctx.options:set`) through
/// `mare.Service` and its built-in contract (`types/mare/services/contracts/options.lua`).
pub(crate) fn install_service(kernel: &Kernel) -> Result<()> {
    let lua = kernel.lua();

    let k_get = kernel.clone();
    let get_fn = lua.create_async_function(move |lua, (keys_val, opts_val): (Value, Option<Table>)| {
        let kernel = k_get.clone();
        async move {
            let keys: Option<Vec<String>> = match keys_val {
                Value::Nil => None,
                Value::Table(tbl) => {
                    let mut out = Vec::new();
                    for item in tbl.sequence_values::<String>() {
                        out.push(item?);
                    }
                    Some(out)
                }
                _ => return Err(mlua::Error::runtime("options:get: keys must be a list of strings or nil")),
            };
            let describe = match opts_val {
                Some(opts) => opts.get::<Option<bool>>("describe")?.unwrap_or(false),
                None => false,
            };
            let json = get(&kernel, keys, describe).await.map_err(|e| mlua::Error::runtime(format!("{e:#}")))?;
            json_to_lua(&lua, &json)
        }
    })?;

    let k_set = kernel.clone();
    let set_fn = lua.create_async_function(
        move |lua, (caller_ctx, values_val, opts_val): (Option<Table>, Value, Option<Table>)| {
            let kernel = k_set.clone();
            async move {
                let Value::Table(_) = values_val else {
                    return Ok((false, Value::String(lua.create_string("options:set: values must be a table")?)));
                };
                let J::Object(values) = lua_to_json(&lua, values_val)? else {
                    return Ok((
                        false,
                        Value::String(lua.create_string("options:set: values must be a key-value table")?),
                    ));
                };
                let validate_only = match opts_val {
                    Some(opts) => opts.get::<Option<bool>>("validate_only")?.unwrap_or(false),
                    None => false,
                };
                let res = set_with_service_caller(
                    &kernel,
                    &values,
                    validate_only,
                    |service, method, value| {
                        let k = kernel.clone();
                        let caller = caller_ctx.clone();
                        async move { call_host_service_set(&k, caller, service, method, value).await }
                    },
                    |e: &mlua::Error| e.to_string(),
                )
                .await;
                match res {
                    Ok(_) => Ok((true, Value::Nil)),
                    Err(SetError::InvalidArguments(msg)) => Ok((false, Value::String(lua.create_string(msg)?))),
                    Err(SetError::Internal(err)) => {
                        let msg = mare_shared::redact::redact_str(&format!("{err:#}"));
                        Ok((false, Value::String(lua.create_string(msg)?)))
                    }
                    Err(SetError::Service(err)) => {
                        let msg = mare_shared::redact::redact_str(&err.to_string());
                        Ok((false, Value::String(lua.create_string(msg)?)))
                    }
                }
            }
        },
    )?;

    let methods: Table = lua
        .load(
            r#"local get_fn, set_fn = ...
            return {
                get = function(self, keys, opts)
                    return get_fn(keys, opts)
                end,
                set = function(self, values, opts)
                    local ok, err = set_fn(self.ctx, values, opts)
                    if not ok then
                        error({ code = "invalid_arguments", message = tostring(err) }, 0)
                    end
                end,
            }"#,
        )
        .set_name("@core/options_service_methods")
        .call((get_fn, set_fn))?;
    kernel.host_provide_service(OPTIONS_SERVICE_NAME, methods).context("installing host options service")?;
    Ok(())
}

/// Put `value` at `key` in `into`, merging objects leaf by leaf: `a.b.x` and `a.b.y` in one
/// request both land.
fn merge_leaves(into: &mut Map<String, J>, key: &str, value: J) {
    match (into.get_mut(key), value) {
        (Some(J::Object(existing)), J::Object(more)) => {
            for (k, v) in more {
                merge_leaves(existing, &k, v);
            }
        }
        (_, value) => {
            into.insert(key.to_string(), value);
        }
    }
}
