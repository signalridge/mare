//! JSON-RPC frame envelope checks and reload-barrier exemption rules.

use super::{Conn, J, RequestId, methods, proto};
use std::rc::Rc;

pub(super) struct Members(Vec<(String, J)>);

impl<'de> serde::Deserialize<'de> for Members {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Members;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON-RPC message object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                let mut members = Vec::new();
                while let Some(entry) = map.next_entry::<String, J>()? {
                    members.push(entry);
                }
                Ok(Members(members))
            }
        }
        d.deserialize_map(V)
    }
}

impl Members {
    /// The value of `key` when it occurs exactly once.
    pub(super) fn single(&self, key: &str) -> Option<&J> {
        let mut found = self.0.iter().filter(|(k, _)| k == key);
        let first = found.next().map(|(_, v)| v);
        if found.next().is_some() { None } else { first }
    }

    /// A request's id, when readable: it names a method and has exactly one valid `id`.
    pub(super) fn request_id(&self) -> Option<RequestId> {
        self.0
            .iter()
            .any(|(k, _)| k == "method")
            .then(|| self.single("id"))
            .flatten()
            .and_then(|id| serde_json::from_value(id.clone()).ok())
    }
}

/// Requests that never wait for a reload in progress: the connection's lifecycle, cancelling work
/// (a turn must stay cancellable while a reload runs: it may be what the turn's tools wait on),
/// reading the host's state, and the model requests, which a reload in progress refuses instead
/// . Replies to the server's
/// own callbacks are not requests at all.
pub(super) const RELOAD_EXEMPT: &[&str] =
    &[methods::INITIALIZE, methods::SHUTDOWN, "agent.prompt", "agent.cancel", "agent.get", "status.get"];

/// Wait until no reload is in progress before `method` runs (not for [`RELOAD_EXEMPT`]); a
/// cancelled request stops waiting with a cancelled error.
pub(super) async fn reload_settled(
    conn: &Rc<Conn>,
    method: &str,
    params: &Option<J>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), proto::RpcError> {
    if RELOAD_EXEMPT.contains(&method) {
        return Ok(());
    }
    let host = super::request_target_host(conn, method, params);
    tokio::select! {
        _ = host.kernel.reload_barrier() => Ok(()),
        _ = cancel.cancelled() => Err(proto::RpcError::coded("cancelled", "request cancelled")),
    }
}
