//! Shared in-process JSON-RPC test harness for `crates/mare-cli/src/rpc/*`.
#![allow(dead_code)]

use std::future::pending;

use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::*;
pub(in crate::rpc) use crate::HOST_TEST_LOCK as HOST_LOCK;

pub(in crate::rpc) type RpcReader = BufReader<ReadHalf<DuplexStream>>;
pub(in crate::rpc) type RpcWriter = WriteHalf<DuplexStream>;

/// Write a single JSON-RPC frame line and flush the duplex writer.
pub(in crate::rpc) async fn rpc_send(writer: &mut (impl AsyncWrite + Unpin), value: J) {
    writer.write_all(format!("{value}\n").as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}

/// Read the next JSON-RPC frame line (20s timeout).
pub(in crate::rpc) async fn rpc_read(reader: &mut (impl AsyncBufReadExt + Unpin)) -> J {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), reader.read_line(&mut line)).await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Read the next JSON-RPC frame line or return `None` on EOF (20s timeout).
pub(in crate::rpc) async fn read_until_eof(reader: &mut (impl AsyncBufReadExt + Unpin)) -> Option<J> {
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(20), reader.read_line(&mut line)).await.unwrap().unwrap();
    (n > 0).then(|| serde_json::from_str(&line).unwrap())
}

/// Build an in-process test [`Host`] with default faux provider and disabled permissions plugin.
pub(in crate::rpc) async fn test_host(cwd: &std::path::Path) -> Arc<Host> {
    Host::new(mare_host::HostOptions {
        cwd: cwd.to_owned(),
        mode: mare_plugin::Mode::Print,
        config: json!({"model": "faux/echo", "plugins": {"permissions": {"enabled": false}}}),
        load_init: false,
        project_trusted: false,
        args: json!({"no_session": true}),
        no_user_plugins: true,
    })
    .await
    .unwrap()
}

/// In-process JSON-RPC client pair connected over `tokio::io::duplex` to [`serve`] or [`serve_peer`].
pub(in crate::rpc) struct TestRpc {
    pub(in crate::rpc) host: Arc<Host>,
    pub(in crate::rpc) cr: RpcReader,
    pub(in crate::rpc) cw: RpcWriter,
    pub(in crate::rpc) server: JoinHandle<()>,
    pub(in crate::rpc) buf: Vec<J>,
    next_id: u64,
}

impl TestRpc {
    /// Connect as primary owner (`serve(..., primary = true)`).
    pub(in crate::rpc) fn connect(host: Arc<Host>) -> Self {
        Self::connect_with_primary(host, true)
    }

    /// Connect via `serve(..., primary, pending())`.
    pub(in crate::rpc) fn connect_with_primary(host: Arc<Host>, primary: bool) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server);
        let server = tokio::task::spawn_local(serve(host.clone(), sr, sw, primary, pending()));
        let (cr, cw) = tokio::io::split(client);
        Self { host, cr: BufReader::new(cr), cw, server, buf: Vec::new(), next_id: 0 }
    }

    /// Connect via `serve_peer(...)`.
    pub(in crate::rpc) fn connect_peer(host: Arc<Host>, peer_pid: Option<u32>) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server);
        let server = tokio::task::spawn_local(serve_peer(host.clone(), sr, sw, peer_pid));
        let (cr, cw) = tokio::io::split(client);
        Self { host, cr: BufReader::new(cr), cw, server, buf: Vec::new(), next_id: 0 }
    }

    /// Connect with an explicit shutdown trigger channel for graceful-shutdown tests.
    pub(in crate::rpc) fn connect_with_shutdown(host: Arc<Host>, primary: bool) -> (Self, oneshot::Sender<()>) {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (sr, sw) = tokio::io::split(server);
        let (tx, rx) = oneshot::channel::<()>();
        let server = tokio::task::spawn_local(serve(host.clone(), sr, sw, primary, async move {
            let _ = rx.await;
        }));
        let (cr, cw) = tokio::io::split(client);
        (Self { host, cr: BufReader::new(cr), cw, server, buf: Vec::new(), next_id: 0 }, tx)
    }

    pub(in crate::rpc) async fn send(&mut self, value: J) {
        rpc_send(&mut self.cw, value).await;
    }

    pub(in crate::rpc) async fn read(&mut self) -> J {
        rpc_read(&mut self.cr).await
    }

    /// Send request with explicit numeric `id` and read until its reply (dropping unrelated frames).
    pub(in crate::rpc) async fn call_id(&mut self, id: u64, method: &str, params: J) -> J {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = self.read().await;
            if msg.get("id").and_then(J::as_u64) == Some(id) {
                return msg;
            }
        }
    }

    /// Send request with auto-incremented numeric `id` and return its reply.
    pub(in crate::rpc) async fn call(&mut self, method: &str, params: J) -> J {
        self.next_id += 1;
        let id = self.next_id;
        self.call_id(id, method, params).await
    }

    /// Send request with explicit `id` and buffer any intervening frames into `self.buf`.
    pub(in crate::rpc) async fn call_buffered_id(&mut self, id: u64, method: &str, params: J) -> J {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = self.read().await;
            if msg.get("id").and_then(J::as_u64) == Some(id) {
                return msg;
            }
            self.buf.push(msg);
        }
    }

    /// Send request with explicit string `id` and read until its reply (failing on EOF).
    pub(in crate::rpc) async fn request_str(&mut self, id: &str, method: &str, params: J) -> J {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = read_until_eof(&mut self.cr).await.expect("EOF before the reply");
            if msg["id"] == id {
                return msg;
            }
        }
    }

    /// Pop earliest buffered message or read next frame from the stream.
    pub(in crate::rpc) async fn next_msg(&mut self) -> J {
        if !self.buf.is_empty() {
            return self.buf.remove(0);
        }
        self.read().await
    }

    /// Consume buffered or subsequent frames until notification `method` arrives; return its `params`.
    pub(in crate::rpc) async fn next_notification(&mut self, method: &str) -> J {
        if let Some(idx) = self.buf.iter().position(|m| m.get("method").and_then(J::as_str) == Some(method)) {
            return self.buf.remove(idx)["params"].clone();
        }
        loop {
            let msg = self.read().await;
            if msg.get("method").and_then(J::as_str) == Some(method) {
                return msg["params"].clone();
            }
        }
    }

    /// Drop client halves to trigger server disconnect cleanup, returning the server handle.
    pub(in crate::rpc) fn disconnect(self) -> JoinHandle<()> {
        drop(self.cw);
        drop(self.cr);
        self.server
    }

    /// Disconnect client halves and await server task completion (with 20s timeout).
    pub(in crate::rpc) async fn close(self) {
        let server = self.disconnect();
        tokio::time::timeout(Duration::from_secs(20), server).await.unwrap().unwrap();
    }
}
