//! Peer sessions: serve bidirectional RPC on a local socket
//! `<state>/peers/<pid>.sock` (mode 0600, any number of clients, each with its own ctx) and keep a
//! registry entry `<state>/peers/<pid>.json` (0600) with name, pid, cwd, socket, a random token,
//! the unified status and `plugins`, what plugins published with `mare.peers.publish`
//! (opaque here: core knows no plugin's fields). Both are removed on `app/shutdown`, which every exit path goes
//! through. Peer connections are least-privilege: they present the token, then may only call the
//! default peer methods unless the session allows more (`peer = { allow = {...} }`, see rpc.rs).
//!
//! Interactive TUI sessions register by default; headless and `--rpc` only with `--peer`.
//! `mare.setup { peer = false }` disables it; `peer = { name = "..." }` names the session.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use mare_binding::peers;
use mare_binding::platform::{Ipc, IpcBackend};
use mare_host::Host;
use serde_json::{Value as J, json};

/// How often the registry file is rewritten at most while the status or published fields change.
const STATUS_WRITE_INTERVAL: Duration = Duration::from_millis(200);

/// `peer` option: disabled by `false`; `{name = ...}` or `true`/absent = enabled with defaults.
fn option(host: &Host) -> Option<J> {
    match host.kernel.config().get("peer").cloned() {
        Some(J::Bool(false)) => None,
        Some(v @ J::Object(_)) => Some(v),
        _ => Some(json!({})),
    }
}

/// "<directory>-<4 random hex digits>", not taken by another
/// registered session: `pid & 0xff` collided once in 256 sessions.
fn default_name(host: &Host) -> String {
    let base = host.cwd.file_name().and_then(|s| s.to_str()).unwrap_or("mare").to_string();
    let taken: Vec<String> =
        peers::list().iter().filter_map(|e| e.get("name").and_then(J::as_str).map(str::to_string)).collect();
    let mut name = String::new();
    for _ in 0..16 {
        name = format!("{base}-{}", &uuid::Uuid::new_v4().simple().to_string()[..4]);
        if !taken.contains(&name) {
            break;
        }
    }
    name
}

/// `plugins`: what plugins published (`mare.peers.publish`), by namespace; no field while empty.
fn set_plugins(entry: &mut J, published: &serde_json::Map<String, J>) {
    let Some(o) = entry.as_object_mut() else { return };
    if published.is_empty() {
        o.remove("plugins");
    } else {
        o.insert("plugins".into(), J::Object(published.clone()));
    }
}

fn current_status(host: &Host) -> J {
    let lua = host.lua();
    let Some(svc) = host.service_table("status") else { return J::Null };
    let Ok(get) = svc.get::<mlua::Function>("get") else { return J::Null };
    // a method of the status service (a mare.Service: called on the service)
    get.call::<mlua::Value>(svc.clone())
        .ok()
        .and_then(|v| mare_shared::lua::lua_to_json(lua, v).ok())
        .unwrap_or(J::Null)
}

fn write_projected_self(proj: Option<&mare_shared::wire_projection::WireProjector>, entry: &J) -> std::io::Result<()> {
    if let Some(proj) = proj.filter(|p| p.is_active()) {
        let mut projected = entry.clone();
        proj.project_json(&mut projected);
        return peers::write_self(&projected);
    }
    peers::write_self(entry)
}

/// Admit a connection only from the same OS user: Unix compares the socket's
/// peer uid (SO_PEERCRED / getpeereid); Windows relies on the pipe's DACL (this user's SID and
/// SYSTEM only, mare-binding `platform/pipe.rs`).
/// `Some(pid)` when admitted, with the connecting process's pid (Windows:
/// `GetNamedPipeClientProcessId`) where the platform reports it (a `from` in `peer.message` that
/// disagrees with it is refused, rpc/inbound.rs); `None` = refused.
#[cfg(unix)]
fn admit_uid(peer_uid: u32, my_uid: u32, peer_pid: Option<i32>) -> Option<Option<u32>> {
    if peer_uid == my_uid {
        Some(peer_pid.and_then(|p| u32::try_from(p).ok()))
    } else {
        tracing::warn!("peer socket: refused uid {peer_uid}");
        None
    }
}

#[cfg(unix)]
fn admit(stream: &tokio::net::UnixStream) -> Option<Option<u32>> {
    match stream.peer_cred() {
        Ok(c) => admit_uid(c.uid(), unsafe { libc::geteuid() }, c.pid()),
        Err(e) => {
            tracing::warn!("peer socket: no peer credentials: {e}");
            None
        }
    }
}

#[cfg(windows)]
fn admit(stream: &mare_binding::platform::pipe::PipeStream) -> Option<Option<u32>> {
    Some(stream.client_pid())
}

/// Start serving and register this session. Returns `Ok(false)` when disabled by config.
pub fn start(host: &Arc<Host>) -> Result<bool> {
    start_inner(host, None)
}

/// Start serving and register this multi-session RPC process peer endpoint.
pub fn start_manager(rpc: Rc<crate::rpc::RpcRuntime>, anchor_host: &Arc<Host>) -> Result<bool> {
    start_inner(anchor_host, Some(rpc))
}

fn start_inner(host: &Arc<Host>, rpc: Option<Rc<crate::rpc::RpcRuntime>>) -> Result<bool> {
    let Some(opt) = option(host) else { return Ok(false) };
    let pid = std::process::id();
    // drop entries of sessions that are gone before adding ours
    let _ = peers::list();
    let sock = peers::socket_path(pid);
    // 0700 real directory owned by us, socket 0600; any failure (chmod included) = don't listen
    peers::ensure_dir()?;
    let listener = <Ipc as IpcBackend>::bind(&sock, 0o600)?;

    let entry = Rc::new(RefCell::new(json!({
        "name": opt.get("name").and_then(J::as_str).map(str::to_string).unwrap_or_else(|| default_name(host)),
        "pid": pid,
        "cwd": host.cwd.to_string_lossy(),
        "socket": sock.to_string_lossy(),
        "mode": host.mode.as_str(),
        "started_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "status": current_status(host),
    })));
    let mut published = peers::published(host.lua());
    let mut projector = mare_shared::wire_projection::hook(host.lua());
    set_plugins(&mut entry.borrow_mut(), &published.borrow_and_update());
    write_projected_self(projector.as_ref(), &entry.borrow())?;

    // accept loop: every connection is a secondary RPC frontend with its own ctx
    let h = host.clone();
    let rpc_accept = rpc.clone();
    let accept = tokio::task::spawn_local(async move {
        loop {
            match <Ipc as IpcBackend>::accept(&listener).await {
                Ok(stream) => {
                    let Some(pid) = admit(&stream) else { continue };
                    let (r, w) = tokio::io::split(stream);
                    let runtime = rpc_accept.clone();
                    let host = h.clone();
                    tokio::task::spawn_local(async move {
                        if let Some(runtime) = runtime {
                            crate::rpc::serve_peer_manager(runtime, r, w, pid).await;
                        } else {
                            crate::rpc::serve_peer(host, r, w, pid).await;
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("peer socket accept: {e}");
                    break;
                }
            }
        }
    });

    // keep the registry's status and published fields current (throttled; the last change is
    // always written); the cursor carries only `status/changed`, so no other event is converted.
    // In multi-runtime RPC mode, follow the active default session host whenever sessions switch.
    let mut tracked_host = host.clone();
    let mut cursor = tracked_host.kernel.open_cursor(Arc::new(|e| (e == "status/changed").then_some(true)));
    let rpc_watch = rpc.clone();
    let mut rpc_wake = rpc_watch.as_ref().map(|r| r.subscribe_wake());
    let e2 = entry.clone();
    let status = tokio::task::spawn_local(async move {
        enum StatusUpdate {
            Closed,
            Ignore,
            Lagged,
            Status(J),
        }
        let status_of = |rec: Option<mare_kernel::CursorRecord>| match rec {
            None => StatusUpdate::Closed,
            Some(mare_kernel::CursorRecord::Event { event, selected }) => {
                if selected && event.name == "status/changed" {
                    StatusUpdate::Status(event.payload)
                } else {
                    StatusUpdate::Ignore
                }
            }
            Some(mare_kernel::CursorRecord::Lagged { .. } | mare_kernel::CursorRecord::Dropped { .. }) => {
                StatusUpdate::Lagged
            }
        };
        let mut cursor_open = true;
        let mut published_open = true;
        loop {
            let mut latest = tokio::select! {
                rec = cursor.recv(), if cursor_open => match status_of(rec) {
                    StatusUpdate::Closed => {
                        if rpc_watch.is_some() {
                            cursor_open = false;
                            continue;
                        }
                        break;
                    }
                    StatusUpdate::Ignore => continue,
                    StatusUpdate::Lagged => Some(current_status(&tracked_host)),
                    StatusUpdate::Status(s) => Some(s),
                },
                r = published.changed(), if published_open => match r {
                    Ok(()) => None,
                    Err(_) => {
                        if rpc_watch.is_some() {
                            published_open = false;
                            continue;
                        }
                        break;
                    }
                },
                r = async {
                    match rpc_wake.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => match r {
                    Ok(()) => {
                        if let Some(next) = rpc_watch.as_ref().and_then(|r| r.active_peer_host())
                            && !Arc::ptr_eq(&tracked_host, &next)
                        {
                            tracked_host = next;
                            cursor = tracked_host.kernel.open_cursor(Arc::new(|e| (e == "status/changed").then_some(true)));
                            cursor_open = true;
                            published = peers::published(tracked_host.lua());
                            published_open = true;
                            projector = mare_shared::wire_projection::hook(tracked_host.lua());
                            e2.borrow_mut()["cwd"] = json!(tracked_host.cwd.to_string_lossy());
                            Some(current_status(&tracked_host))
                        } else {
                            continue;
                        }
                    }
                    Err(_) => break,
                },
            };
            // coalesce a burst: wait out the interval, keep only the newest state
            let deadline = tokio::time::sleep(STATUS_WRITE_INTERVAL);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    rec = cursor.recv(), if cursor_open => match status_of(rec) {
                        StatusUpdate::Closed => {
                            cursor_open = false;
                            if rpc_watch.is_none() { break; }
                        }
                        StatusUpdate::Ignore => {}
                        StatusUpdate::Lagged => latest = Some(current_status(&tracked_host)),
                        StatusUpdate::Status(s) => latest = Some(s),
                    },
                    r = published.changed(), if published_open => if r.is_err() {
                        published_open = false;
                        if rpc_watch.is_none() { break; }
                    },
                    r = async {
                        match rpc_wake.as_mut() {
                            Some(rx) => rx.changed().await,
                            None => std::future::pending().await,
                        }
                    } => match r {
                        Ok(()) => {
                            if let Some(next) = rpc_watch.as_ref().and_then(|r| r.active_peer_host())
                                && !Arc::ptr_eq(&tracked_host, &next)
                            {
                                tracked_host = next;
                                cursor = tracked_host.kernel.open_cursor(Arc::new(|e| (e == "status/changed").then_some(true)));
                                cursor_open = true;
                                published = peers::published(tracked_host.lua());
                                published_open = true;
                                projector = mare_shared::wire_projection::hook(tracked_host.lua());
                                e2.borrow_mut()["cwd"] = json!(tracked_host.cwd.to_string_lossy());
                                latest = Some(current_status(&tracked_host));
                            }
                        }
                        Err(_) => break,
                    },
                }
            }
            if let Some(s) = latest {
                e2.borrow_mut()["status"] = s;
            }
            set_plugins(&mut e2.borrow_mut(), &published.borrow_and_update());
            if peers::self_entry().is_none() {
                break; // unregistered (shutting down)
            }
            if let Err(e) = write_projected_self(projector.as_ref(), &e2.borrow()) {
                tracing::warn!("peer registry: {e}");
            }
        }
    });

    // unregister and stop serving when the plugin tree closes (or when RpcRuntime drops for manager mode)
    let tasks = RefCell::new(Some((accept, status)));
    let cleanup = move || {
        if let Some((a, s)) = tasks.borrow_mut().take() {
            a.abort();
            s.abort();
        }
        peers::unregister_self();
    };
    if let Some(rpc) = rpc {
        rpc.set_peer_cleanup(Box::new(cleanup));
    } else {
        let lua = host.lua();
        let off = lua.create_function(move |_, _: mlua::MultiValue| {
            cleanup();
            Ok(())
        })?;
        let root: mlua::Table = host.kernel.cordis_boot()?.get("root")?;
        lua.load("local root, off = ... root:effect(function() return off end, 'peer registration')")
            .set_name("=peer registration")
            .call::<()>((root, off))?;
    }
    Ok(true)
}

/// `mare peer send [--to …] <text>`: one message over a
/// session's peer socket. Without `--to` it goes to `$MARE_PEER_SOCKET` — the session that
/// started this process — and fails when that's unset instead of guessing another session. The
/// receiver holds it until its user confirms, like every inbound message; message only, no
/// actions. Exit code 0 when the receiver accepted it (held, delivered or handled).
pub async fn send_cli(to: Option<&str>, text: &[String]) -> Result<i32> {
    use tokio::io::AsyncReadExt;
    let socket = match to {
        Some(t) if std::path::Path::new(t).exists() => t.to_string(),
        Some(t) => match peers::list().into_iter().find(|e| e.get("name").and_then(J::as_str) == Some(t)) {
            Some(e) => e.get("socket").and_then(J::as_str).unwrap_or_default().to_string(),
            None => anyhow::bail!("no session named {t:?} (and no such socket path)"),
        },
        None => match std::env::var("MARE_PEER_SOCKET") {
            Ok(s) if !s.is_empty() => s,
            _ => anyhow::bail!(
                "MARE_PEER_SOCKET is not set: run this from a tool or hook inside a mare session with its peer socket on, or pass --to"
            ),
        },
    };
    let mut body = text.join(" ");
    if body.trim().is_empty() {
        tokio::io::stdin().read_to_string(&mut body).await?;
    }
    if body.trim().is_empty() {
        anyhow::bail!("nothing to send");
    }
    let stream = <Ipc as IpcBackend>::connect(&socket).await.with_context(|| format!("connecting to {socket}"))?;
    let status = send_over_stream(stream, &socket, body, std::time::Duration::from_secs(30)).await?;
    println!("{status}");
    Ok(0)
}

async fn send_over_stream<S>(stream: S, socket: &str, body: String, timeout: std::time::Duration) -> Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    let send = |id: u64, method: &str, params: J| {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string() + "\n"
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let timeout_err = || anyhow::anyhow!("no answer from {socket} within {}s", timeout.as_secs());

    // 1. Complete the `initialize` handshake before sending `peer.message`.
    tokio::time::timeout_at(
        deadline,
        w.write_all(
            send(
                1,
                mare_protocol::methods::INITIALIZE,
                json!({ "protocol_version": mare_protocol::PROTOCOL_VERSION, "client": { "name": "mare peer send" } }),
            )
            .as_bytes(),
        ),
    )
    .await
    .map_err(|_| timeout_err())??;
    tokio::time::timeout_at(deadline, w.flush()).await.map_err(|_| timeout_err())??;

    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .map_err(|_| timeout_err())??
            .ok_or_else(|| anyhow::anyhow!("{socket} closed the connection"))?;
        let Ok(v) = serde_json::from_str::<J>(&line) else { continue };
        if v.get("id").and_then(J::as_u64) != Some(1) {
            continue;
        }
        if let Some(e) = v.get("error") {
            anyhow::bail!("{socket}: {}", e.get("message").and_then(J::as_str).unwrap_or("initialize failed"));
        }
        break;
    }

    // 2. Send `peer.message` and wait for its reply under the same overall deadline.
    let from = json!({
        "name": "mare peer send",
        "pid": std::process::id(),
        "cwd": std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()),
        // the session this process was started from (the receiver labels it "a child process of
        // this session" when it matches its own)
        "session_id": std::env::var("MARE_SESSION_ID").ok().filter(|s| !s.is_empty()),
    });
    tokio::time::timeout_at(
        deadline,
        w.write_all(send(2, mare_protocol::methods::PEER_MESSAGE, json!({ "text": body, "from": from })).as_bytes()),
    )
    .await
    .map_err(|_| timeout_err())??;
    tokio::time::timeout_at(deadline, w.flush()).await.map_err(|_| timeout_err())??;

    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .map_err(|_| timeout_err())??
            .ok_or_else(|| anyhow::anyhow!("{socket} closed the connection"))?;
        let Ok(v) = serde_json::from_str::<J>(&line) else { continue };
        if v.get("id").and_then(J::as_u64) != Some(2) {
            continue;
        }
        if let Some(e) = v.get("error") {
            anyhow::bail!("{socket}: {}", e.get("message").and_then(J::as_str).unwrap_or("refused"));
        }
        let status = v.pointer("/result/status").and_then(J::as_str).unwrap_or("sent");
        return Ok(status.to_string());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{admit_uid, send_over_stream};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Peer socket rejects connections whose OS UID differs from this process's euid.
    #[test]
    fn admit_uid_accepts_matching_uid_and_refuses_mismatch() {
        assert_eq!(admit_uid(1000, 1000, Some(4242)), Some(Some(4242)));
        assert_eq!(admit_uid(1000, 1000, None), Some(None));
        assert_eq!(admit_uid(1001, 1000, Some(4242)), None);
        assert_eq!(admit_uid(0, 1000, Some(1)), None);

        let mut entry = serde_json::json!({ "plugins": { "a": 1 } });
        super::set_plugins(&mut entry, &serde_json::Map::new());
        assert!(entry.get("plugins").is_none());
        let mut published = serde_json::Map::new();
        published.insert("x".into(), serde_json::json!(true));
        super::set_plugins(&mut entry, &published);
        assert_eq!(entry["plugins"]["x"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn send_over_stream_enforces_overall_deadline_under_notifications() {
        let (client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server);
            let mut lines = BufReader::new(r).lines();
            let _ = lines.next_line().await;
            let _ = w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n").await;
            let _ = w.flush().await;
            for _ in 0..40 {
                // sleep-ok: periodic notification stream simulating a busy peer session
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                if w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"status/changed\",\"params\":{}}\n").await.is_err() {
                    break;
                }
                let _ = w.flush().await;
            }
        });
        let res = tokio::time::timeout(
            std::time::Duration::from_millis(450),
            send_over_stream(client, "mock", "hello".into(), std::time::Duration::from_millis(120)),
        )
        .await;
        srv.abort();
        let err = res.expect("notifications must not reset the overall peer send deadline").unwrap_err();
        assert!(err.to_string().contains("no answer"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn send_over_stream_waits_for_initialize_before_sending_message() {
        let (client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(server);
            let mut lines = BufReader::new(r).lines();
            let first = lines.next_line().await.unwrap().unwrap();
            assert!(first.contains("initialize"), "expected initialize first: {first}");
            // Reject pipelined peer.message arriving before initialize response is sent.
            let early = tokio::time::timeout(std::time::Duration::from_millis(60), lines.next_line()).await;
            if let Ok(Ok(Some(second))) = early {
                let _ = w
                    .write_all(
                        b"{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32002,\"message\":\"not initialized\"}}\n",
                    )
                    .await;
                let _ = w.flush().await;
                panic!("peer.message was sent before initialize completed: {second}");
            }
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n").await.unwrap();
            w.flush().await.unwrap();
            let second = lines.next_line().await.unwrap().unwrap();
            assert!(second.contains("peer.message"), "expected peer.message second: {second}");
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"status\":\"delivered\"}}\n").await.unwrap();
            w.flush().await.unwrap();
        });
        let status = send_over_stream(client, "mock", "hello".into(), std::time::Duration::from_secs(2)).await.unwrap();
        assert_eq!(status, "delivered");
        srv.await.unwrap();
    }
}
