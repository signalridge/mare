//! Isolated namespaces over the existing private atomic writer. No Lua callbacks enter IO.
//! Storage-domain isolation / schedule revision CAS inform the contract. Legacy state.json
//! never participates in a namespace transaction.
use std::path::{Path, PathBuf};
use std::time::Duration;

use mare_protocol::{
    StateCompareExchangeRequest, StateCompareExchangeResult, StateErrorCode, StateFailure, StateSnapshot,
    StateSnapshotRequest, StateSnapshotResult,
};
use mare_shared::lua::{json_to_lua_lossless, strict_lua_to_json};
use mlua::{Lua, Table, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

pub const ENVELOPE_VERSION: u32 = 1;
const MAX_REVISION: u64 = 9_007_199_254_740_991;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    namespace: String,
    revision: u64,
    exists: bool,
    value: Json,
}

fn failure(code: StateErrorCode, message: impl ToString) -> StateFailure {
    StateFailure { code, message: message.to_string() }
}

fn path(namespace: &str) -> Result<PathBuf, StateFailure> {
    if namespace.is_empty()
        || namespace.len() > 128
        || !namespace.as_bytes()[0].is_ascii_alphanumeric()
        || !namespace.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    {
        return Err(failure(
            StateErrorCode::InvalidNamespace,
            "namespace must be 1..128 ASCII letters/digits/._-, starting with a letter or digit",
        ));
    }
    // Fixed-length lowercase names avoid device names, case-folding aliases and long
    // filename components on Windows. The envelope still verifies the exact identifier.
    let key = Sha256::digest(namespace.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    Ok(mare_shared::paths::state_dir().join("domains").join(format!("ns-{key}.json")))
}

fn absent(namespace: &str) -> StateSnapshot {
    StateSnapshot { namespace: namespace.into(), revision: 0, exists: false, value: Json::Null }
}

fn read_at(path: &Path, namespace: &str) -> Result<StateSnapshot, StateFailure> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(absent(namespace)),
        Err(e) => return Err(failure(StateErrorCode::Io, e)),
    };
    let value: Json = serde_json::from_slice(&bytes).map_err(|e| failure(StateErrorCode::Corrupt, e))?;
    let version = value
        .get("version")
        .and_then(Json::as_u64)
        .ok_or_else(|| failure(StateErrorCode::Corrupt, "namespace envelope has no integer version"))?;
    if version != ENVELOPE_VERSION as u64 {
        return Err(failure(StateErrorCode::UnsupportedVersion, format!("unsupported namespace version {version}")));
    }
    let entry: Envelope = serde_json::from_value(value).map_err(|e| failure(StateErrorCode::Corrupt, e))?;
    if entry.namespace != namespace
        || entry.revision == 0
        || entry.revision > MAX_REVISION
        || (!entry.exists && !entry.value.is_null())
    {
        return Err(failure(StateErrorCode::Corrupt, "namespace identity, revision or tombstone is invalid"));
    }
    Ok(StateSnapshot { namespace: entry.namespace, revision: entry.revision, exists: entry.exists, value: entry.value })
}

fn cancelled(token: &CancellationToken) -> Result<(), StateFailure> {
    if token.is_cancelled() {
        Err(failure(StateErrorCode::Cancelled, "namespace operation cancelled before publication"))
    } else {
        Ok(())
    }
}

fn acquire(path: &Path, token: &CancellationToken) -> Result<mare_shared::private::LockGuard, StateFailure> {
    cancelled(token)?;
    let lock = path.with_extension("lock");
    mare_shared::private::lock_file_blocking(
        &lock,
        Some(Duration::from_secs(5)),
        Duration::from_millis(10),
        true,
        Some(token),
    )
    .map_err(|e| match e {
        mare_shared::private::LockError::Cancelled => {
            failure(StateErrorCode::Cancelled, "namespace operation cancelled before publication")
        }
        mare_shared::private::LockError::TimedOut => {
            failure(StateErrorCode::LockTimeout, "namespace lock was not available within five seconds")
        }
        mare_shared::private::LockError::Io(io) => failure(StateErrorCode::Io, io),
    })
}

fn exchange_at(
    path: &Path,
    request: StateCompareExchangeRequest,
    token: CancellationToken,
) -> StateCompareExchangeResult {
    let result = (|| {
        let _lock = acquire(path, &token)?;
        let current = read_at(path, &request.namespace)?;
        cancelled(&token)?;
        if current.revision != request.expected_revision {
            return Ok(StateCompareExchangeResult::Conflict { snapshot: current });
        }
        if current.revision == MAX_REVISION {
            return Err(failure(StateErrorCode::RevisionExhausted, "namespace revision cannot advance safely"));
        }
        let next = Envelope {
            version: ENVELOPE_VERSION,
            namespace: request.namespace,
            revision: current.revision + 1,
            exists: !request.delete,
            value: if request.delete { Json::Null } else { request.value },
        };
        let bytes = serde_json::to_vec(&next).map_err(|e| failure(StateErrorCode::InvalidValue, e))?;
        let mut publishing = false;
        let written =
            mare_shared::paths::write_atomic_checked(path, &bytes, mare_shared::paths::WriteMode::Private, || {
                if token.is_cancelled() {
                    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled before publication"));
                }
                // Irreversible publication phase; visibility linearizes at rename. Cancellation
                // after this final admission check cannot promise rollback.
                publishing = true;
                Ok(())
            });
        if let Err(error) = written {
            let code = if publishing {
                StateErrorCode::UnknownState
            } else if token.is_cancelled() {
                StateErrorCode::Cancelled
            } else {
                StateErrorCode::Io
            };
            return Err(failure(code, error));
        }
        Ok(StateCompareExchangeResult::Ok {
            snapshot: StateSnapshot {
                namespace: next.namespace,
                revision: next.revision,
                exists: next.exists,
                value: next.value,
            },
        })
    })();
    result.unwrap_or_else(|error| StateCompareExchangeResult::Rejected { error })
}

fn representable(lua: &Lua, value: &Json) -> Result<(), StateFailure> {
    // Match Lua's strict JSON depth, value budget and safe-number boundary on every surface.
    let detached = json_to_lua_lossless(lua, value).map_err(|e| failure(StateErrorCode::InvalidValue, e))?;
    strict_lua_to_json(lua, detached).map_err(|e| failure(StateErrorCode::InvalidValue, e))?;
    Ok(())
}

pub async fn snapshot(lua: &Lua, request: Json) -> StateSnapshotResult {
    let checked = (|| {
        let request: StateSnapshotRequest =
            serde_json::from_value(request).map_err(|e| failure(StateErrorCode::InvalidRequest, e))?;
        let path = path(&request.namespace)?;
        crate::exec_ctx::check(lua, "read", &path.to_string_lossy()).map_err(|e| failure(StateErrorCode::Denied, e))?;
        Ok((request.namespace, path))
    })();
    let (namespace, path) = match checked {
        Ok(value) => value,
        Err(error) => return StateSnapshotResult::Rejected { error },
    };
    match tokio::task::spawn_blocking(move || read_at(&path, &namespace)).await {
        Ok(Ok(snapshot)) => match representable(lua, &snapshot.value) {
            Ok(()) => StateSnapshotResult::Ok { snapshot },
            Err(error) => StateSnapshotResult::Rejected { error },
        },
        Ok(Err(error)) => StateSnapshotResult::Rejected { error },
        Err(error) => StateSnapshotResult::Rejected { error: failure(StateErrorCode::Io, error) },
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn execute(
    path: PathBuf,
    request: StateCompareExchangeRequest,
    token: CancellationToken,
) -> StateCompareExchangeResult {
    let scope = CancelOnDrop(token.clone());
    let result = tokio::task::spawn_blocking(move || exchange_at(&path, request, token)).await;
    drop(scope);
    result.unwrap_or_else(|error| StateCompareExchangeResult::Rejected {
        error: failure(StateErrorCode::UnknownState, error),
    })
}

pub async fn compare_exchange(lua: &Lua, request: Json, options: Option<Table>) -> StateCompareExchangeResult {
    let checked = (|| {
        mare_shared::exec_ctx::check_mutation(lua, "mare.state.compare_exchange")
            .map_err(|e| failure(StateErrorCode::ReadOnly, e))?;
        crate::exec_ctx::refuse_deny_read(lua, "mare.state.compare_exchange")
            .map_err(|e| failure(StateErrorCode::Denied, e))?;
        let request: StateCompareExchangeRequest =
            serde_json::from_value(request).map_err(|e| failure(StateErrorCode::InvalidRequest, e))?;
        if request.expected_revision > MAX_REVISION || (request.delete && !request.value.is_null()) {
            return Err(failure(
                StateErrorCode::InvalidRequest,
                "expected_revision must be safe; delete requires a null value",
            ));
        }
        let path = path(&request.namespace)?;
        representable(lua, &request.value)?;
        let cancel = if let Some(options) = options {
            if options.metatable().is_some() {
                return Err(failure(StateErrorCode::InvalidRequest, "operation options must be a plain table"));
            }
            for pair in options.clone().pairs::<Value, Value>() {
                let (key, _) = pair.map_err(|e| failure(StateErrorCode::InvalidRequest, e))?;
                if !matches!(key, Value::String(s) if s.as_bytes().as_ref() == b"cancel") {
                    return Err(failure(StateErrorCode::InvalidRequest, "only cancel is a supported operation option"));
                }
            }
            crate::task::checked_token(&options).map_err(|e| failure(StateErrorCode::InvalidRequest, e))?
        } else {
            None
        };
        Ok((request, path, cancel.map_or_else(CancellationToken::new, |token| token.child_token())))
    })();
    let (request, path, token) = match checked {
        Ok(value) => value,
        Err(error) => return StateCompareExchangeResult::Rejected { error },
    };
    match execute(path, request, token).await {
        StateCompareExchangeResult::Conflict { snapshot } => match representable(lua, &snapshot.value) {
            Ok(()) => StateCompareExchangeResult::Conflict { snapshot },
            Err(error) => StateCompareExchangeResult::Rejected { error },
        },
        result => result,
    }
}

pub fn install(lua: &Lua, state: &Table) -> mlua::Result<()> {
    state.set(
        "snapshot",
        lua.create_async_function(|lua, request: Value| async move {
            let result = match strict_lua_to_json(&lua, request) {
                Ok(request) => snapshot(&lua, request).await,
                Err(error) => StateSnapshotResult::Rejected { error: failure(StateErrorCode::InvalidRequest, error) },
            };
            json_to_lua_lossless(&lua, &serde_json::to_value(result).map_err(mlua::Error::external)?)
        })?,
    )?;
    state.set(
        "compare_exchange",
        lua.create_async_function(|lua, (request, options): (Value, Option<Table>)| async move {
            let result = match strict_lua_to_json(&lua, request) {
                Ok(request) => compare_exchange(&lua, request, options).await,
                Err(error) => {
                    StateCompareExchangeResult::Rejected { error: failure(StateErrorCode::InvalidRequest, error) }
                }
            };
            json_to_lua_lossless(&lua, &serde_json::to_value(result).map_err(mlua::Error::external)?)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
