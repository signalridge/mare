//! RPC handlers for `flags.*` (extension CLI flags).

use mare_host::Host;
use mare_protocol as proto;
use serde_json::Value as J;

use super::params_req;

pub(super) fn flags_list(host: &Host) -> J {
    let data = host
        .flags()
        .list()
        .into_iter()
        .map(|info| proto::FlagEntry {
            name: info.spec.name,
            flag_type: info.spec.flag_type.as_str().to_string(),
            description: info.spec.description,
            default: info.spec.default.as_ref().map(|v| v.to_json()),
            value: info.value.as_ref().map(|v| v.to_json()),
            source: info.spec.source,
        })
        .collect();
    serde_json::to_value(proto::FlagsListResult { data }).unwrap_or(J::Null)
}

pub(super) fn flags_get(host: &Host, p: &Option<J>) -> Result<J, proto::RpcError> {
    let args: proto::FlagsGetParams =
        params_req(p).map_err(|e| proto::RpcError::core(proto::ErrorCode::InvalidArguments, e))?;
    let store = host.flags();
    let registered = store.specs().iter().any(|s| s.name == args.name);
    let value = store.get(&args.name).map(|v| v.to_json());
    Ok(serde_json::to_value(proto::FlagsGetResult { name: args.name, registered, value }).unwrap_or(J::Null))
}
