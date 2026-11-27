use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::*;
pub(super) use crate::rpc::rpc_harness::{HOST_LOCK, rpc_read, rpc_send};

async fn call_rpc(
    cw: &mut (impl AsyncWrite + Unpin),
    cr: &mut (impl AsyncBufReadExt + Unpin),
    id: u64,
    method: &str,
    params: J,
) -> J {
    rpc_send(cw, json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
    loop {
        let msg = rpc_read(cr).await;
        if msg.get("id").and_then(J::as_u64) == Some(id) {
            return msg;
        }
    }
}

async fn next_notification(cr: &mut (impl AsyncBufReadExt + Unpin), method: &str) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg.get("method").and_then(J::as_str) == Some(method) {
            return msg["params"].clone();
        }
    }
}

async fn call_rpc_with_buf(
    cw: &mut (impl AsyncWrite + Unpin),
    cr: &mut (impl AsyncBufReadExt + Unpin),
    id: u64,
    method: &str,
    params: J,
    buf: &mut Vec<J>,
) -> J {
    rpc_send(cw, json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
    loop {
        let msg = rpc_read(cr).await;
        if msg.get("id").and_then(J::as_u64) == Some(id) {
            return msg;
        }
        buf.push(msg);
    }
}

async fn next_notification_with_buf(cr: &mut (impl AsyncBufReadExt + Unpin), method: &str, buf: &mut Vec<J>) -> J {
    if let Some(idx) = buf.iter().position(|m| m.get("method").and_then(J::as_str) == Some(method)) {
        return buf.remove(idx)["params"].clone();
    }
    next_notification(cr, method).await
}

fn set_lua_sandbox(host: &Host, spec: &str) {
    host.lua().load(format!(r#"mare.proc.sandbox.set_current(mare.proc.sandbox.policy({spec}))"#)).exec().unwrap();
}

async fn make_rpc_host(
    cwd: &std::path::Path,
    config: J,
    project_trusted: bool,
    no_user_plugins: bool,
) -> std::sync::Arc<Host> {
    Host::new(mare_host::HostOptions {
        cwd: cwd.to_owned(),
        mode: mare_plugin::Mode::Print,
        config,
        load_init: false,
        project_trusted,
        args: json!({"no_session": true}),
        no_user_plugins,
    })
    .await
    .unwrap()
}

type RpcReader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type RpcWriter = tokio::io::WriteHalf<tokio::io::DuplexStream>;

async fn open_rpc_session(
    host: &std::sync::Arc<Host>,
    primary: bool,
    caps: J,
) -> (RpcWriter, RpcReader, tokio::task::JoinHandle<()>, J) {
    let (client, server) = tokio::io::duplex(65536);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::task::spawn_local(serve(host.clone(), sr, sw, primary, std::future::pending()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut cr = BufReader::new(cr);
    let init =
        call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": caps})).await;
    (cw, cr, task, init)
}

async fn close_rpc_session(cw: RpcWriter, cr: RpcReader, task: tokio::task::JoinHandle<()>) {
    drop(cr);
    drop(cw);
    task.await.unwrap();
}

#[cfg(unix)]
fn pid_terminated(pid: u32) -> bool {
    if !std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().unwrap().success() {
        return true;
    }
    // Killed but not yet reaped by PID 1 in containers: treat zombie state as terminated.
    #[cfg(target_os = "linux")]
    if std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|s| s.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')))
    {
        return true;
    }
    false
}

#[cfg(unix)]
async fn assert_pid_terminated(pid: u32, msg: &str) {
    let mut dead = false;
    for _ in 0..50 {
        if pid_terminated(pid) {
            dead = true;
            break;
        }
        // sleep-ok: poll process exit status in assert_pid_terminated
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(dead, "{msg}: pid {pid}");
}

pub(super) async fn proc_eval<R: mlua::FromLuaMulti>(host: &Host, code: &str) -> R {
    if let Some(rpc) = host.kernel.service("rpc") {
        let _ = host.lua().globals().set("RPC", rpc);
    }
    let _ = host.lua().globals().set("PROC", host.service_table("proc"));
    host.lua().load(code).call_async(()).await.unwrap()
}

pub(super) async fn rpc_disconnect(host: &Host, id: &str) {
    host.lua().globals().set("DC_ID", id).unwrap();
    let (): () = proc_eval(
        host,
        r#"
        local r = RPC:disconnect(DC_ID)
        local ok, cordis = pcall(require, "cordis")
        if ok and cordis.future.isThenable(r) then cordis.await(r) end
        "#,
    )
    .await;
}

#[path = "fs_proc_tests/fs_and_watch.rs"]
mod fs_and_watch;
#[path = "fs_proc_tests/fs_confined.rs"]
mod fs_confined;
#[path = "fs_proc_tests/fs_contract.rs"]
mod fs_contract;
#[path = "fs_proc_tests/primary_image.rs"]
mod primary_image;
#[path = "fs_proc_tests/process_pty_and_net.rs"]
mod process_pty_and_net;
#[path = "fs_proc_tests/process_reap.rs"]
mod process_reap;
#[path = "fs_proc_tests/prompt_mentions.rs"]
mod prompt_mentions;
