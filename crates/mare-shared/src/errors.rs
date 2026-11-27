//! The error vocabulary: one string code space for Lua error values and
//! RPC `error.data.code`. Core codes are listed here; a service's own codes are declared by its
//! contract (`errors`) and written `<service>.<name>`. Every other error becomes `internal`.

/// Exhaustive enum of every core error code in [`CORE`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoreErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidArguments,
    AlreadyProvided,
    InvalidResult,
    Internal,
    Cancelled,
    InactiveCtx,
    TimedOut,
    Disconnected,
    Denied,
    Unavailable,
    UnknownSession,
    StaleSession,
    Overloaded,
    Busy,
    Unsupported,
    UnsupportedVersion,
    RequestIdConflict,
    PersistFailed,
    InvalidNamespace,
    InvalidValue,
    ReadOnly,
    LockTimeout,
    Corrupt,
    RevisionExhausted,
    Io,
    UnknownState,
}

impl CoreErrorCode {
    /// Every core error code in canonical [`CORE`] order.
    pub const ALL: &'static [Self] = &[
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidArguments,
        Self::AlreadyProvided,
        Self::InvalidResult,
        Self::Internal,
        Self::Cancelled,
        Self::InactiveCtx,
        Self::TimedOut,
        Self::Disconnected,
        Self::Denied,
        Self::Unavailable,
        Self::UnknownSession,
        Self::StaleSession,
        Self::Overloaded,
        Self::Busy,
        Self::Unsupported,
        Self::UnsupportedVersion,
        Self::RequestIdConflict,
        Self::PersistFailed,
        Self::InvalidNamespace,
        Self::InvalidValue,
        Self::ReadOnly,
        Self::LockTimeout,
        Self::Corrupt,
        Self::RevisionExhausted,
        Self::Io,
        Self::UnknownState,
    ];

    /// Wire/Lua string identifier for this core code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::InvalidArguments => "invalid_arguments",
            Self::AlreadyProvided => "already_provided",
            Self::InvalidResult => "invalid_result",
            Self::Internal => "internal",
            Self::Cancelled => "cancelled",
            Self::InactiveCtx => "inactive_ctx",
            Self::TimedOut => "timed_out",
            Self::Disconnected => "disconnected",
            Self::Denied => "denied",
            Self::Unavailable => "unavailable",
            Self::UnknownSession => "unknown_session",
            Self::StaleSession => "stale_session",
            Self::Overloaded => "overloaded",
            Self::Busy => "busy",
            Self::Unsupported => "unsupported",
            Self::UnsupportedVersion => "unsupported_version",
            Self::RequestIdConflict => "request_id_conflict",
            Self::PersistFailed => "persist_failed",
            Self::InvalidNamespace => "invalid_namespace",
            Self::InvalidValue => "invalid_value",
            Self::ReadOnly => "read_only",
            Self::LockTimeout => "lock_timeout",
            Self::Corrupt => "corrupt",
            Self::RevisionExhausted => "revision_exhausted",
            Self::Io => "io",
            Self::UnknownState => "unknown_state",
        }
    }

    /// Parse a wire/Lua string code into its core enum variant, if it is a core code.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|variant| variant.as_str() == code)
    }

    /// JSON-RPC numeric code for this core error code.
    pub const fn rpc_number(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidArguments => -32602,
            Self::Internal => -32603,
            Self::Cancelled => -32800,
            Self::InvalidResult
            | Self::AlreadyProvided
            | Self::InactiveCtx
            | Self::TimedOut
            | Self::Disconnected
            | Self::Denied
            | Self::Unavailable
            | Self::UnknownSession
            | Self::StaleSession
            | Self::Overloaded
            | Self::Busy
            | Self::Unsupported
            | Self::UnsupportedVersion
            | Self::RequestIdConflict
            | Self::PersistFailed
            | Self::InvalidNamespace
            | Self::InvalidValue
            | Self::ReadOnly
            | Self::LockTimeout
            | Self::Corrupt
            | Self::RevisionExhausted
            | Self::Io
            | Self::UnknownState => SERVER_ERROR,
        }
    }

    /// Human-readable specification description from [`CORE`].
    pub fn meaning(self) -> &'static str {
        CORE.iter()
            .find_map(|(name, meaning)| (*name == self.as_str()).then_some(*meaning))
            .expect("every CoreErrorCode variant is registered in CORE")
    }
}

/// Core codes, with what each means. `mare-protocol` re-exports this list and [`CoreErrorCode`]
/// as `ErrorCode`; the state namespace codes share the space.
pub const CORE: &[(&str, &str)] = &[
    ("parse_error", "the message is not valid JSON"),
    ("invalid_request", "the message is not a valid request"),
    ("method_not_found", "no such method, or it is not exported to this caller"),
    ("invalid_arguments", "the arguments do not match the method's schema"),
    ("already_provided", "the service already has a provider"),
    ("invalid_result", "the provider's result does not match the method's schema"),
    ("internal", "an unexpected failure; the message is bounded and carries no raw error value"),
    ("cancelled", "cancelled by the caller, an owner release or a shutdown"),
    ("inactive_ctx", "the caller's context was disposed; it can no longer register anything"),
    ("timed_out", "a deadline passed before the work finished"),
    ("disconnected", "the connection that implements or awaits the call went away"),
    ("denied", "not allowed for the connection this work runs for"),
    ("unavailable", "the service or its provider is not available"),
    ("unknown_session", "no session with that id"),
    ("stale_session", "the session changed since the id was obtained"),
    ("overloaded", "admission refused before the work started; try again later"),
    ("busy", "the target cannot take the request now and it cannot be queued"),
    ("unsupported", "no backend or model supports the requested capability"),
    ("unsupported_version", "the protocol major version is not supported"),
    ("request_id_conflict", "a request id was reused with different content"),
    ("persist_failed", "durable session persistence failed"),
    // the state namespace (mare-protocol StateErrorCode)
    ("invalid_namespace", "the state namespace name is not valid"),
    ("invalid_value", "the state value is not valid"),
    ("read_only", "the state namespace is read-only here"),
    ("lock_timeout", "the state lock was not acquired in time"),
    ("corrupt", "the stored state cannot be read"),
    ("revision_exhausted", "the state revision counter is exhausted"),
    ("io", "the state could not be read or written"),
    ("unknown_state", "the state outcome is unknown"),
];

/// The longest error message passed on, in bytes; longer ones are cut at a character boundary.
pub const MESSAGE_LIMIT: usize = 1024;

/// JSON-RPC numbers: the five standard ones, `-32800` for cancellation (mare convention) and
/// `-32000` for everything else.
pub const SERVER_ERROR: i64 = -32000;

pub fn is_core(code: &str) -> bool {
    CORE.iter().any(|(name, _)| *name == code)
}

/// The JSON-RPC number of a code.
pub fn rpc_number(code: &str) -> i64 {
    match code {
        "parse_error" => -32700,
        "invalid_request" => -32600,
        "method_not_found" => -32601,
        "invalid_arguments" => -32602,
        "internal" => -32603,
        "cancelled" => -32800,
        _ => SERVER_ERROR,
    }
}

/// A valid name for a declared service error: lowercase snake_case.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Has `code` the shape of a declared service error, `<service>.<name>`? (Whether the service
/// declares it is the kernel's check against the contract.)
pub fn is_domain_code(code: &str) -> bool {
    let Some((service, name)) = code.rsplit_once('.') else { return false };
    !service.is_empty()
        && service.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.')
        && valid_name(name)
}

/// `<service>.<name>` of a declared service error.
pub fn domain_code(service: &str, name: &str) -> String {
    format!("{service}.{name}")
}

/// A message cut to [`MESSAGE_LIMIT`] bytes (first line only for an `internal` error, where the
/// rest is usually a traceback).
pub fn bounded(message: &str) -> String {
    let mut end = message.len().min(MESSAGE_LIMIT);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

/// The message of an `internal` error made from an arbitrary failure: its first line, bounded,
/// without mlua's `runtime error: ` prefixes.
pub fn internal_message(text: &str) -> String {
    let mut first = text.lines().next().unwrap_or_default();
    while let Some(rest) = first.strip_prefix("runtime error: ") {
        first = rest;
    }
    bounded(first)
}

/// A failure with a vocabulary code, raised from Rust code that has no service context (a
/// binding): callers outside the process keep its code instead of `internal`.
#[derive(Clone, Debug)]
pub struct Coded {
    pub code: &'static str,
    pub message: String,
}
impl std::fmt::Display for Coded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for Coded {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_error_code_enum_is_exact_bijection_with_core_table() {
        assert_eq!(CoreErrorCode::ALL.len(), CORE.len());
        for (variant, (code, meaning)) in CoreErrorCode::ALL.iter().zip(CORE.iter()) {
            assert_eq!(variant.as_str(), *code);
            assert_eq!(variant.meaning(), *meaning);
            assert_eq!(variant.rpc_number(), rpc_number(code));
            assert_eq!(CoreErrorCode::from_code(code), Some(*variant));
        }
        assert_eq!(CoreErrorCode::from_code("theme.not_found"), None);
    }

    #[test]
    fn numbers_are_the_standard_five_cancellation_and_server_error() {
        let numbers: std::collections::BTreeSet<i64> = CORE.iter().map(|(code, _)| rpc_number(code)).collect();
        assert_eq!(numbers, [-32800, -32700, -32603, -32602, -32601, -32600, -32000].into_iter().collect());
        assert_eq!(rpc_number("theme.not_found"), SERVER_ERROR);
    }

    #[test]
    fn messages_are_bounded_at_a_character_boundary() {
        let long = "é".repeat(MESSAGE_LIMIT);
        let cut = bounded(&long);
        assert!(cut.len() <= MESSAGE_LIMIT && cut.chars().all(|c| c == 'é'));
        assert_eq!(internal_message("runtime error: boom\nstack traceback:\n\t..."), "boom");
    }
}
