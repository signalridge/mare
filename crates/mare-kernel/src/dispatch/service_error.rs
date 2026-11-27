//! Service call errors: one value `{code, message, data?}` for Lua and RPC
//! callers. A provider's error keeps its code and data only when the code is a core code or one
//! its service declares (`errors`) and the data encodes as JSON; anything else is a bounded
//! `internal` error that carries no raw error value.
use std::collections::BTreeMap;

use mlua::{Lua, Table, Value};
use serde_json::Value as Json;

use super::contracts::ErrorSpec;

/// A coded error value: what a Lua caller catches and what RPC sends as `error.data`.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorValue {
    pub code: String,
    pub message: String,
    /// The code's additional data (`error.data.data` over RPC).
    pub data: Option<Json>,
}

#[derive(Clone, Debug)]
pub enum ServiceCallError {
    Unavailable,
    MethodUnavailable,
    CallerDisposed,
    Cancelled,
    Arguments(String),
    Result(String),
    /// Any other failure: `internal`. `label` names the call (`service.method`); `detail` is the
    /// bounded original, shown to in-process Lua callers only and logged.
    Internal {
        label: String,
        detail: String,
    },
    CallbackTimeout,
    CallbackDisconnected,
    Unconfined,
    /// The connection this work acts for may not perform the operation.
    Denied,
    /// A coded error value raised by a guard, a provider or a remote implementation.
    Coded(ErrorValue),
}

impl ServiceCallError {
    /// The stable error code (RPC `error.data.code`); the message is for people.
    pub fn code(&self) -> &str {
        match self {
            Self::Unavailable => "unavailable",
            Self::MethodUnavailable => "method_not_found",
            Self::CallerDisposed | Self::Cancelled => "cancelled",
            Self::Arguments(_) => "invalid_arguments",
            Self::Result(_) => "invalid_result",
            Self::Internal { .. } => "internal",
            Self::CallbackTimeout => "timed_out",
            Self::CallbackDisconnected => "disconnected",
            Self::Unconfined | Self::Denied => "denied",
            Self::Coded(value) => &value.code,
        }
    }
    /// The code's additional data, if any.
    pub fn data(&self) -> Option<&Json> {
        match self {
            Self::Coded(value) => value.data.as_ref(),
            _ => None,
        }
    }
    /// The message for a caller outside the process (RPC, a peer): an `internal` error says
    /// only which call failed.
    pub fn public_message(&self) -> String {
        match self {
            Self::Internal { label, .. } => format!("{label} failed (details are in the log)"),
            other => other.message(),
        }
    }
    /// A coded error from a code and a message (no data).
    pub fn coded(code: &str, message: impl Into<String>) -> Self {
        Self::Coded(ErrorValue { code: code.to_owned(), message: message.into(), data: None })
    }
}

impl ServiceCallError {
    /// The message alone, for people (`message` of the Lua value, RPC `error.message`).
    pub fn message(&self) -> String {
        Message(self).to_string()
    }
}

/// Prints as `code: message`, like the Lua error value.
impl std::fmt::Display for ServiceCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message())
    }
}

struct Message<'a>(&'a ServiceCallError);
impl std::fmt::Display for Message<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ServiceCallError::*;
        match self.0 {
            Unavailable => formatter.write_str("service provider is unavailable"),
            MethodUnavailable => formatter.write_str("service method is not exported"),
            CallerDisposed => formatter.write_str("service caller is disposed"),
            Cancelled => formatter.write_str("service call was cancelled"),
            Arguments(message) => write!(formatter, "invalid service arguments: {message}"),
            Result(message) => write!(formatter, "invalid service result: {message}"),
            Internal { label, detail } => write!(formatter, "{label} failed: {detail}"),
            CallbackTimeout => formatter.write_str("service callback timed out"),
            CallbackDisconnected => formatter.write_str("service callback disconnected"),
            Unconfined => formatter.write_str("unconfined service backend is unavailable inside a sandbox"),
            Denied => formatter.write_str("operation is not allowed for the connection this runs for"),
            Coded(value) => formatter.write_str(&value.message),
        }
    }
}
impl std::error::Error for ServiceCallError {}

/// A Rust-side failure: a service call error anywhere in its chain keeps its code; anything
/// else is `internal`. Codes are never recovered from text.
impl From<mlua::Error> for ServiceCallError {
    fn from(error: mlua::Error) -> Self {
        if let Some(known) = error.chain().find_map(|cause| cause.downcast_ref::<Self>()) {
            return known.clone();
        }
        if let Some(coded) = error.chain().find_map(|cause| cause.downcast_ref::<mare_shared::errors::Coded>()) {
            return Self::coded(coded.code, mare_shared::errors::bounded(&coded.message));
        }
        internal("service call", &error.to_string())
    }
}

/// `internal` for `label` (`service.method`). The original is logged; callers outside the
/// process see only [`ServiceCallError::public_message`].
pub fn internal(label: &str, original: &str) -> ServiceCallError {
    let detail = mare_shared::errors::internal_message(original);
    tracing::warn!(target: "mare::service", "{label} failed: {detail}");
    ServiceCallError::Internal { label: label.to_owned(), detail }
}

/// What a caller of `service.method` gets for a raised Lua value `raised`: a coded
/// table keeps its code, message and data when the code is a core code, or one of `declared`
/// (the errors this method may raise) whose data matches its schema; a Rust service error keeps
/// its code; anything else is `internal`.
pub(crate) fn settle_error(
    lua: &Lua,
    service: &str,
    method: &str,
    declared: &BTreeMap<String, ErrorSpec>,
    raised: Value,
) -> ServiceCallError {
    let label = format!("{service}.{method}");
    let checked = match raised {
        Value::Error(error) => match ServiceCallError::from(*error) {
            // a value from Rust: a remote implementation's JSON is checked as it was sent
            ServiceCallError::Coded(value) => check(service, declared, value, false),
            ServiceCallError::Internal { detail, .. } => match mare_shared::lua_error::recall_error(lua, &detail) {
                Some(table) => from_table(lua, &table).and_then(|value| check(service, declared, value, true)),
                None => Err(detail),
            },
            other => Ok(other),
        },
        Value::Table(table) => from_table(lua, &table).and_then(|value| check(service, declared, value, true)),
        Value::String(text) => match mare_shared::lua_error::recall_error(lua, &text.to_string_lossy()) {
            Some(table) => from_table(lua, &table).and_then(|value| check(service, declared, value, true)),
            None => Err(text.to_string_lossy()),
        },
        other => Err(other.to_string().unwrap_or_else(|_| format!("a {} error value", other.type_name()))),
    };
    checked.unwrap_or_else(|why| internal(&label, &why))
}

/// A coded value whose code this caller may see, or why not.
fn check(
    service: &str,
    declared: &BTreeMap<String, ErrorSpec>,
    mut value: ErrorValue,
    from_lua: bool,
) -> Result<ServiceCallError, String> {
    // an `internal` value (an inner call's, passed on unchanged) carries an in-process detail:
    // it is internal again here, never a coded value whose message would go out as it is
    if value.code == "internal" {
        return Err(value.message);
    }
    if mare_shared::errors::is_core(&value.code) {
        return Ok(ServiceCallError::Coded(value));
    }
    let spec = value
        .code
        .strip_prefix(service)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|name| declared.get(name))
        .or_else(|| declared.get(&value.code));
    let Some(spec) = spec else {
        return Err(format!(
            "error code `{}` is neither a core code nor declared for this method: {}",
            value.code, value.message
        ));
    };
    match (&spec.data, value.data.take()) {
        (None, None) => {}
        (None, Some(_)) => return Err(format!("`{}` declares no data but carries some", value.code)),
        (Some(schema), data) => {
            let mut data = data.unwrap_or(Json::Null);
            // a Lua table's empty tables take the shape the data schema declares; JSON from
            // outside the process is checked as it is
            if from_lua {
                mare_shared::schema::shape_empty(&mut data, schema);
            }
            mare_shared::schema::Schema::compile_contract(schema.clone())
                .and_then(|schema| schema.validate(&data))
                .map_err(|why| format!("`{}` data does not match its schema: {why}", value.code))?;
            value.data = Some(data);
        }
    }
    Ok(ServiceCallError::Coded(value))
}

fn from_table(lua: &Lua, table: &Table) -> Result<ErrorValue, String> {
    let Ok(Value::String(code)) = table.raw_get::<Value>("code") else {
        return Err("a table error without a string `code`".into());
    };
    let mut code = code.to_string_lossy();
    // 四: CordisError("INACTIVE_EFFECT") normalizes to core `inactive_ctx`.
    if code == "INACTIVE_EFFECT"
        && matches!(table.raw_get::<Value>("name"), Ok(Value::String(ref name)) if name.as_bytes() == b"CordisError")
    {
        code = "inactive_ctx".to_owned();
    }
    let message = match table.raw_get::<Value>(mare_shared::MESSAGE_KEY) {
        Ok(Value::String(message)) => mare_shared::errors::bounded(&message.to_string_lossy()),
        _ => code.clone(),
    };
    let data = match table.raw_get::<Value>("data") {
        Ok(Value::Nil) | Err(_) => None,
        Ok(value) => Some(
            mare_shared::lua::strict_lua_to_json(lua, value)
                .map_err(|error| format!("`{code}` data is not JSON: {error}"))?,
        ),
    };
    Ok(ErrorValue { code, message, data })
}

/// The Lua error value of `error`: `{code, message, data?}` with the `mare.Error` metatable
/// (prints as `code: message`).
pub(crate) fn lua_error(lua: &Lua, meta: &Table, error: &ServiceCallError) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    table.set("code", error.code())?;
    table.set(mare_shared::MESSAGE_KEY, error.message())?;
    if let Some(data) = error.data() {
        table.set("data", mare_shared::lua::json_to_lua_lossless(lua, data)?)?;
    }
    table.set_metatable(Some(meta.clone()))?;
    Ok(Value::Table(table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_error_preserves_inactive_ctx_and_maps_cordis_inactive_effect() {
        let lua = Lua::new();
        let declared = BTreeMap::new();

        let coded = lua.create_table().unwrap();
        coded.set("code", "inactive_ctx").unwrap();
        coded.set("message", "the caller's context was disposed").unwrap();
        let settled = settle_error(&lua, "rpc", "hook", &declared, Value::Table(coded));
        assert_eq!(settled.code(), "inactive_ctx");

        let cordis = lua.create_table().unwrap();
        cordis.set("name", "CordisError").unwrap();
        cordis.set("code", "INACTIVE_EFFECT").unwrap();
        cordis.set("message", "cannot create effect on inactive context").unwrap();
        let mapped = settle_error(&lua, "reg", "add", &declared, Value::Table(cordis));
        assert_eq!(mapped.code(), "inactive_ctx");

        let unknown = lua.create_table().unwrap();
        unknown.set("code", "NOT_A_MARE_CODE").unwrap();
        unknown.set("message", "boom").unwrap();
        let clamped = settle_error(&lua, "reg", "add", &declared, Value::Table(unknown));
        assert_eq!(clamped.code(), "internal");

        // Non-CordisError tables carrying INACTIVE_EFFECT must still clamp to internal.
        let non_cordis = lua.create_table().unwrap();
        non_cordis.set("name", "OtherError").unwrap();
        non_cordis.set("code", "INACTIVE_EFFECT").unwrap();
        non_cordis.set("message", "not cordis").unwrap();
        let clamped_non_cordis = settle_error(&lua, "reg", "add", &declared, Value::Table(non_cordis));
        assert_eq!(clamped_non_cordis.code(), "internal");
    }

    #[test]
    fn settle_error_recalls_mare_error_across_internal_and_string_boundaries() {
        let lua = Lua::new();
        let mut declared = BTreeMap::new();
        declared.insert("active_session".to_owned(), ErrorSpec { doc: "active".to_owned(), data: None });

        let err_tbl =
            mare_shared::lua_error::error_value(&lua, "session.active_session", "still open", Value::Nil).unwrap();
        let printed: String = lua.load("return tostring(...)").call(err_tbl).unwrap();
        let from_err = Value::Error(Box::new(mlua::Error::external(ServiceCallError::Internal {
            label: "boundary".to_owned(),
            detail: format!("runtime error: {printed}\nstack traceback:\n\there"),
        })));
        let settled_err = settle_error(&lua, "session", "delete", &declared, from_err);
        assert_eq!(settled_err.code(), "session.active_session");
        assert_eq!(settled_err.message(), "still open");

        let core_tbl = mare_shared::lua_error::error_value(&lua, "unknown_session", "missing", Value::Nil).unwrap();
        let core_printed: String = lua.load("return tostring(...)").call(core_tbl).unwrap();
        let settled_str = settle_error(
            &lua,
            "session",
            "delete",
            &declared,
            Value::String(lua.create_string(&core_printed).unwrap()),
        );
        assert_eq!(settled_str.code(), "unknown_session");
        assert_eq!(settled_str.message(), "missing");

        // Recalled errors with "internal" or undeclared code must still clamp to `internal`.
        let internal_tbl = mare_shared::lua_error::error_value(&lua, "internal", "secret detail", Value::Nil).unwrap();
        let internal_printed: String = lua.load("return tostring(...)").call(internal_tbl).unwrap();
        let clamped_internal = settle_error(
            &lua,
            "session",
            "delete",
            &declared,
            Value::Error(Box::new(mlua::Error::external(ServiceCallError::Internal {
                label: "boundary".to_owned(),
                detail: internal_printed,
            }))),
        );
        assert_eq!(clamped_internal.code(), "internal");

        let undeclared_tbl =
            mare_shared::lua_error::error_value(&lua, "session.undeclared_code", "bad", Value::Nil).unwrap();
        let undeclared_printed: String = lua.load("return tostring(...)").call(undeclared_tbl).unwrap();
        let clamped_undeclared = settle_error(
            &lua,
            "session",
            "delete",
            &declared,
            Value::String(lua.create_string(&undeclared_printed).unwrap()),
        );
        assert_eq!(clamped_undeclared.code(), "internal");
    }

    #[test]
    fn settle_error_preserves_coded_error_across_callback_error_chain() {
        let lua = Lua::new();
        let declared = BTreeMap::new();
        let wrapped = mlua::Error::CallbackError {
            traceback: "stack traceback:\n\ttest".to_owned(),
            cause: std::sync::Arc::new(mlua::Error::external(mare_shared::errors::Coded {
                code: "unavailable",
                message: "session unavailable in this workspace".to_owned(),
            })),
        };
        let settled = settle_error(&lua, "session", "search_items", &declared, Value::Error(Box::new(wrapped)));
        assert_eq!(settled.code(), "unavailable");
        assert_eq!(settled.message(), "session unavailable in this workspace");
    }

    #[test]
    fn converted_coded_error_message_is_bounded_at_a_utf8_boundary() {
        let prefix = "a".repeat(mare_shared::errors::MESSAGE_LIMIT - 1);
        let error = mlua::Error::external(mare_shared::errors::Coded {
            code: "invalid_arguments",
            message: format!("{prefix}界 and more"),
        });
        let mapped = ServiceCallError::from(error);
        assert_eq!(mapped.code(), "invalid_arguments");
        assert_eq!(mapped.public_message(), prefix);
    }

    #[test]
    fn settle_callback_runtime_error_hides_private_details_from_public_message() {
        let lua = Lua::new();
        let secret_path = "/home/example/private-session/history.jsonl";
        let wrapped = mlua::Error::CallbackError {
            traceback: format!("traceback at {secret_path}"),
            cause: std::sync::Arc::new(mlua::Error::RuntimeError(format!("cannot read {secret_path}"))),
        };
        let settled = settle_error(&lua, "session", "search_items", &BTreeMap::new(), Value::Error(Box::new(wrapped)));
        assert_eq!(settled.code(), "internal");
        assert!(settled.message().contains(secret_path));
        assert_eq!(settled.public_message(), "session.search_items failed (details are in the log)");
        assert!(!settled.public_message().contains(secret_path));
    }
}
