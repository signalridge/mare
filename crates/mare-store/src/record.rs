//! The fields of session records interpreted by the host. Unknown fields are retained.

use mare_protocol::session_records::Message;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

macro_rules! record {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Default, Serialize, Deserialize)]
        pub struct $name {
            $(#[serde(default, skip_serializing_if = "Option::is_none")]
            pub $field: Option<$ty>,)*
            #[serde(flatten)]
            pub extra: Map<String, Value>,
        }
    };
}

record!(SessionHeader {
    session_id: String,
    version: i64,
    cwd: String,
    created_at_ms: i64,
    parent_session_id: String,
    run_id: String,
    label: String,
});
record!(MessageRecord { entry_id: String, parent_entry_id: Value, created_at_ms: i64, message: Message });
record!(ToolState { entry_id: String, state: String, tool_call_id: String, created_at_ms: i64 });
record!(State { state: String, created_at_ms: i64 });
record!(Name { name: String });
record!(Label { target_entry_id: Value, label: String, created_at_ms: i64 });
record!(Archive { archived: bool, created_at_ms: i64 });
record!(Compaction { entry_id: String, parent_entry_id: Value, first_kept_entry_id: String, created_at_ms: i64 });
record!(Custom { entry_id: String, kind: String, data: Value, created_at_ms: i64 });
record!(RunEndData { session_id: String, run_id: String, leaf_entry_id: String });

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkOriginData {
    pub source_session_id: String,
    pub source_leaf_entry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_through_entry_id: Option<String>,
}

impl ToolState {
    pub fn is_started_for(&self, id: &str) -> bool {
        self.state.as_deref() == Some("started") && self.tool_call_id.as_deref() == Some(id)
    }
}

impl State {
    pub fn is_closed(&self) -> bool {
        self.state.as_deref() == Some("closed")
    }
}

impl Custom {
    pub fn is_run_end(&self) -> bool {
        self.kind.as_deref() == Some("agent/run-end")
    }
}

/// An ID may be carried by an open record type as well as by a core record.
pub fn entry_id(value: &Value) -> Option<&str> {
    value.get("entry_id").and_then(Value::as_str)
}

/// A typed core record whose `type` tag matches [`Self::TYPE`].
pub trait CoreRecord: DeserializeOwned {
    const TYPE: CoreRecordType;
}

/// Deserialize `value` as `T` only when its `type` field matches `T::TYPE`.
pub fn core<T: CoreRecord>(value: &Value) -> Option<T> {
    (CoreRecordType::of(value) == Some(T::TYPE)).then(|| T::deserialize(value).ok()).flatten()
}

macro_rules! core_records {
    ($(($variant:ident, $tag:literal, $ty:ty, $enum_ty:ty)),* $(,)?) => {
        /// The closed set of record types interpreted by the host. Other types are opaque.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum CoreRecordType {
            $($variant,)*
        }

        impl CoreRecordType {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            /// Canonical `"type"` tag string for this core record variant.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $tag,)*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($tag => Some(Self::$variant),)*
                    _ => None,
                }
            }

            pub fn of(value: &Value) -> Option<Self> {
                Self::from_name(value.get("type")?.as_str()?)
            }

            fn validate_fields(self, value: &Value) -> Result<(), String> {
                match self {
                    $(Self::$variant => serde_path_to_error::deserialize::<_, $ty>(value)
                        .map(|_| ())
                        .map_err(|error| format!("{}: {}", error.path(), error.inner())),)*
                }
            }
        }

        $(impl CoreRecord for $ty {
            const TYPE: CoreRecordType = CoreRecordType::$variant;
        })*

        /// Tagged records used by readers. The original JSON values remain the persistence format.
        #[derive(Clone, Debug, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum Record {
            $($variant($enum_ty),)*
        }
    };
}

core_records!(
    (Session, "session", SessionHeader, SessionHeader),
    (Message, "message", MessageRecord, Box<MessageRecord>),
    (ToolState, "tool_state", ToolState, ToolState),
    (State, "state", State, State),
    (Name, "name", Name, Name),
    (Label, "label", Label, Label),
    (Archive, "archive", Archive, Archive),
    (Compaction, "compaction", Compaction, Compaction),
    (Custom, "custom", Custom, Custom),
);

/// Enabled always in debug/test builds, and in release when `MARE_VALIDATE_RECORDS=1`
/// (measured ~32–35% overhead on a 1 MiB message append, above the 5% always-on threshold).
pub(crate) fn validation_enabled() -> bool {
    cfg!(any(debug_assertions, test)) || std::env::var_os("MARE_VALIDATE_RECORDS").is_some_and(|v| v == "1")
}

/// Parse the same fields used by readers before any bytes in a batch are written.
pub(crate) fn validate(value: &Value) -> Result<(), String> {
    let Some(kind) = CoreRecordType::of(value) else { return Ok(()) };
    kind.validate_fields(value)?;
    // Other custom record kinds are owned by plugins and stay opaque.
    if kind == CoreRecordType::Custom && value.get("kind").and_then(Value::as_str) == Some("agent/run-end") {
        let data = value.get("data").ok_or_else(|| "data: missing field".to_string())?;
        serde_path_to_error::deserialize::<_, RunEndData>(data)
            .map(|_| ())
            .map_err(|error| format!("data.{}: {}", error.path(), error.inner()))?;
    }
    if kind == CoreRecordType::Custom && value.get("kind").and_then(Value::as_str) == Some("session/fork-origin") {
        let data = value.get("data").ok_or_else(|| "data: missing field".to_string())?;
        let origin: ForkOriginData = serde_path_to_error::deserialize(data)
            .map_err(|error| format!("data.{}: {}", error.path(), error.inner()))?;
        if origin.source_session_id.is_empty()
            || origin.source_leaf_entry_id.is_empty()
            || origin.inherited_through_entry_id.as_deref().is_some_and(str::is_empty)
            || data.get("inherited_through_entry_id").is_some_and(Value::is_null)
        {
            return Err("data: fork-origin identities must be nonempty strings".into());
        }
    }
    Ok(())
}
