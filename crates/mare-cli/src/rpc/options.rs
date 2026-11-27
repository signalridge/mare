//! `options.get` / `options.set` / `config.describe`: RPC transport adapter over
//! [`mare_host::config::options`], adding connection authorization and `base_opts` /
//! `runtime_defaults` sync while preserving exact wire shapes.

use std::collections::BTreeMap;

use mare_host::config::options::{self as host_options, SetError};

use super::*;

pub(super) async fn options_get(host: &Host, keys: Option<Vec<String>>, describe: bool) -> Result<J> {
    let raw = host_options::get(&host.kernel, keys, describe).await?;
    if !describe {
        return Ok(raw);
    }
    let descriptors: BTreeMap<String, proto::OptionDescriptor> = serde_json::from_value(raw)?;
    Ok(serde_json::to_value(descriptors)?)
}

pub(super) async fn config_describe(host: &Host, keys: Option<Vec<String>>) -> Result<J> {
    let raw = host_options::get(&host.kernel, keys, true).await?;
    let options: BTreeMap<String, proto::OptionDescriptor> = serde_json::from_value(raw)?;
    let warnings = host_options::redacted_warnings(&host.kernel).await?;
    Ok(serde_json::to_value(proto::ConfigDescribeResult { options, warnings })?)
}

/// Set an option backed by a service operation: remote-exported methods go through their contract
/// using their first ordered parameter name, any other service through its Lua operation.
async fn service_set(
    conn: &Rc<Conn>,
    session_id: &str,
    host: &Host,
    service: &str,
    method: &str,
    value: J,
) -> std::result::Result<(), proto::RpcError> {
    if let Some(spec) = host.kernel.service_method(service, method)
        && let Some(key) = spec.order.first()
    {
        let mut args = json!({ key: value });
        if spec.scope == mare_kernel::contracts::Scope::Session {
            args["session_id"] = J::String(session_id.to_owned());
        }
        return super::services::call(conn, service, method, args).await.map(|_| ());
    }
    call_service(host, service, method, vec![value]).await.map(|_| ()).map_err(super::failure)
}

pub(super) async fn options_set(
    conn: &Rc<Conn>,
    rt: &Rc<SessionRuntime>,
    values: serde_json::Map<String, J>,
    sync_base: bool,
) -> std::result::Result<(), proto::RpcError> {
    let refuse = |code: proto::ErrorCode, message: String| proto::RpcError::core(code, message);
    let host = &rt.host;
    // An option backed by a contract operation is that operation: the same authorization.
    for &(key, service, operation) in host_options::service_backed_options() {
        if values.contains_key(key) && !super::services::permits_on(conn, host, service, operation) {
            return Err(refuse(
                proto::ErrorCode::InvalidArguments,
                format!("option `{key}` needs `{service}.{operation}` on this connection"),
            ));
        }
    }

    let _outcome = host_options::set_with_service_caller(
        &host.kernel,
        &values,
        false,
        |service, method, value| service_set(conn, &rt.session_id, host, service, method, value),
        |e: &proto::RpcError| e.message().to_string(),
    )
    .await
    .map_err(|err| match err {
        SetError::InvalidArguments(msg) => refuse(proto::ErrorCode::InvalidArguments, msg),
        SetError::Internal(err) => super::failure(err),
        SetError::Service(err) => err,
    })?;

    if sync_base {
        let mut defaults = conn.rpc.runtime_defaults.borrow_mut();
        for (k, v) in values {
            match k.as_str() {
                "model" => defaults.model = (!v.is_null()).then_some(v),
                "effort" => defaults.effort = (!v.is_null()).then_some(v),
                _ => {
                    let prefix = format!("{k}.");
                    defaults.options.retain(|existing, _| !existing.starts_with(&prefix));
                    if v.is_null() {
                        defaults.options.remove(&k);
                    } else {
                        defaults.options.insert(k, v);
                    }
                }
            }
        }
    }
    Ok(())
}
