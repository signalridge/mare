//! LuaLS declarations generated from service contracts: JSON Schema to LuaLS
//! types, and one `*.gen.lua` class per typed service. Shared by the contract tooling and the
//! event and config registries (separate from contract parsing, contracts.rs).
use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{Value as Json, json};

use super::contracts::Expose;
use crate::ServiceContract;

/// The LuaLS type the generator writes for a JSON Schema (what a titled class's field is checked
/// against).
pub fn luals_schema_type(schema: &Json) -> String {
    luals_type(schema)
}

/// The LuaLS type of a JSON Schema (the subset contracts use).
fn luals_type(schema: &Json) -> String {
    // a Lua-only field (a callback) carries its LuaLS type; a union return is parenthesized
    if let Some(Json::String(kind)) = schema.get("luals") {
        return wrap(kind);
    }
    // a schema titled with a LuaLS class (`mare.ThemeSnapshot`) is that class, whose field
    // types may say more than JSON can (mare.Style, mare.Node); `title` has no RPC meaning
    if let Some(Json::String(title)) = schema.get("title")
        && title.strip_prefix("mare.").is_some_and(|rest| {
            rest.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    {
        return title.clone();
    }
    if let Some(values) = schema.get("enum").and_then(Json::as_array) {
        return values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("|");
    }
    if let Some(branches) = schema.get("oneOf").or_else(|| schema.get("anyOf")).and_then(Json::as_array) {
        let kinds: Vec<String> = branches.iter().map(|b| wrap(&luals_type(b))).collect();
        if !kinds.is_empty() {
            return kinds.join("|");
        }
    }
    let kind = |t: &str| match t {
        "string" => "string".to_owned(),
        "integer" => "integer".to_owned(),
        "number" => "number".to_owned(),
        "boolean" => "boolean".to_owned(),
        "null" => "nil".to_owned(),
        "array" => match schema.get("items") {
            Some(items) => format!("{}[]", wrap(&luals_type(items))),
            None => "any[]".to_owned(),
        },
        "object" => object_type(schema),
        _ => "any".to_owned(),
    };
    match schema.get("type") {
        Some(Json::String(t)) => kind(t),
        Some(Json::Array(types)) => types.iter().filter_map(Json::as_str).map(kind).collect::<Vec<_>>().join("|"),
        _ => "any".to_owned(),
    }
}

fn wrap(t: &str) -> String {
    let has_return =
        t.starts_with("fun(") && t.rsplit_once(')').is_some_and(|(_, tail)| tail.trim_start().starts_with(':'));
    if t.contains('|') || has_return || (t.starts_with("fun(") && t.contains("?:")) {
        format!("({t})")
    } else {
        t.to_owned()
    }
}

fn object_type(schema: &Json) -> String {
    let required: BTreeSet<&str> =
        schema.get("required").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).collect();
    // `additionalProperties`: a schema types the other fields; `true` allows any; absent or
    // false closes the object
    let extra = match schema.get("additionalProperties") {
        Some(Json::Object(values)) => Some(luals_type(&Json::Object(values.clone()))),
        Some(Json::Bool(true)) => Some("any".to_owned()),
        _ => None,
    };
    match schema.get("properties").and_then(Json::as_object) {
        Some(properties) if !properties.is_empty() => {
            let mut fields: Vec<String> = properties
                .iter()
                .map(|(k, v)| format!("{k}{}: {}", if required.contains(k.as_str()) { "" } else { "?" }, luals_type(v)))
                .collect();
            // fields beyond the named ones keep their type too
            if let Some(values) = extra {
                fields.push(format!("[string]: {values}"));
            }
            format!("{{{}}}", fields.join(", "))
        }
        _ => match extra {
            Some(values) => format!("table<string, {values}>"),
            None => "table".to_owned(),
        },
    }
}

/// LuaLS declarations generated from a service contract (the contract is
/// the truth, the types derive from it). Remote-only methods are not Lua API.
/// `class` is the service's LuaLS class. (`write!` into a String cannot fail.)
pub fn luals_types(service: &str, class: &str, contract: &ServiceContract) -> String {
    let file = service.replace('-', "_");
    let mut out = format!(
        "---@meta _\n-- GENERATED from types/mare/services/contracts/{file}.lua (service `{service}`): do not edit.\n\
         -- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current\n\n\
         ---@class {class}\nlocal M = {{}}\n"
    );
    for (method, spec) in contract.methods.iter().filter(|(_, s)| matches!(s.expose, Expose::Lua | Expose::Both)) {
        out.push('\n');
        for line in spec.doc.lines().filter(|l| !l.is_empty()) {
            let _ = writeln!(out, "--- {line}");
        }
        let properties = spec.params.get("properties").and_then(Json::as_object);
        let required: BTreeSet<&str> = spec
            .params
            .get("required")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        let mut names = vec![];
        for name in &spec.order {
            if let Some(kind) = spec.callbacks.get(name) {
                // a Lua-only parameter: its declared LuaLS type (optional when it ends in `?`)
                let (kind, optional) = kind.strip_suffix('?').map_or((kind.as_str(), ""), |k| (k, "?"));
                let _ = writeln!(out, "---@param {name}{optional} {kind}");
                names.push(name.clone());
                continue;
            }
            let schema = properties.and_then(|p| p.get(name)).cloned().unwrap_or(Json::Null);
            let optional = if required.contains(name.as_str()) { "" } else { "?" };
            let _ = writeln!(out, "---@param {name}{optional} {}", luals_type(&schema));
            names.push(name.clone());
        }
        if spec.rest {
            // the trailing opts: the params not named in order, with their required list and
            // their additionalProperties; opts itself is optional only when nothing in it is
            let mut rest: serde_json::Map<String, Json> = properties
                .into_iter()
                .flatten()
                .filter(|(k, _)| !spec.order.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (name, kind) in spec.callbacks.iter().filter(|(name, _)| !spec.order.contains(name)) {
                rest.insert(name.clone(), json!({ "luals": kind }));
            }
            let rest_required: Vec<&str> = required.iter().copied().filter(|k| rest.contains_key(*k)).collect();
            let mut schema = json!({ "type": "object", "properties": rest, "required": rest_required });
            if let Some(extra) = spec.params.get("additionalProperties") {
                schema["additionalProperties"] = extra.clone();
            }
            let optional = if rest_required.is_empty() { "?" } else { "" };
            let _ = writeln!(out, "---@param opts{optional} {}", object_type(&schema));
            names.push("opts".to_owned());
        }
        if let Some(returns) = &spec.lua_result {
            for ret in returns {
                let _ = writeln!(out, "---@return {ret}");
            }
        } else if spec.registration {
            out.push_str("---@return fun() dispose releases this registration\n");
        } else {
            let _ = writeln!(out, "---@return {}", luals_type(&spec.result));
        }
        let _ = writeln!(out, "function M:{method}({}) end", names.join(", "));
    }
    out
}

/// Generate LuaLS declarations for the merged event registry (`protocol_events!` + built-in
/// contract `events`), written to `types/mare/events.gen.lua`.
pub fn luals_events(contracts: &std::collections::BTreeMap<String, ServiceContract>) -> String {
    fn class_ident(name: &str) -> String {
        let mut s = String::from("mare.event.");
        for ch in name.chars() {
            match ch {
                '/' => s.push_str("__"),
                '-' => s.push('_'),
                c => s.push(c),
            }
        }
        s
    }

    struct Entry {
        name: String,
        doc: String,
        mode: &'static str,
        delivery: &'static str,
        scope: &'static str,
        ignorable: bool,
        retain: bool,
        on_error: &'static str,
        data: Json,
        result: Option<Json>,
    }

    let mode_name = |m: mare_protocol::EventMode| match m {
        mare_protocol::EventMode::Emit => "emit",
        mare_protocol::EventMode::Parallel => "parallel",
        mare_protocol::EventMode::Serial => "serial",
        mare_protocol::EventMode::Bail => "bail",
        mare_protocol::EventMode::Waterfall => "waterfall",
        mare_protocol::EventMode::Policy => "policy",
        mare_protocol::EventMode::Rewrite => "rewrite",
    };
    let delivery_name = |d: mare_protocol::EventDelivery| match d {
        mare_protocol::EventDelivery::Required => "required",
        mare_protocol::EventDelivery::Lossy => "lossy",
    };
    let on_error_name = |e: mare_protocol::EventOnError| match e {
        mare_protocol::EventOnError::Continue => "continue",
        mare_protocol::EventOnError::Abort => "abort",
    };

    let mut entries = std::collections::BTreeMap::new();
    for def in mare_protocol::event_registry::PROTOCOL_EVENTS {
        let info = def.to_info();
        entries.insert(
            info.name.clone(),
            Entry {
                name: info.name,
                doc: info.doc,
                mode: mode_name(info.mode),
                delivery: delivery_name(info.delivery),
                scope: info.scope.as_str(),
                ignorable: info.ignorable,
                retain: info.retain,
                on_error: on_error_name(info.on_error),
                data: info.data_schema,
                result: info.result_schema,
            },
        );
    }
    for contract in contracts.values() {
        for spec in contract.events.values() {
            entries.insert(
                spec.name.clone(),
                Entry {
                    name: spec.name.clone(),
                    doc: spec.doc.clone(),
                    mode: mode_name(spec.mode),
                    delivery: delivery_name(spec.delivery),
                    scope: spec.scope.as_str(),
                    ignorable: spec.ignorable,
                    retain: spec.retain.is_some(),
                    on_error: on_error_name(spec.on_error),
                    data: spec.data.clone(),
                    result: spec.result.clone(),
                },
            );
        }
    }

    let mut out = String::from(
        "---@meta _\n\
         -- GENERATED from mare-protocol event registry and built-in service contracts: do not edit.\n\
         -- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current\n\n\
         ---@class mare.EventEnvelope\n\
         ---@field type string\n\
         ---@field session_id? string\n\
         ---@field session_epoch string\n\
         ---@field seq? integer\n\
         ---@field turn_id? string\n\
         ---@field next? fun(data?: any): any\n\n\
         ---@class mare.OnOptions\n\
         ---@field prepend? boolean\n\
         ---@field global? boolean\n",
    );

    let mut overloads = Vec::new();
    for entry in entries.values() {
        let ident = class_ident(&entry.name);
        out.push('\n');
        for line in entry.doc.lines().filter(|l| !l.is_empty()) {
            let _ = writeln!(out, "--- {line}");
        }
        let _ = writeln!(
            out,
            "--- `{}` (`mode = {}`, `delivery = {}`, `scope = {}`, `ignorable = {}`, `retain = {}`, `on_error = {}`)",
            entry.name, entry.mode, entry.delivery, entry.scope, entry.ignorable, entry.retain, entry.on_error
        );
        let props = entry.data.get("properties").and_then(Json::as_object);
        let required: BTreeSet<&str> = entry
            .data
            .get("required")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        if let Some(props) = props
            && !props.is_empty()
        {
            let _ = writeln!(out, "---@class {ident}");
            for (k, v) in props {
                let opt = if required.contains(k.as_str()) { "" } else { "?" };
                let _ = writeln!(out, "---@field {k}{opt} {}", luals_type(v));
            }
        } else {
            let _ = writeln!(out, "---@alias {ident} {}", luals_type(&entry.data));
        }

        if matches!(entry.mode, "policy" | "rewrite") {
            continue;
        }
        let ret = match &entry.result {
            Some(schema) => format!(": ({}|nil)", luals_type(schema)),
            None => ": any".to_owned(),
        };
        overloads.push(format!(
            "---@overload fun(self: mare.Ctx, name: \"{}\", listener: (fun(data: {ident}, ev: mare.EventEnvelope){ret}), options?: boolean|mare.OnOptions): fun()",
            entry.name
        ));
    }

    out.push_str("\n---@class mare.Ctx\nlocal Ctx = {}\n");
    for line in overloads {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(
        "---@param name string\n\
         ---@param listener function\n\
         ---@param options? boolean|mare.OnOptions\n\
         ---@return fun() dispose\n\
         function Ctx:on(name, listener, options) end\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_of_and_one_of_unions_render_to_luals() {
        assert_eq!(luals_schema_type(&json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] })), "string|nil");
        assert_eq!(
            luals_schema_type(&json!({ "oneOf": [{ "type": "integer" }, { "type": "boolean" }] })),
            "integer|boolean"
        );
    }
}
