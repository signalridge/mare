use mlua::{Function, Lua, Table, Value};

use super::ParentHost;
use crate::child_params::{carry_param_rules, inherit_params};
use mare_shared::lua::{lua_to_json, rt_err};

const LEVEL_RANK: [(&str, u8); 3] = [("read-only", 1), ("workspace-write", 2), ("danger-full-access", 3)];
const PERMISSIONS_PLUGIN: &str = mare_shared::PERMISSIONS_SERVICE_NAME;
type PermissionRank = (u8, u8);

fn approval_rank(name: &str) -> Option<u8> {
    match name {
        "read-only" => Some(1),
        "auto" => Some(2),
        "full-access" | "yolo" => Some(3),
        _ => None,
    }
}

fn within(rank: PermissionRank, limit: PermissionRank) -> bool {
    rank.0 <= limit.0 && rank.1 <= limit.1
}

/// The built-in presets by the sandbox level they expand to: the ranking used when the parent has
/// no `permissions` service to describe its presets.
fn builtin_rank(name: &str) -> Option<PermissionRank> {
    let level = match name {
        "read-only" => "read-only",
        "auto" => "workspace-write",
        "full-access" | "yolo" => "danger-full-access",
        _ => return None,
    };
    let level = LEVEL_RANK.iter().find(|(l, _)| *l == level).map(|(_, r)| *r)?;
    Some((level, approval_rank(name)?))
}

struct ParentPermissions {
    preset: String,
    names: Vec<String>,
    service: Table,
    describe: Function,
}

impl ParentPermissions {
    fn rank(&self, name: &str) -> Option<PermissionRank> {
        let d: Option<Table> = self.describe.call((self.service.clone(), name)).ok()?;
        let d = d?;
        let level: String = d.get("level").ok()?;
        let level = LEVEL_RANK.iter().find(|(l, _)| *l == level).map(|(_, r)| *r)?;
        let approval = match d.get::<Option<String>>("like").ok()? {
            Some(like) => approval_rank(&like)?,
            None => level,
        };
        Some((level, approval))
    }
}

/// The parent's effective preset and its sandbox/approval ranks, including custom presets.
/// `None` without a `permissions` service (the plugin is disabled).
fn host_permissions(host: &crate::Host) -> Option<ParentPermissions> {
    let service = host.service_table(PERMISSIONS_PLUGIN)?;
    let preset = service.get::<Function>("preset").ok()?.call(service.clone()).ok()?;
    let mut names = service
        .get::<Function>("presets")
        .ok()
        .and_then(|presets| presets.call::<Vec<String>>(service.clone()).ok())
        .unwrap_or_default();
    names.sort();
    let describe = service.get("describe").ok()?;
    Some(ParentPermissions { preset, names, service, describe })
}

fn parent_permissions(lua: &Lua) -> Option<ParentPermissions> {
    let host = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade())?;
    host_permissions(&host)
}

fn preset_for_policy(policy: Option<&mare_binding::ossandbox::Policy>) -> &'static str {
    match policy.map(|p| p.level) {
        Some(mare_binding::ossandbox::Level::ReadOnly) => "read-only",
        Some(mare_binding::ossandbox::Level::WorkspaceWrite) => "auto",
        _ => "full-access",
    }
}

/// Compute the active permission preset and sandbox policy ceilings currently enforced on `host`
/// so a non-admin session load or fork cannot restore a looser preset or sandbox policy.
pub(crate) fn host_permission_ceiling(host: &crate::Host) -> (Option<String>, Option<serde_json::Value>) {
    let lua = host.lua();
    let cur_policy = mare_binding::ossandbox::enforce_ceiling(lua, mare_binding::ossandbox::current(lua));
    let inherited = preset_for_policy(cur_policy.as_ref());
    let svc = host_permissions(host);
    let rank = |name: &str| -> Option<PermissionRank> {
        match &svc {
            Some(parent) => parent.rank(if name == "yolo" { "full-access" } else { name }),
            None => builtin_rank(name),
        }
    };
    let sandbox = builtin_rank(inherited).unwrap_or((1, 1));
    let parent = svc.as_ref().and_then(|p| rank(&p.preset).map(|r| (p.preset.clone(), r)));
    let top = parent.as_ref().map_or(sandbox, |(_, r)| (sandbox.0.min(r.0), r.1));
    let preset = parent
        .as_ref()
        .filter(|(_, r)| within(*r, top))
        .map(|(n, _)| n.clone())
        .or_else(|| {
            let mut candidates = vec![inherited.to_string(), "read-only".into(), "auto".into(), "full-access".into()];
            if let Some(parent) = &svc {
                candidates.extend(parent.names.iter().cloned());
            }
            candidates.into_iter().find(|name| rank(name).is_some_and(|r| within(r, top)))
        })
        .unwrap_or_else(|| "read-only".to_owned());

    let policy_json = cur_policy.as_ref().and_then(|p| {
        let tbl = p.to_lua(lua).ok()?;
        lua_to_json(lua, Value::Table(tbl)).ok()
    });
    if preset == "full-access" && policy_json.is_none() {
        return (None, None);
    }
    (Some(preset), policy_json)
}

/// a child inherits the parent's preset as its `permission_ceiling` and may
/// only be stricter. The parent's ceiling is the stricter of its permission preset and the
/// sandbox actually in force for this call (`inherited`: the exec context / per-VM policy; a
/// "custom" preset ranks by it). A caller's own `permission_ceiling` / `permission` only narrow
/// it — never widen, never get overwritten by a looser inherited value; the child's permissions
/// plugin rejects any later switch above the ceiling.
pub(super) fn inherit_permissions(lua: &Lua, spec: &mut serde_json::Value, inherited: &str) -> mlua::Result<()> {
    let svc = parent_permissions(lua);
    let rank = |name: &str| -> Option<PermissionRank> {
        match &svc {
            Some(parent) => parent.rank(if name == "yolo" { "full-access" } else { name }),
            None => builtin_rank(name),
        }
    };
    // A narrower active sandbox limits the level, but it does not replace the parent's
    // independent approval limit. For an unrecognized/custom parent retain the previous
    // conservative builtin fallback based on the sandbox in force.
    let sandbox = builtin_rank(inherited).unwrap_or((1, 1));
    let parent = svc.as_ref().and_then(|p| rank(&p.preset).map(|r| (p.preset.clone(), r)));
    let top = parent.as_ref().map_or(sandbox, |(_, r)| (sandbox.0.min(r.0), r.1));
    let narrower = |key: &str, limit: PermissionRank| -> Option<(String, PermissionRank)> {
        let name = spec.get(key)?.as_str()?.to_string();
        let r = rank(&name)?;
        within(r, limit).then_some((name, r))
    };
    let fallback = || {
        parent.as_ref().filter(|(_, r)| within(*r, top)).cloned().or_else(|| {
            let mut candidates = vec![inherited.to_string(), "read-only".into(), "auto".into(), "full-access".into()];
            if let Some(parent) = &svc {
                candidates.extend(parent.names.iter().cloned());
            }
            candidates.into_iter().find_map(|name| rank(&name).filter(|r| within(*r, top)).map(|r| (name, r)))
        })
    };
    let explicit_ceiling = spec.get("permission_ceiling").is_some_and(|v| !v.is_null());
    let (mut ceiling, crank) = narrower("permission_ceiling", top)
        .or_else(fallback)
        .ok_or_else(|| rt_err("no permission preset satisfies the inherited ceiling"))?;
    let (permission, prank) = narrower("permission", crank).unwrap_or_else(|| (ceiling.clone(), crank));
    // An explicit read-only `permission` without a looser explicit `permission_ceiling` establishes
    // a hard read-only ceiling so the child cannot widen itself back to the parent's preset.
    if !explicit_ceiling && prank == (1, 1) {
        ceiling = permission.clone();
    }
    if !spec.is_object() {
        *spec = serde_json::json!({});
    }
    spec["permission_ceiling"] = ceiling.into();
    spec["permission"] = permission.into();
    Ok(())
}

fn set_plugin_config_leaf(config: &mut serde_json::Value, plugin: &str, key: &str, value: serde_json::Value) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let Some(root) = config.as_object_mut() else { return };
    if !root.get(mare_shared::PLUGINS_KEY).is_some_and(serde_json::Value::is_object) {
        root.insert(mare_shared::PLUGINS_KEY.into(), serde_json::json!({}));
    }
    let Some(plugins) = root.get_mut(mare_shared::PLUGINS_KEY).and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    if !plugins.get(plugin).is_some_and(serde_json::Value::is_object) {
        plugins.insert(plugin.into(), serde_json::json!({}));
    }
    if let Some(own) = plugins.get_mut(plugin).and_then(serde_json::Value::as_object_mut) {
        own.insert(key.into(), value);
    }
}

fn narrow_child_approval_policy(
    requested: Option<serde_json::Value>,
    ceilings: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let mut has_never = false;
    let mut has_untrusted = false;
    if let Some(items) = ceilings.and_then(serde_json::Value::as_array) {
        for item in items {
            if item.as_str() == Some("never") {
                has_never = true;
            } else if item.as_str() == Some("untrusted") {
                has_untrusted = true;
            }
        }
    }
    if has_never {
        return Some(serde_json::Value::String("never".into()));
    }
    if has_untrusted && requested.as_ref().and_then(serde_json::Value::as_str).is_none_or(|s| s == "ask") {
        return Some(serde_json::Value::String("untrusted".into()));
    }
    requested
}

fn string_list(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let arr = value?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(s) = item.as_str() {
            out.push(s.to_owned());
        }
    }
    Some(out)
}

/// Narrow child tool allow/deny lists against the parent's ceilings:
/// - `tools_enabled` (allowlist): a child may only narrow (intersect), never widen or clear.
/// - `tools_excluded` / `tools_disabled` (denylists): a child inherits all parent entries (union).
fn enforce_tool_config_ceilings(
    config: &mut serde_json::Value,
    parent_enabled: Option<Vec<String>>,
    parent_excluded: Vec<String>,
    parent_disabled: Vec<String>,
) {
    if let Some(parent_allow) = parent_enabled {
        let parent_set: std::collections::HashSet<&str> = parent_allow.iter().map(String::as_str).collect();
        let next = match string_list(config.get("tools_enabled")) {
            Some(child_allow) => {
                let mut seen = std::collections::HashSet::new();
                child_allow
                    .into_iter()
                    .filter(|name| parent_set.contains(name.as_str()) && seen.insert(name.clone()))
                    .collect::<Vec<_>>()
            }
            None => parent_allow,
        };
        config["tools_enabled"] = serde_json::json!(next);
    }
    for (key, parent_deny) in [("tools_excluded", parent_excluded), ("tools_disabled", parent_disabled)] {
        if parent_deny.is_empty() {
            continue;
        }
        let mut merged = string_list(config.get(key)).unwrap_or_default();
        let mut seen: std::collections::HashSet<String> = merged.iter().cloned().collect();
        for name in parent_deny {
            if seen.insert(name.clone()) {
                merged.push(name);
            }
        }
        config[key] = serde_json::json!(merged);
    }
}

fn parent_current_model(lua: &Lua) -> Option<String> {
    let host = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade())?;
    let svc = host.service_table(mare_shared::LLM_SERVICE_NAME)?;
    let current: Function = svc.get("current").ok()?;
    let model: Table = current.call(svc).ok()?;
    model.get::<String>("ref").ok().filter(|r| !r.is_empty())
}

/// Inherit the parent's config, carry param rules, overlay `spec.config`, and apply `model`/`effort`
/// plus the narrowed child permission preset and approval policy into the child's CLI config layer.
pub(super) fn merge_child_config(
    lua: &Lua,
    spec: &serde_json::Value,
) -> mlua::Result<(serde_json::Value, mare_shared::carry::Carried)> {
    let parent_config = lua.globals().get::<Table>("mare").and_then(|m| m.get::<Value>("config")).ok();
    let mut config: serde_json::Value = parent_config
        .clone()
        .and_then(|v| lua_to_json(lua, v).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    let parent_enabled = string_list(config.get("tools_enabled"));
    let parent_excluded = string_list(config.get("tools_excluded")).unwrap_or_default();
    let parent_disabled = string_list(config.get("tools_disabled")).unwrap_or_default();
    let carried = carry_param_rules(lua, parent_config, &mut config)?;
    if let Some(over) = spec.get("config").cloned().filter(|v| v.is_object()) {
        mare_config::merge_json(&mut config, over);
    }
    enforce_tool_config_ceilings(&mut config, parent_enabled, parent_excluded, parent_disabled);
    let explicit_model = spec
        .get("model")
        .and_then(serde_json::Value::as_str)
        .or_else(|| spec.get("config").and_then(|c| c.get("model")).and_then(serde_json::Value::as_str))
        .is_some();
    if !explicit_model && let Some(current_model) = parent_current_model(lua) {
        config["model"] = serde_json::Value::String(current_model);
    }
    for key in ["model", "effort"] {
        if let Some(v) = spec.get(key).and_then(|v| v.as_str()) {
            config[key] = serde_json::Value::String(v.into());
        }
    }
    if let Some(preset) = spec.get("permission").filter(|v| !v.is_null()) {
        set_plugin_config_leaf(&mut config, PERMISSIONS_PLUGIN, "preset", preset.clone());
    }
    let requested_policy = spec
        .get("approval_policy")
        .filter(|v| !v.is_null())
        .cloned()
        .or_else(|| config.pointer("/plugins/approvals/policy").filter(|v| !v.is_null()).cloned());
    if let Some(policy) = narrow_child_approval_policy(requested_policy, spec.get("approval_policy_ceilings")) {
        set_plugin_config_leaf(&mut config, mare_shared::APPROVALS_SERVICE_NAME, "policy", policy);
    }
    Ok((config, carried))
}

/// Pin the parent's live approval-policy ceilings, review requirement, and required services onto
/// `spec` so a child's independent approval gate cannot relax its parent's live override.
pub(super) fn inherit_approval_gates(lua: &Lua, spec: &mut serde_json::Value) -> mlua::Result<()> {
    if let Some(host) = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade())
        && let Some(approvals) = host.service_table(mare_shared::APPROVALS_SERVICE_NAME)
    {
        let policy: Function = approvals.get("inheritance")?;
        let inherited: Value = policy.call(approvals.clone())?;
        spec["approval_policy_ceilings"] = lua_to_json(lua, inherited)?;
        let required: Function = approvals.get("review_required")?;
        spec["approval_review_required"] = required.call::<bool>(approvals.clone())?.into();
        let requirements: Function = approvals.get("delegation_services")?;
        let names: Vec<String> = requirements.call(approvals)?;
        let mut inherited = lua
            .globals()
            .get::<Table>("mare")?
            .get::<Table>("args")?
            .get::<Option<Vec<String>>>("required_services")?
            .unwrap_or_default();
        inherited.extend(names);
        inherited.sort();
        inherited.dedup();
        spec["required_services"] = serde_json::json!(inherited);
    }
    Ok(())
}

/// Effective sandbox policy for spawning or resyncing a child: intersects any active frozen tool
/// context with the parent VM's live policy and inherited ceiling so an in-flight tool call cannot
/// smuggle a pre-tightening sandbox to a newly created descendant.
fn effective_parent_policy(lua: &Lua) -> mlua::Result<Option<mare_binding::ossandbox::Policy>> {
    let active = mare_binding::exec_ctx::active(lua)?.map(|a| a.policy);
    let live = mare_binding::ossandbox::enforce_ceiling(lua, mare_binding::ossandbox::current(lua));
    Ok(match (active, live) {
        (Some(a), Some(b)) => Some(a.intersect(&b)),
        (a, b) => a.or(b),
    })
}

/// Whether a child spawned with raw caller `spec` inherits the parent's live preset rather than
/// pinning an explicit permission preset, ceiling, or custom permissions plugin configuration
/// (such as built-in `explore`/`plan`, `/review`, or caller-supplied `permission = "read-only"`).
pub(super) fn inherits_parent_preset(spec: &serde_json::Value) -> bool {
    spec.get("permission").is_none_or(serde_json::Value::is_null)
        && spec.get("permission_ceiling").is_none_or(serde_json::Value::is_null)
        && crate::approvals::shared_rule::shares_parent_rules(spec)
}

/// Snapshot the parent's current live preset and sandbox policy for resynchronizing an existing
/// unpinned subagent on continuation (`steer`). Captures `child_scope.tighten_seq()` first so any
/// concurrent tightening after this snapshot rejects a stale widening.
pub(super) fn parent_permission_sync(
    lua: &Lua,
    child_scope: &crate::agent_tree::Scope,
) -> mlua::Result<crate::agent_registry::PermissionSync> {
    let expected_tighten_seq = child_scope.tighten_seq();
    let parent_policy = effective_parent_policy(lua)?;
    let inherited_preset = preset_for_policy(parent_policy.as_ref());
    let mut tmp = serde_json::json!({});
    inherit_permissions(lua, &mut tmp, inherited_preset)?;
    let preset = tmp.get("permission").and_then(serde_json::Value::as_str).unwrap_or(inherited_preset).to_owned();
    Ok(crate::agent_registry::PermissionSync { preset, policy: parent_policy, expected_tighten_seq })
}

/// Apply sandbox, permission, config, approval-gate, and parameter inheritance onto `spec`.
pub(super) fn apply_inheritance(
    lua: &Lua,
    spec: &mut serde_json::Value,
) -> mlua::Result<(Option<mare_binding::ossandbox::Policy>, serde_json::Value, mare_shared::carry::Carried)> {
    // the sandbox in force for this call intersected with the parent VM's live policy/ceiling:
    // the child starts inside both and its ceiling never exceeds either.
    let parent_policy = effective_parent_policy(lua)?;
    let inherited_preset = preset_for_policy(parent_policy.as_ref());
    inherit_permissions(lua, spec, inherited_preset)?;
    inherit_approval_gates(lua, spec)?;
    let (config, carried) = merge_child_config(lua, spec)?;
    inherit_params(lua, spec, &config);
    Ok((parent_policy, config, carried))
}

/// Validate `spec.output_schema` if present and resolve `history_model` for forked histories.
pub(super) fn validate_and_resolve_history_model(lua: &Lua, spec: &serde_json::Value) -> mlua::Result<Option<String>> {
    // Forked history must be priced before the child switches from its source model.
    if let Some(schema) = spec.get("output_schema").filter(|v| !v.is_null()) {
        mare_shared::schema::OutputSchema::compile(schema.clone())
            .map_err(|e| rt_err(format!("invalid output_schema: {e}")))?;
    }
    // This is runtime state: the parent may have selected a session-only model.
    if spec.get("messages").and_then(serde_json::Value::as_array).is_some_and(|messages| !messages.is_empty()) {
        let host = lua
            .app_data_ref::<ParentHost>()
            .and_then(|p| p.0.upgrade())
            .ok_or_else(|| rt_err("cannot identify the source model of forked history"))?;
        let svc = host
            .service_table(mare_shared::LLM_SERVICE_NAME)
            .ok_or_else(|| rt_err("cannot identify the source model of forked history"))?;
        if spec.get("_fork_origin").is_some() {
            if let Some(effort) = spec.get("_history_effort").filter(|value| !value.is_null())
                && effort.as_str().is_none_or(str::is_empty)
            {
                return Err(rt_err("snapshot source effort must be a non-empty string"));
            }
            let source = spec
                .get("_history_model")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| rt_err("cannot identify the snapshot's source model"))?;
            let resolve: Function = svc.get("resolve")?;
            let resolved: Option<Table> = resolve.call((svc, source))?;
            let model = resolved.ok_or_else(|| rt_err("snapshot source model is unavailable"))?;
            return Ok(Some(model.get::<String>("ref")?));
        }
        let current: Function = svc.get("current")?;
        let model: Table = current.call(svc)?;
        Ok(Some(model.get::<String>("ref")?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inheritance_helpers_narrow_untrusted_ceiling_initialize_non_object_config_and_reject_invalid_schema() {
        let mut cfg = serde_json::json!(null);
        set_plugin_config_leaf(&mut cfg, "permissions", "preset", serde_json::json!("auto"));
        assert_eq!(cfg["plugins"]["permissions"]["preset"], "auto");

        let narrowed =
            narrow_child_approval_policy(Some(serde_json::json!("ask")), Some(&serde_json::json!(["untrusted"])));
        assert_eq!(narrowed, Some(serde_json::json!("untrusted")));

        let lua = Lua::new();
        let bad_schema = validate_and_resolve_history_model(
            &lua,
            &serde_json::json!({ "output_schema": { "type": "not_a_json_schema_type" } }),
        );
        assert!(bad_schema.is_err());

        let no_host = validate_and_resolve_history_model(
            &lua,
            &serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        );
        assert!(no_host.is_err());
    }
}
