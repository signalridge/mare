//! Real process regressions for acp.
use super::*;

/// `mare acp` with a scripted faux model in a temp MARE_HOME.
fn acp(script: Value) -> (RpcClient, tempfile::TempDir) {
    acp_with_args(script, &[], &[])
}

/// Spawn `mare acp` against an existing `MARE_HOME` directory.
pub(super) fn acp_in_home(
    home: &std::path::Path,
    script: Value,
    extra_args: &[&str],
    envs: &[(&str, &str)],
) -> RpcClient {
    let path = home.join("faux.json");
    std::fs::write(&path, script.to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo"])
        .args(extra_args)
        .arg("acp")
        .env("MARE_HOME", home)
        .env("MARE_FAUX_SCRIPT", &path)
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    RpcClient::spawn_cmd(&mut cmd)
}

/// Read JSON-RPC messages until response `id` arrives, collecting matching `session/update` payloads.
pub(super) fn read_until(rpc: &mut RpcClient, id: u64, session_id: Option<&str>) -> (Value, Vec<Value>) {
    let mut updates = Vec::new();
    let resp = loop {
        let m = rpc.read();
        if m["method"] == "session/update" {
            if session_id.is_none_or(|sid| m["params"]["sessionId"] == sid) {
                updates.push(m["params"]["update"].clone());
            }
        } else if m.get("method").is_none() && m["id"] == id {
            break m;
        }
    };
    (resp, updates)
}

fn acp_with_args(script: Value, extra_args: &[&str], envs: &[(&str, &str)]) -> (RpcClient, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let rpc = acp_in_home(home.path(), script, extra_args, envs);
    (rpc, home)
}

mod additional_dirs;
mod cancel_and_validation;
mod concurrent_sessions;
mod core_and_media;
mod lifecycle;
mod permissions_and_config;
