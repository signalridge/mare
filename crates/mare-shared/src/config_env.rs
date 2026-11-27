// Shared `config_schema` `env = "MARE_..."` binding extraction.
//
// Used by both `mare-shared` at runtime (`core::config::plugin_env_bindings`) and
// `mare-plugin/build.rs` at build time (via `include!`) so env-binding discovery has a single
// source of truth.

use mlua::{Table, Value};

/// A plugin option bound to an environment variable via `env = "MARE_..."` in its `config_schema`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PluginEnvBinding {
    pub path: String,
    pub env_var: String,
    pub plugin: String,
    pub on_invalid: Option<String>,
}

/// Walk a plugin's `config_schema` table (including nested `object.fields`), appending every
/// `env = "MARE_..."` binding with inherited `on_invalid` behavior into `out`.
pub fn collect_schema_env_bindings(
    schema: &Table,
    plugin: &str,
    prefix: &str,
    inherited_on_invalid: Option<&str>,
    out: &mut Vec<PluginEnvBinding>,
) -> mlua::Result<()> {
    for kv in schema.pairs::<Value, Value>() {
        let (Value::String(k), Value::Table(entry)) = kv? else {
            continue;
        };
        let Ok(field) = k.to_str() else {
            continue;
        };
        let dotted = format!("{prefix}.{field}");
        let entry_on_invalid = entry.get::<Option<String>>("on_invalid").ok().flatten();
        let effective_on_invalid = entry_on_invalid.as_deref().or(inherited_on_invalid).map(str::to_string);
        if let Ok(Some(env_name)) = entry.get::<Option<String>>("env")
            && !env_name.is_empty()
        {
            out.push(PluginEnvBinding {
                path: dotted.clone(),
                env_var: env_name,
                plugin: plugin.to_string(),
                on_invalid: effective_on_invalid.clone(),
            });
        }
        if let Ok(Value::Table(fields)) = entry.get::<Value>("fields") {
            collect_schema_env_bindings(&fields, plugin, &dotted, effective_on_invalid.as_deref(), out)?;
        }
    }
    Ok(())
}
