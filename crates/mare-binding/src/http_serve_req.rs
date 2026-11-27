//! Request and response helpers for `mare.http.serve`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bytes::Bytes;
use http::{HeaderName, HeaderValue, StatusCode};
use http_body_util::BodyExt;
use hyper::body::{Frame, Incoming};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use mlua::{Lua, MultiValue, Table, UserData, UserDataFields, UserDataMethods, Value};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_util::sync::CancellationToken;

use super::{
    ActiveTasks, CancelOnDrop, STREAM_QUEUE_CAPACITY, ServeBody, ServerWs, UPGRADE_FAILED, UPGRADE_IN_PROGRESS,
    UPGRADE_SUCCEEDED, empty_body, full_body,
};
use crate::task::Token;
use mare_shared::lua::{nil_err, rt_err};

pub(super) struct UpgradeGuard {
    pub(super) phase: Arc<AtomicU8>,
    pub(super) conn_cancel: CancellationToken,
    pub(super) armed: bool,
}

impl UpgradeGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UpgradeGuard {
    fn drop(&mut self) {
        if self.armed {
            self.phase.store(UPGRADE_FAILED, Ordering::Release);
            self.conn_cancel.cancel();
        }
    }
}

pub(super) struct UpgradeState {
    pub(super) on_upgrade: Option<OnUpgrade>,
    pub(super) ws_key: Option<String>,
    pub(super) ws_config: WebSocketConfig,
    pub(super) resp_tx: Option<oneshot::Sender<hyper::Response<ServeBody>>>,
    pub(super) conn_permit: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    pub(super) upgrade_phase: Arc<AtomicU8>,
    pub(super) conn_cancel: CancellationToken,
    pub(super) ws_drained: Arc<(AtomicBool, tokio::sync::Notify)>,
    pub(super) peer_eof: super::ws::PeerEof,
    pub(super) ws_stall_limit: std::time::Duration,
    #[cfg(test)]
    pub(super) test_hooks: Option<Arc<super::ws::ServeTestHooks>>,
}

pub(super) struct ReqBodyState {
    pub(super) incoming: Option<Incoming>,
    pub(super) max_body_bytes: usize,
    pub(super) read_bytes: usize,
}

/// Incoming HTTP request passed to `mare.http.serve` handler.
pub struct HttpRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) raw_query: Option<String>,
    pub(super) query: Vec<(String, String)>,
    pub(super) headers: Vec<(String, String)>,
    pub(super) cancel: CancellationToken,
    pub(super) tasks: ActiveTasks,
    pub(super) body: Arc<AsyncMutex<ReqBodyState>>,
    pub(super) upgrade: Arc<AsyncMutex<UpgradeState>>,
}

impl HttpRequest {
    async fn read_all_body(&self) -> Result<Vec<u8>, String> {
        let mut st = self.body.lock().await;
        let Some(mut inc) = st.incoming.take() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        while let Some(frame_res) = inc.frame().await {
            let frame = frame_res.map_err(|e| format!("request body read: {e}"))?;
            if let Ok(data) = frame.into_data() {
                st.read_bytes = st.read_bytes.saturating_add(data.len());
                if st.read_bytes > st.max_body_bytes {
                    return Err(format!("request body exceeds {} bytes", st.max_body_bytes));
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(out)
    }
}

impl UserData for HttpRequest {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_field_method_get("method", |_, this| Ok(this.method.clone()));
        f.add_field_method_get("path", |_, this| Ok(this.path.clone()));
        f.add_field_method_get("raw_query", |_, this| Ok(this.raw_query.clone()));
        f.add_field_method_get("query", |lua, this| {
            let t = lua.create_table()?;
            for (k, v) in &this.query {
                t.set(k.as_str(), v.as_str())?;
            }
            Ok(t)
        });
        f.add_field_method_get("headers", |lua, this| {
            let t = lua.create_table()?;
            for (k, v) in &this.headers {
                t.set(k.as_str(), v.as_str())?;
            }
            Ok(t)
        });
        f.add_field_method_get("cancel", |_, this| Ok(Token(this.cancel.clone())));
    }

    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("read", |lua, this, ()| async move {
            let mut st = this.body.lock().await;
            let Some(inc) = st.incoming.as_mut() else {
                return Ok(MultiValue::from_vec(vec![Value::Nil]));
            };
            loop {
                match inc.frame().await {
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            if data.is_empty() {
                                continue;
                            }
                            st.read_bytes = st.read_bytes.saturating_add(data.len());
                            if st.read_bytes > st.max_body_bytes {
                                st.incoming = None;
                                return nil_err(&lua, format!("request body exceeds {} bytes", st.max_body_bytes));
                            }
                            let s = lua.create_string(&data)?;
                            return Ok(MultiValue::from_vec(vec![Value::String(s)]));
                        }
                    }
                    Some(Err(e)) => {
                        st.incoming = None;
                        return nil_err(&lua, format!("request body read: {e}"));
                    }
                    None => {
                        st.incoming = None;
                        return Ok(MultiValue::from_vec(vec![Value::Nil]));
                    }
                }
            }
        });

        m.add_async_method("body", |lua, this, ()| async move {
            match this.read_all_body().await {
                Ok(bytes) => {
                    let s = lua.create_string(&bytes)?;
                    Ok(MultiValue::from_vec(vec![Value::String(s)]))
                }
                Err(e) => nil_err(&lua, e),
            }
        });

        m.add_async_method("text", |lua, this, ()| async move {
            match this.read_all_body().await {
                Ok(bytes) => {
                    let s = String::from_utf8(bytes).map_err(|e| rt_err(format!("invalid UTF-8 body: {e}")))?;
                    Ok(MultiValue::from_vec(vec![Value::String(lua.create_string(&s)?)]))
                }
                Err(e) => nil_err(&lua, e),
            }
        });

        m.add_async_method("json", |lua, this, ()| async move {
            match this.read_all_body().await {
                Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(v) => Ok(MultiValue::from_vec(vec![mare_shared::lua::json_to_lua_with_null(&lua, &v)?])),
                    Err(e) => nil_err(&lua, format!("invalid JSON body: {e}")),
                },
                Err(e) => nil_err(&lua, e),
            }
        });

        m.add_async_method("upgrade", |lua, this, ()| async move {
            let (on_upgrade, ws_key, ws_config, resp_tx, conn_permit, upgrade_phase, conn_cancel, ws_drained, peer_eof) = {
                let mut g = this.upgrade.lock().await;
                let Some(on_upgrade) = g.on_upgrade.take() else {
                    return nil_err(&lua, "not a websocket upgrade request (or already upgraded)");
                };
                let Some(ws_key) = g.ws_key.take() else {
                    return nil_err(&lua, "missing Sec-WebSocket-Key");
                };
                let Some(resp_tx) = g.resp_tx.take() else {
                    return nil_err(&lua, "response already sent");
                };
                let Some(conn_permit) = g.conn_permit.take() else {
                    return nil_err(&lua, "connection permit already released");
                };
                g.upgrade_phase.store(UPGRADE_IN_PROGRESS, Ordering::Release);
                (
                    on_upgrade,
                    ws_key,
                    g.ws_config,
                    resp_tx,
                    conn_permit,
                    g.upgrade_phase.clone(),
                    g.conn_cancel.clone(),
                    g.ws_drained.clone(),
                    g.peer_eof.clone(),
                )
            };
            let stall_limit = this.upgrade.lock().await.ws_stall_limit;
            #[cfg(test)]
            let test_hooks = this.upgrade.lock().await.test_hooks.clone();

            let mut upgrade_guard =
                UpgradeGuard { phase: upgrade_phase.clone(), conn_cancel: conn_cancel.clone(), armed: true };

            let accept = derive_accept_key(ws_key.as_bytes());
            let resp = hyper::Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(http::header::UPGRADE, "websocket")
                .header(http::header::CONNECTION, "Upgrade")
                .header("Sec-WebSocket-Accept", accept)
                .body(empty_body())
                .map_err(rt_err)?;

            if resp_tx.send(resp).is_err() {
                return nil_err(&lua, "connection closed before upgrade");
            }

            let upgraded = match on_upgrade.await {
                Ok(u) => u,
                Err(e) => return nil_err(&lua, format!("websocket upgrade failed: {e}")),
            };

            upgrade_phase.store(UPGRADE_SUCCEEDED, Ordering::Release);
            upgrade_guard.disarm();
            let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(ws_config)).await;
            let ud = lua.create_userdata(ServerWs::from_stream(
                ws,
                this.cancel.clone(),
                super::ws::WsConn {
                    cancel_guard: Arc::new(CancelOnDrop::new(conn_cancel)),
                    permit: conn_permit,
                    peer_eof,
                    stall_limit,
                },
                &this.tasks,
                ws_drained,
                #[cfg(test)]
                test_hooks,
            ))?;
            Ok(MultiValue::from_vec(vec![Value::UserData(ud)]))
        });
    }
}

fn format_sse_event(t: &Table) -> mlua::Result<String> {
    let mut out = String::new();
    if let Some(id) = t.get::<Option<String>>("id")? {
        out.push_str("id: ");
        out.push_str(&id.replace(['\r', '\n'], ""));
        out.push('\n');
    }
    if let Some(ev) = t.get::<Option<String>>("event")? {
        out.push_str("event: ");
        out.push_str(&ev.replace(['\r', '\n'], ""));
        out.push('\n');
    }
    if let Some(retry) = t.get::<Option<u64>>("retry")? {
        use std::fmt::Write;
        let _ = writeln!(out, "retry: {retry}");
    }
    if let Some(data) = t.get::<Option<String>>("data")? {
        for line in data.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    Ok(out)
}

async fn send_stream_err(
    tx: &mpsc::Sender<Result<Frame<Bytes>, std::io::Error>>,
    cancel: &CancellationToken,
    msg: String,
) {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {}
        _ = tx.send(Err(std::io::Error::other(msg))) => {}
    }
}

pub(super) fn build_lua_response(
    lua: &Lua,
    ectx: Option<Table>,
    val: Value,
    req_cancel: CancellationToken,
    tasks: &ActiveTasks,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> mlua::Result<hyper::Response<ServeBody>> {
    match val {
        Value::Nil => hyper::Response::builder().status(StatusCode::NO_CONTENT).body(empty_body()).map_err(rt_err),
        Value::Table(t) => {
            let status_u16 = t.get::<Option<u16>>("status")?.unwrap_or(200);
            let status = StatusCode::from_u16(status_u16).map_err(rt_err)?;
            let mut builder = hyper::Response::builder().status(status);
            let mut has_content_type = false;

            if let Some(headers) = t.get::<Option<Table>>("headers")? {
                for pair in headers.pairs::<String, String>() {
                    let (k, v) = pair?;
                    if k.eq_ignore_ascii_case("content-type") {
                        has_content_type = true;
                    }
                    let name = HeaderName::from_bytes(k.as_bytes()).map_err(rt_err)?;
                    let value = HeaderValue::from_str(&v).map_err(rt_err)?;
                    builder = builder.header(name, value);
                }
            }

            let body_val: Value = t.get("body")?;
            let body = match body_val {
                Value::Nil => empty_body(),
                Value::String(s) => full_body(Bytes::copy_from_slice(&s.as_bytes())),
                Value::Function(gen_fn) => {
                    if !has_content_type {
                        builder = builder.header(http::header::CONTENT_TYPE, "text/event-stream; charset=utf-8");
                    }
                    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(STREAM_QUEUE_CAPACITY);
                    let lua_c = lua.clone();
                    let stream_cancel = req_cancel.clone();
                    let gen_handle = mare_shared::txn_scope::spawn_local(async move {
                        let _permit = permit;
                        loop {
                            if req_cancel.is_cancelled() || tx.is_closed() {
                                break;
                            }
                            let next = tokio::select! {
                                _ = req_cancel.cancelled() => break,
                                _ = tx.closed() => break,
                                res = mare_shared::exec_ctx::call_in::<Value>(&lua_c, ectx.clone(), gen_fn.clone(), ()) => res,
                            };
                            match next {
                                Ok(Value::Nil) => break,
                                Ok(Value::String(s)) => {
                                    let b = Bytes::copy_from_slice(&s.as_bytes());
                                    tokio::select! {
                                        _ = req_cancel.cancelled() => break,
                                        res = tx.send(Ok(Frame::data(b))) => if res.is_err() { break; }
                                    }
                                }
                                Ok(Value::Table(ev)) => match format_sse_event(&ev) {
                                    Ok(s) => tokio::select! {
                                        _ = req_cancel.cancelled() => break,
                                        res = tx.send(Ok(Frame::data(Bytes::from(s)))) => if res.is_err() { break; }
                                    },
                                    Err(e) => {
                                        send_stream_err(&tx, &req_cancel, e.to_string()).await;
                                        break;
                                    }
                                },
                                Ok(other) => {
                                    send_stream_err(
                                        &tx,
                                        &req_cancel,
                                        format!("stream generator yielded unsupported type: {}", other.type_name()),
                                    )
                                    .await;
                                    break;
                                }
                                Err(e) => {
                                    send_stream_err(&tx, &req_cancel, e.to_string()).await;
                                    break;
                                }
                            }
                        }
                    });
                    tasks.push(gen_handle);
                    super::ReceiverStreamBody { rx, _cancel_guard: CancelOnDrop::new(stream_cancel) }.boxed()
                }
                other => {
                    return Err(rt_err(format!("response body must be string or function, got {}", other.type_name())));
                }
            };

            builder.body(body).map_err(rt_err)
        }
        other => Err(rt_err(format!("handler must return a response table or nil, got {}", other.type_name()))),
    }
}
