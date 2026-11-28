//! ACP server (`mare acp`): Agent Client Protocol over stdio via the `agent-client-protocol`
//! crate. The protocol connection runs on its own thread; mare hosts (one per ACP session) stay
//! on the main LocalSet (actor model) and are driven through channels.

mod approvals;
mod helpers;
mod turns;
mod wire;

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionConfigOption, SessionConfigOptionValue,
    SessionListCapabilities, SessionResumeCapabilities, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    StopReason,
};
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, ByteStreams, Client, JsonRpcResponse};
use anyhow::Result;
use serde_json::{Map, Value as J, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use mare_host::{
    Host, HostManager, HostOptions, OrchestratorCaller, OrchestratorError, OrchestratorExt, SessionError,
    SessionRuntime,
};
use mare_shared::ui::Mode;

use crate::rpc::call_service;
use crate::session_addr::{SelectorError, resolve_session};
use helpers::{
    available_commands, available_commands_update, command_result_text, config_options, extract_prompt, find_session,
    image_refusal, list_sessions, parse_slash_command, resolve_cwd, same_directory, set_config_option,
    translate_mcp_servers,
};
use mare_shared::wire_projection::WireProjector;
use turns::{
    AcpSessionAliases, ConfigSync, ForwarderTarget, PROMPT_DRAIN_TIMEOUT, SessionForwarders, TurnCancels,
    map_turn_result, wait_for_queued_input, wait_for_started_turn,
};
use wire::{send_reply, send_update};

enum AcpErr {
    InvalidParams(String),
    /// A session-scoped request names a session that does not exist or is not active: ACP's
    /// `-32002` Resource not found, with the session id (and why) in `data`.
    SessionNotFound {
        session_id: String,
        message: String,
    },
    Internal(String),
    Scoped(Box<Self>, Vec<WireProjector>),
}

/// The error for a request whose session the manager cannot hand out: one that is not loaded
/// (never existed, or closed) is not found; one still closing is a conflict with this request.
fn session_lookup_error(err: SessionError, session: &str) -> AcpErr {
    match err {
        SessionError::NotLoaded(_) => {
            AcpErr::SessionNotFound { session_id: session.to_string(), message: format!("unknown session `{session}`") }
        }
        SessionError::Closing(_) => AcpErr::InvalidParams(format!("session is already closing: {session}")),
        other => AcpErr::InvalidParams(other.to_string()),
    }
}

impl AcpErr {
    fn into_rpc(self) -> agent_client_protocol::Error {
        match self {
            Self::InvalidParams(msg) => agent_client_protocol::Error::invalid_params().data(msg),
            Self::SessionNotFound { session_id, message } => agent_client_protocol::Error::from(
                agent_client_protocol::ErrorCode::from(agent_client_protocol::v1::schema::ErrorCode::ResourceNotFound),
            )
            .data(serde_json::json!({ "sessionId": session_id, "message": message })),
            Self::Internal(msg) => agent_client_protocol::Error::internal_error().data(msg),
            Self::Scoped(error, projectors) => wire::project_error(error.into_rpc(), &projectors),
        }
    }

    fn with_projector(self, host: &Host) -> Self {
        self.with_hook(mare_shared::wire_projection::hook(host.lua()))
    }

    fn with_hook(self, projector: Option<WireProjector>) -> Self {
        self.with_hooks(projector.into_iter().collect())
    }

    fn with_hooks(self, projectors: Vec<WireProjector>) -> Self {
        if projectors.is_empty() {
            return self;
        }
        match self {
            Self::Scoped(error, mut existing) => {
                existing.extend(projectors);
                Self::Scoped(error, existing)
            }
            error => Self::Scoped(Box::new(error), projectors),
        }
    }
}

struct RestoredSession {
    config_options: Vec<SessionConfigOption>,
    updates: Vec<J>,
    commands_update: J,
    projector: Option<WireProjector>,
    replay_done: Option<oneshot::Sender<()>>,
}

struct CreatedSession {
    id: String,
    config_options: Vec<SessionConfigOption>,
    commands_update: J,
    projector: Option<WireProjector>,
    ready: oneshot::Sender<()>,
}

type NewSessionReply = Result<CreatedSession, AcpErr>;
type ConfigReply = Result<(Vec<SessionConfigOption>, Option<WireProjector>), AcpErr>;

enum Req {
    NewSession {
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        conn: ConnectionTo<Client>,
        reply: oneshot::Sender<NewSessionReply>,
    },
    RestoreSession {
        session_id: String,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        replay: bool,
        conn: ConnectionTo<Client>,
        reply: oneshot::Sender<Result<RestoredSession, AcpErr>>,
    },
    ListSessions {
        cwd: Option<PathBuf>,
        cursor: Option<String>,
        reply: oneshot::Sender<Result<(ListSessionsResponse, Vec<WireProjector>), AcpErr>>,
    },
    CloseSession {
        session: String,
        reply: oneshot::Sender<Result<(), AcpErr>>,
    },
    SetConfigOption {
        session: String,
        config_id: String,
        value: SessionConfigOptionValue,
        reply: oneshot::Sender<ConfigReply>,
    },
    Prompt {
        session: String,
        text: String,
        images: Vec<J>,
        content: Vec<J>,
        /// the session's turn token when the protocol thread accepted the prompt
        turn_cancel: CancellationToken,
        prompt_gen: u64,
        conn: ConnectionTo<Client>,
        reply: oneshot::Sender<Result<String, AcpErr>>,
    },
}

fn try_enqueue_req<T>(tx: &mpsc::Sender<Req>, req: Req, responder: Responder<T>) -> Option<Responder<T>>
where
    T: serde::Serialize + JsonRpcResponse,
{
    match tx.try_send(req) {
        Ok(()) => Some(responder),
        Err(mpsc::error::TrySendError::Full(_)) => {
            let _ = responder.respond_with_error(AcpErr::Internal("mare: ACP request queue full".into()).into_rpc());
            None
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            let _ = channel_closed(responder);
            None
        }
    }
}

fn channel_closed<T>(responder: Responder<T>) -> agent_client_protocol::Result<()>
where
    T: serde::Serialize + JsonRpcResponse,
{
    responder.respond_with_error(agent_client_protocol::Error::internal_error())
}

/// Forward an actor reply in a detached task so a busy session actor never stalls other handlers.
fn spawn_reply<V, T, F>(rrx: oneshot::Receiver<Result<V, AcpErr>>, responder: Responder<T>, map: F)
where
    V: Send + 'static,
    T: serde::Serialize + serde::de::DeserializeOwned + JsonRpcResponse + Send + 'static,
    F: FnOnce(V) -> (T, Vec<WireProjector>) + Send + 'static,
{
    tokio::spawn(async move {
        let _ = match rrx.await {
            Ok(Ok(val)) => {
                let (resp, projectors) = map(val);
                send_reply(responder, Ok(resp), &projectors)
            }
            Ok(Err(e)) => send_reply(responder, Err(e), &[]),
            Err(_) => channel_closed(responder),
        };
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_restore_request<T, F>(
    tx: &mpsc::Sender<Req>,
    session_id: String,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    mcp_servers: Vec<McpServer>,
    replay: bool,
    conn: ConnectionTo<Client>,
    responder: Responder<T>,
    make_response: F,
) -> agent_client_protocol::Result<()>
where
    T: serde::Serialize + serde::de::DeserializeOwned + JsonRpcResponse + Send + 'static,
    F: FnOnce(Vec<SessionConfigOption>) -> T + Send + 'static,
{
    let (rtx, rrx) = oneshot::channel();
    let Some(responder) = try_enqueue_req(
        tx,
        Req::RestoreSession {
            session_id: session_id.clone(),
            cwd,
            additional_directories,
            mcp_servers,
            replay,
            conn: conn.clone(),
            reply: rtx,
        },
        responder,
    ) else {
        return Ok(());
    };
    // the connection dispatches one handler at a time: stream any replay updates before
    // answering so the response lands strictly after the replay batch
    tokio::spawn(async move {
        let _ = match rrx.await {
            Ok(Ok(restored)) => {
                let RestoredSession { config_options, updates, commands_update, projector, replay_done } = restored;
                let mut replay_err = None;
                for u in updates {
                    if let Err(e) = send_update(&conn, &session_id, u, projector.as_slice()) {
                        replay_err = Some(e);
                        break;
                    }
                }
                let sent = match replay_err {
                    Some(e) => send_reply(responder, Err(AcpErr::Internal(e)), projector.as_slice()),
                    None => {
                        let res = send_reply(responder, Ok(make_response(config_options)), projector.as_slice());
                        if res.is_ok() {
                            let _ = send_update(&conn, &session_id, commands_update, projector.as_slice());
                        }
                        res
                    }
                };
                if let Some(tx) = replay_done {
                    let _ = tx.send(());
                }
                sent
            }
            Ok(Err(e)) => send_reply(responder, Err(e), &[]),
            Err(_) => channel_closed(responder),
        };
    });
    Ok(())
}

struct TokioCompat<T>(T);

impl<T: tokio::io::AsyncRead + Unpin> futures_util::io::AsyncRead for TokioCompat<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let mut rb = tokio::io::ReadBuf::new(buf);
        match tokio::io::AsyncRead::poll_read(Pin::new(&mut self.0), cx, &mut rb) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> futures_util::io::AsyncWrite for TokioCompat<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(Pin::new(&mut self.0), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(Pin::new(&mut self.0), cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(Pin::new(&mut self.0), cx)
    }
}

/// Protocol side (separate thread, Send world).
async fn serve(
    tx: mpsc::Sender<Req>,
    cancels: TurnCancels,
    cancel_tx: mpsc::Sender<(String, u64)>,
    client_compaction: Arc<AtomicBool>,
    proto_stdout: std::fs::File,
) -> agent_client_protocol::Result<()> {
    let prompt_cancels = cancels.clone();
    let tx_new = tx.clone();
    let tx_load = tx.clone();
    let tx_resume = tx.clone();
    let tx_list = tx.clone();
    let tx_close = tx.clone();
    let tx_cfg = tx.clone();
    let tx_prompt = tx;

    Agent
        .v1()
        .name("mare")
        .on_receive_request(
            async move |init: InitializeRequest, responder, _conn| {
                let supports_compaction =
                    init.client_capabilities.session.as_ref().and_then(|s| s.compaction.as_ref()).is_some();
                client_compaction.store(supports_compaction, Ordering::Relaxed);
                let caps = AgentCapabilities::new()
                    .load_session(true)
                    .mcp_capabilities(McpCapabilities::new().http(true))
                    .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                    .session_capabilities(
                        SessionCapabilities::new()
                            .list(SessionListCapabilities::new())
                            .resume(SessionResumeCapabilities::new())
                            .close(SessionCloseCapabilities::new()),
                    );
                responder.respond(
                    InitializeResponse::new(init.protocol_version.min(ProtocolVersion::LATEST))
                        .agent_info(Implementation::new("mare", env!("CARGO_PKG_VERSION")))
                        .agent_capabilities(caps),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_auth: AuthenticateRequest, responder, _conn| responder.respond(AuthenticateResponse::new()),
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, conn| {
                let (rtx, rrx) = oneshot::channel();
                let conn_reply = conn.clone();
                let Some(responder) = try_enqueue_req(
                    &tx_new,
                    Req::NewSession {
                        cwd: req.cwd,
                        additional_directories: req.additional_directories,
                        mcp_servers: req.mcp_servers,
                        conn,
                        reply: rtx,
                    },
                    responder,
                ) else {
                    return Ok(());
                };
                // the connection dispatches one handler at a time: answer from a task, so a
                // busy session actor never holds up later messages (other requests, cancels).
                // Publish `available_commands_update` immediately after `NewSessionResponse` so
                // the client already knows `sessionId` and receives the command list before live
                // forwarder updates begin.
                tokio::spawn(async move {
                    let _ = match rrx.await {
                        Ok(Ok(created)) => {
                            let CreatedSession { id, config_options, commands_update, projector, ready } = created;
                            let resp = NewSessionResponse::new(id.clone()).config_options(config_options);
                            let sent = send_reply(responder, Ok(resp), projector.as_slice());
                            if sent.is_ok() {
                                let _ = send_update(&conn_reply, &id, commands_update, projector.as_slice());
                            }
                            let _ = ready.send(());
                            sent
                        }
                        Ok(Err(e)) => send_reply(responder, Err(e), &[]),
                        Err(_) => channel_closed(responder),
                    };
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, conn| {
                handle_restore_request(
                    &tx_load,
                    req.session_id.to_string(),
                    req.cwd,
                    req.additional_directories,
                    req.mcp_servers,
                    true,
                    conn,
                    responder,
                    |opts| LoadSessionResponse::new().config_options(opts),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ResumeSessionRequest, responder, conn| {
                handle_restore_request(
                    &tx_resume,
                    req.session_id.to_string(),
                    req.cwd,
                    req.additional_directories,
                    req.mcp_servers,
                    false,
                    conn,
                    responder,
                    |opts| ResumeSessionResponse::new().config_options(opts),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ListSessionsRequest, responder, _conn| {
                let (rtx, rrx) = oneshot::channel();
                let Some(responder) = try_enqueue_req(
                    &tx_list,
                    Req::ListSessions { cwd: req.cwd, cursor: req.cursor, reply: rtx },
                    responder,
                ) else {
                    return Ok(());
                };
                // the connection dispatches one handler at a time: answer from a task, so a
                // busy session actor never holds up later messages (other requests, cancels)
                spawn_reply(rrx, responder, |(res, projectors)| (res, projectors));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: CloseSessionRequest, responder, _conn| {
                let (rtx, rrx) = oneshot::channel();
                let Some(responder) = try_enqueue_req(
                    &tx_close,
                    Req::CloseSession { session: req.session_id.to_string(), reply: rtx },
                    responder,
                ) else {
                    return Ok(());
                };
                // the connection dispatches incoming messages one handler at a time: await the
                // close result in a separate task so slow session shutdown never blocks other
                // sessions' requests or approval replies
                spawn_reply(rrx, responder, |()| (CloseSessionResponse::new(), Vec::new()));
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest, responder, _conn| {
                let (rtx, rrx) = oneshot::channel();
                let Some(responder) = try_enqueue_req(
                    &tx_cfg,
                    Req::SetConfigOption {
                        session: req.session_id.to_string(),
                        config_id: req.config_id.to_string(),
                        value: req.value,
                        reply: rtx,
                    },
                    responder,
                ) else {
                    return Ok(());
                };
                // the connection dispatches incoming messages one handler at a time: await the
                // result in a separate task so a slow config switch (e.g. compaction on model
                // change) never blocks other sessions' requests, approvals, or cancel
                spawn_reply(rrx, responder, |(opts, projector)| {
                    (SetSessionConfigOptionResponse::new(opts), projector.into_iter().collect())
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, conn| {
                let extracted = match extract_prompt(&req.prompt) {
                    Ok(v) => v,
                    Err(msg) => return responder.respond_with_error(AcpErr::InvalidParams(msg).into_rpc()),
                };
                let session = req.session_id.to_string();
                // taken in dispatch order: a later `session/cancel` covers this prompt even before
                // the session actor starts it; an unknown session gets a token nobody cancels
                let (turn_cancel, prompt_gen) =
                    prompt_cancels.current_with_gen(&session).unwrap_or_else(|| (CancellationToken::new(), 0));
                let (rtx, rrx) = oneshot::channel();
                let Some(responder) = try_enqueue_req(
                    &tx_prompt,
                    Req::Prompt {
                        session,
                        text: extracted.text,
                        images: extracted.images,
                        content: extracted.content,
                        turn_cancel,
                        prompt_gen,
                        conn,
                        reply: rtx,
                    },
                    responder,
                ) else {
                    return Ok(());
                };
                // the connection dispatches incoming messages one handler at a time: run the turn in
                // its own task so `session/cancel` (and other requests) are handled meanwhile
                tokio::spawn(async move {
                    let stop = match rrx.await {
                        Ok(r) => r,
                        Err(_) => Err(AcpErr::Internal("mare: prompt task ended unexpectedly".into())),
                    };
                    let stop = match stop {
                        Ok(s) => s,
                        Err(err) => return send_reply(responder, Err(err), &[]),
                    };
                    let reason = match stop.as_str() {
                        "cancelled" => StopReason::Cancelled,
                        "max_tokens" => StopReason::MaxTokens,
                        "max_turn_requests" => StopReason::MaxTurnRequests,
                        "refusal" => StopReason::Refusal,
                        _ => StopReason::EndTurn,
                    };
                    send_reply(responder, Ok(PromptResponse::new(reason)), &[])
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _conn| {
                let sid = n.session_id.to_string();
                if let Some(cancel_gen) = cancels.cancel_with_gen(&sid) {
                    let _ = cancel_tx.send((sid, cancel_gen)).await;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(ByteStreams::new(
            TokioCompat(tokio::fs::File::from_std(proto_stdout)),
            TokioCompat(tokio::io::stdin()),
        ))
        .await
}

fn build_session_config(mut base: J, mcp_servers: Map<String, J>) -> J {
    if mcp_servers.is_empty() {
        return base;
    }
    let root = base.as_object_mut();
    if let Some(root) = root {
        let plugins = root.entry("plugins").or_insert_with(|| json!({}));
        if let Some(plugins_obj) = plugins.as_object_mut() {
            let mcp = plugins_obj.entry("mcp").or_insert_with(|| json!({}));
            if let Some(mcp_obj) = mcp.as_object_mut() {
                let servers = mcp_obj.entry("servers").or_insert_with(|| json!({}));
                if let Some(servers_obj) = servers.as_object_mut() {
                    for (k, v) in mcp_servers {
                        servers_obj.insert(k, v);
                    }
                }
            }
        }
    }
    base
}

fn cli_trust_override(opts: &HostOptions) -> Option<bool> {
    (opts.args.get("trust_source").and_then(|v| v.as_str()) == Some("cli"))
        .then(|| opts.args.get("trust").and_then(|v| v.as_str()) == Some("trusted"))
}

fn session_trust(opts: &HostOptions, cwd: &std::path::Path) -> bool {
    let same_ws = same_directory(&opts.cwd, cwd);
    mare_host::resolve_workspace_trust(
        cwd,
        same_ws,
        opts.project_trusted,
        cli_trust_override(opts),
        opts.no_user_plugins,
    )
}

fn session_args(opts: &HostOptions, cwd: &std::path::Path, trusted: bool, resume: Option<&str>) -> J {
    let mut args = opts.args.clone();
    let same_ws = same_directory(&opts.cwd, cwd);
    let plugins_enabled = opts.load_init && !opts.no_user_plugins;
    mare_host::sync_workspace_trust_args(&mut args, cwd, same_ws, trusted, cli_trust_override(opts), plugins_enabled);
    if let Some(r) = resume
        && let Some(obj) = args.as_object_mut()
    {
        obj.insert("resume".into(), J::String(r.to_string()));
    }
    args
}

async fn start_host(
    base_opts: &HostOptions,
    cwd: PathBuf,
    mcp_servers: Map<String, J>,
    resume: Option<&str>,
    strict_config: bool,
    orchestrator: Rc<mare_host::SessionOrchestrator>,
) -> Result<Arc<Host>, AcpErr> {
    let trusted = session_trust(base_opts, &cwd);
    let args = session_args(base_opts, &cwd, trusted, resume);
    let o = HostOptions {
        cwd,
        mode: Mode::Acp,
        config: build_session_config(base_opts.config.clone(), mcp_servers),
        load_init: base_opts.load_init,
        project_trusted: trusted,
        args,
        no_user_plugins: base_opts.no_user_plugins,
    };
    let host = Host::new_with_orchestrator(o, orchestrator).await.map_err(|e| AcpErr::Internal(e.to_string()))?;
    if let Err(msg) = crate::report_config_warnings(&host, strict_config) {
        let err = AcpErr::InvalidParams(msg).with_projector(&host);
        host.shutdown().await;
        return Err(err);
    }
    if let Some(msg) = host.config_error() {
        let err = AcpErr::Internal(msg).with_projector(&host);
        host.shutdown().await;
        return Err(err);
    }
    crate::report_plugin_warnings(&host);
    if let Err(e) = crate::settle_headless_trust(&host).await {
        let err = AcpErr::Internal(format!("{e:#}")).with_projector(&host);
        host.shutdown().await;
        return Err(err);
    }
    if resume.is_some()
        && let Some(err) = crate::headless::resume_error(&host).await
    {
        let err = AcpErr::InvalidParams(err).with_projector(&host);
        host.shutdown().await;
        return Err(err);
    }
    Ok(host)
}

fn map_additional_directories_error(err: anyhow::Error) -> AcpErr {
    let msg = err.to_string();
    let code = err.chain().find_map(|cause| {
        if let Some(mare_kernel::ServiceCallError::Coded(ev)) = cause.downcast_ref::<mare_kernel::ServiceCallError>() {
            return Some(ev.code.as_str());
        }
        if let Some(coded) = cause.downcast_ref::<mare_shared::errors::Coded>() {
            return Some(coded.code);
        }
        if let Some(lua) = cause.downcast_ref::<mlua::Error>()
            && let Some(coded) = lua.downcast_ref::<mare_shared::errors::Coded>()
        {
            return Some(coded.code);
        }
        None
    });
    match code {
        Some("invalid_arguments" | "denied" | mare_binding::ossandbox::SANDBOX_INVALID_ROOT) => {
            AcpErr::InvalidParams(msg)
        }
        _ => AcpErr::Internal(msg),
    }
}

async fn apply_additional_directories(host: &Host, dirs: &[PathBuf]) -> Result<(), AcpErr> {
    if dirs.is_empty() {
        return Ok(());
    }
    let dir_strings: Vec<String> = dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    if let Err(err) = call_service(host, "session", "add_workspace_dirs", vec![json!(dir_strings)]).await {
        return Err(map_additional_directories_error(err).with_projector(host));
    }
    Ok(())
}

fn map_command_run_error(err: anyhow::Error) -> AcpErr {
    if let Some(mare_kernel::ServiceCallError::Coded(ev)) = err.downcast_ref::<mare_kernel::ServiceCallError>()
        && matches!(
            ev.code.as_str(),
            "invalid_arguments" | "commands.local_only" | "commands.command_unavailable" | "commands.unknown_command"
        )
    {
        return AcpErr::InvalidParams(ev.message.clone());
    }
    AcpErr::Internal(err.to_string())
}

async fn open_session_forwarder(
    host: &Arc<Host>,
    session_id: &str,
    conn: &ConnectionTo<Client>,
    cancel: &CancellationToken,
    forwarders: &SessionForwarders,
    start_after: Option<oneshot::Receiver<()>>,
    compaction_updates: bool,
) -> (Vec<SessionConfigOption>, J) {
    let cfg_opts = config_options(host).await;
    let cfg_json = serde_json::to_value(&cfg_opts).unwrap_or(J::Null);
    let cmds = available_commands(host).await;
    let cmds_json = serde_json::to_value(&cmds).unwrap_or(J::Null);
    let cmds_update = available_commands_update(&cmds);
    let cursor = host.kernel.open_cursor(std::sync::Arc::new(|name| helpers::handles_event(name).then_some(true)));
    let projector = mare_shared::wire_projection::hook(host.lua());
    let config_sync = ConfigSync::install(host, cfg_json, cmds_json, compaction_updates);
    forwarders.open(
        session_id,
        ForwarderTarget {
            cursor,
            conn: conn.clone(),
            projector,
            stop: cancel.child_token(),
            start_after,
            config: config_sync,
        },
    );
    (cfg_opts, cmds_update)
}

#[allow(clippy::too_many_arguments)]
async fn finish_acp_session(
    manager: &std::rc::Rc<HostManager>,
    rt: &mare_host::SessionRuntime,
    conn: &ConnectionTo<Client>,
    cancels: &TurnCancels,
    forwarders: &SessionForwarders,
    start_after: Option<oneshot::Receiver<()>>,
    compaction_updates: bool,
) -> std::result::Result<(Vec<SessionConfigOption>, J), AcpErr> {
    if let Err(e) = approvals::install(&rt.host, &rt.session_id, conn, rt.cancel.clone()) {
        let err = AcpErr::Internal(e.to_string()).with_projector(&rt.host);
        let _ = manager.close_session(&rt.session_id).await;
        return Err(err);
    }
    let (cfg_opts, cmds_update) =
        open_session_forwarder(&rt.host, &rt.session_id, conn, &rt.cancel, forwarders, start_after, compaction_updates)
            .await;
    cancels.open(&rt.session_id);
    Ok((cfg_opts, cmds_update))
}

struct AcpSessionExt {
    base_opts: HostOptions,
    cancels: TurnCancels,
    forwarders: SessionForwarders,
    aliases: Rc<RefCell<AcpSessionAliases>>,
    pending_aborts: Rc<RefCell<HashMap<String, tokio::task::JoinHandle<()>>>>,
    conn: Rc<RefCell<Option<ConnectionTo<Client>>>>,
    client_compaction: Arc<AtomicBool>,
}

impl OrchestratorExt for AcpSessionExt {
    fn clean_opts(
        &self,
        cwd_override: Option<&str>,
        no_session_override: Option<bool>,
    ) -> std::result::Result<HostOptions, OrchestratorError> {
        let cwd = match cwd_override {
            Some(raw) => {
                let p = PathBuf::from(raw);
                let resolved = if p.is_absolute() { p } else { self.base_opts.cwd.join(p) };
                if !resolved.is_dir() {
                    return Err(OrchestratorError::invalid_arguments("cwd does not exist or is not a directory"));
                }
                resolved
            }
            None => self.base_opts.cwd.clone(),
        };
        let trusted = session_trust(&self.base_opts, &cwd);
        let mut args = session_args(&self.base_opts, &cwd, trusted, None);
        if !args.is_object() {
            args = json!({});
        }
        if let Some(obj) = args.as_object_mut() {
            for key in [
                "session_id",
                "name",
                "session_new",
                "resume",
                "continue",
                "fork_session",
                "fork_before_entry",
                "add_dirs",
            ] {
                obj.remove(key);
            }
            if let Some(ns) = no_session_override {
                obj.insert("no_session".into(), J::Bool(ns));
            }
        }
        Ok(HostOptions {
            cwd,
            mode: Mode::Acp,
            config: self.base_opts.config.clone(),
            load_init: self.base_opts.load_init,
            project_trusted: trusted,
            args,
            no_user_plugins: self.base_opts.no_user_plugins,
        })
    }

    fn opened(
        &self,
        _caller: &OrchestratorCaller,
        rt: &Rc<SessionRuntime>,
        _make_default: bool,
        _subscribe: Option<bool>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        let sid = rt.session_id.clone();
        let already_open = self.cancels.current(&sid).is_some();
        if !already_open {
            self.cancels.open(&sid);
            if let Some(conn) = self.conn.borrow().clone() {
                if let Err(e) = approvals::install(&rt.host, &sid, &conn, rt.cancel.clone()) {
                    tracing::warn!(session_id = %sid, "installing ACP approvals on Lua-opened session failed: {e}");
                }
                let host = Arc::clone(&rt.host);
                let cancel = rt.cancel.clone();
                let forwarders = self.forwarders.clone();
                let compaction_updates = self.client_compaction.load(Ordering::Relaxed);
                return Box::pin(async move {
                    let _ = open_session_forwarder(&host, &sid, &conn, &cancel, &forwarders, None, compaction_updates)
                        .await;
                });
            }
        }
        Box::pin(async {})
    }

    fn closed(&self, session_id: &str) {
        let ext_sid = self.aliases.borrow_mut().remove_by_internal(session_id);
        self.forwarders.close(&ext_sid);
        self.cancels.close(&ext_sid);
        self.pending_aborts.borrow_mut().remove(&ext_sid);
    }

    fn resolve_selector(
        &self,
        anchor: &Arc<Host>,
        caller: &OrchestratorCaller,
        selector: &str,
        include_archived: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<String, OrchestratorError>>>> {
        let anchor = Arc::clone(anchor);
        let selector = selector.to_owned();
        let allow_path = caller.allow_path();
        let may_list = caller.may_list();
        Box::pin(async move {
            resolve_session(&anchor, &selector, allow_path, include_archived).await.map_err(|e| {
                match e.downcast_ref::<SelectorError>() {
                    Some(err) => OrchestratorError::invalid_arguments(err.for_caller(may_list)),
                    None => OrchestratorError::internal(e.to_string()),
                }
            })
        })
    }
}

pub async fn run(opts: HostOptions, strict_config: bool) -> Result<i32> {
    if strict_config {
        let warnings = mare_config::env_warnings();
        if !warnings.is_empty() {
            eprintln!("mare: configuration warnings (--strict-config): {}", warnings.join("; "));
            return Ok(2);
        }
    }
    let (tx, mut rx) = mpsc::channel::<Req>(256);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<(String, u64)>(256);
    let cancels = TurnCancels::default();
    let proto_cancels = cancels.clone();
    let client_compaction = Arc::new(AtomicBool::new(false));
    let proto_compaction = Arc::clone(&client_compaction);
    let proto_stdout = crate::clone_protocol_stdout()?;
    let proto = std::thread::Builder::new().name("mare-acp".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("acp runtime");
        if let Err(e) = rt.block_on(serve(tx, proto_cancels, cancel_tx, proto_compaction, proto_stdout)) {
            eprintln!("mare acp: {e:?}");
        }
    })?;
    let manager = HostManager::new();
    let forwarders = SessionForwarders::default();
    let aliases: Rc<RefCell<AcpSessionAliases>> = Rc::new(RefCell::new(AcpSessionAliases::default()));
    let pending_aborts: Rc<RefCell<HashMap<String, tokio::task::JoinHandle<()>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let active_conn: Rc<RefCell<Option<ConnectionTo<Client>>>> = Rc::new(RefCell::new(None));
    manager.orchestrator().set_project_cwd(&opts.cwd);
    manager.orchestrator().set_ext(Rc::new(AcpSessionExt {
        base_opts: opts.clone(),
        cancels: cancels.clone(),
        forwarders: forwarders.clone(),
        aliases: Rc::clone(&aliases),
        pending_aborts: Rc::clone(&pending_aborts),
        conn: Rc::clone(&active_conn),
        client_compaction: Arc::clone(&client_compaction),
    }));
    let in_flight_prompts: Rc<RefCell<HashMap<String, u64>>> = Rc::new(RefCell::new(HashMap::new()));
    let rekey_aliases = Rc::clone(&aliases);
    let _rekey_hook = manager.on_session_rekey(move |old_sid, new_sid, _rt| {
        // Keep the client-visible ACP sessionId fixed (kimi-code `AcpSession.sessionId`):
        // update only the internal lookup alias so forwarders, approvals, cancels, and
        // in-flight prompt guards stay bound to the stable external ACP session ID.
        rekey_aliases.borrow_mut().rekey(old_sid, new_sid);
    });
    // SIGTERM / SIGHUP / SIGINT: stop taking requests and shut down normally, so plugins still see
    // app/shutdown. Idle sessions close at once; a turn in flight is cancelled and awaited, with
    // the forced-exit deadline (and a second signal) as the backstop.
    let terminated = CancellationToken::new();
    let terminate_code = Rc::new(std::cell::Cell::new(TERMINATED_EXIT_CODE));
    let on_terminate = terminated.clone();
    let on_terminate_code = Rc::clone(&terminate_code);
    tokio::task::spawn_local(async move {
        let mut sigint = crate::signals::Interrupts::listen(true);
        let code = tokio::select! {
            _ = crate::signals::terminate() => TERMINATED_EXIT_CODE,
            _ = sigint.recv() => INTERRUPTED_EXIT_CODE,
        };
        on_terminate_code.set(code);
        crate::signals::begin_forceable_shutdown(code);
        on_terminate.cancel();
    });
    enum ActorEvent {
        Terminate,
        Cancel(String, u64),
        Req(Req),
    }
    loop {
        let ev = tokio::select! {
            biased;
            () = terminated.cancelled() => ActorEvent::Terminate,
            Some((sid, cancel_gen)) = cancel_rx.recv() => ActorEvent::Cancel(sid, cancel_gen),
            req = rx.recv() => match req {
                Some(req) => ActorEvent::Req(req),
                None => break,
            },
        };
        let req = match ev {
            ActorEvent::Terminate => break,
            ActorEvent::Cancel(sid, cancel_gen) => {
                // If an in-flight Req::Prompt owns this session, either its `turn_cancel` fired
                // and it cancels its own prompt/turn/queued input itself (prompt_gen < cancel_gen),
                // or it started after this cancel (prompt_gen >= cancel_gen). Only abort directly
                // when no prompt is in flight (spontaneous background wakeup turn) and track the
                // handle so any subsequent prompt waits for the abort to settle before starting.
                let internal_sid = aliases.borrow().resolve_internal(&sid);
                if !in_flight_prompts.borrow().contains_key(&sid)
                    && cancels.current_with_gen(&sid).is_some_and(|(_, cur_gen)| cur_gen >= cancel_gen)
                    && let Ok(entry) = manager.session(&internal_sid)
                {
                    let host = entry.host.clone();
                    let handle = tokio::task::spawn_local(async move {
                        turns::cancel_running_turn(&host).await;
                    });
                    pending_aborts.borrow_mut().insert(sid, handle);
                }
                continue;
            }
            ActorEvent::Req(req) => req,
        };
        match req {
            Req::NewSession { cwd, additional_directories, mcp_servers, conn, reply } => {
                *active_conn.borrow_mut() = Some(conn.clone());
                let cwd = match resolve_cwd(&cwd, &opts.cwd) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = reply.send(Err(AcpErr::InvalidParams(e)));
                        continue;
                    }
                };
                let validated_dirs =
                    match mare_binding::workspace::validate_additional_dirs(&cwd, &additional_directories) {
                        Ok(dirs) => dirs,
                        Err(e) => {
                            let _ = reply.send(Err(AcpErr::InvalidParams(e)));
                            continue;
                        }
                    };
                let mcp = match translate_mcp_servers(&mcp_servers, &cwd) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = reply.send(Err(AcpErr::InvalidParams(e)));
                        continue;
                    }
                };
                match start_host(&opts, cwd, mcp, None, strict_config, manager.orchestrator().clone()).await {
                    Ok(host) => {
                        if reply.is_closed() {
                            host.shutdown().await;
                            continue;
                        }
                        if let Err(err) = apply_additional_directories(&host, &validated_dirs).await {
                            host.shutdown().await;
                            let _ = reply.send(Err(err));
                            continue;
                        }
                        let projector = mare_shared::wire_projection::hook(host.lua());
                        let rt = match manager.register_host(host, None, false).await {
                            Ok(rt) => rt,
                            Err(SessionError::MissingSessionId) => {
                                let _ = reply
                                    .send(Err(AcpErr::Internal("session service did not return a session id".into())
                                        .with_hook(projector.clone())));
                                continue;
                            }
                            Err(e) => {
                                let _ = reply.send(Err(AcpErr::Internal(e.to_string()).with_hook(projector.clone())));
                                continue;
                            }
                        };
                        let compaction_updates = client_compaction.load(Ordering::Relaxed);
                        let (ready_tx, start_after) = oneshot::channel();
                        let res = finish_acp_session(
                            &manager,
                            &rt,
                            &conn,
                            &cancels,
                            &forwarders,
                            Some(start_after),
                            compaction_updates,
                        )
                        .await
                        .map(|(cfg_opts, cmds_update)| CreatedSession {
                            id: rt.session_id.clone(),
                            config_options: cfg_opts,
                            commands_update: cmds_update,
                            projector: mare_shared::wire_projection::hook(rt.host.lua()),
                            ready: ready_tx,
                        });
                        if reply.is_closed() || reply.send(res).is_err() {
                            let ext_sid = aliases.borrow_mut().remove_by_internal(&rt.session_id);
                            forwarders.close(&ext_sid);
                            cancels.close(&ext_sid);
                            let _ = manager.close_session(&rt.session_id).await;
                        }
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Req::RestoreSession { session_id, cwd, additional_directories, mcp_servers, replay, conn, reply } => {
                *active_conn.borrow_mut() = Some(conn.clone());
                let cwd = match resolve_cwd(&cwd, &opts.cwd) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = reply.send(Err(AcpErr::InvalidParams(e)));
                        continue;
                    }
                };
                let validated_dirs =
                    match mare_binding::workspace::validate_additional_dirs(&cwd, &additional_directories) {
                        Ok(dirs) => dirs,
                        Err(e) => {
                            let _ = reply.send(Err(AcpErr::InvalidParams(e)));
                            continue;
                        }
                    };
                let lookup_sid = aliases.borrow().resolve_internal(&session_id);
                match manager.session(&lookup_sid) {
                    Ok(entry) => {
                        let error = AcpErr::InvalidParams(SessionError::AlreadyLoaded(session_id).to_string())
                            .with_projector(&entry.host);
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    Err(e @ SessionError::Closing(_)) => {
                        let _ = reply.send(Err(AcpErr::InvalidParams(e.to_string())));
                        continue;
                    }
                    Err(SessionError::NotLoaded(_)) => {}
                    Err(e) => {
                        let _ = reply.send(Err(AcpErr::Internal(e.to_string())));
                        continue;
                    }
                }
                let sid_lookup = session_id.clone();
                let Ok(Some(persisted)) = tokio::task::spawn_blocking(move || find_session(&sid_lookup)).await else {
                    let message = format!("session is not resumable: {session_id}");
                    let _ = reply.send(Err(AcpErr::SessionNotFound { session_id, message }));
                    continue;
                };
                if !same_directory(&persisted.cwd, &cwd) {
                    let _ = reply
                        .send(Err(AcpErr::InvalidParams(format!("session cwd does not match: {}", cwd.display()))));
                    continue;
                }
                let mcp = match translate_mcp_servers(&mcp_servers, &cwd) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = reply.send(Err(AcpErr::InvalidParams(e)));
                        continue;
                    }
                };
                let path_str = persisted.path.to_string_lossy().into_owned();
                match start_host(&opts, cwd, mcp, Some(&path_str), strict_config, manager.orchestrator().clone()).await
                {
                    Ok(host) => {
                        if reply.is_closed() {
                            host.shutdown().await;
                            continue;
                        }
                        if !validated_dirs.is_empty() {
                            let restored_dirs = mare_binding::workspace::dirs(host.lua());
                            let all_recorded =
                                validated_dirs.iter().all(|req| restored_dirs.iter().any(|r| same_directory(r, req)));
                            if !all_recorded {
                                let err = AcpErr::InvalidParams(
                                    "additionalDirectories does not match the session's recorded workspace directories"
                                        .into(),
                                )
                                .with_projector(&host);
                                host.shutdown().await;
                                let _ = reply.send(Err(err));
                                continue;
                            }
                        }
                        let projector = mare_shared::wire_projection::hook(host.lua());
                        let rt = match manager.register_host(host, Some(&session_id), false).await {
                            Ok(rt) => rt,
                            Err(SessionError::SessionIdMismatch { .. }) => {
                                let _ = reply
                                    .send(Err(AcpErr::Internal("resumed session id mismatch".into())
                                        .with_hook(projector.clone())));
                                continue;
                            }
                            Err(e) => {
                                let _ = reply.send(Err(AcpErr::Internal(e.to_string()).with_hook(projector.clone())));
                                continue;
                            }
                        };
                        let compaction_updates = client_compaction.load(Ordering::Relaxed);
                        let updates = if replay {
                            let tool_info = helpers::tool_metadata_map(&rt.host).await;
                            match helpers::collect_replay_messages(&rt.host).await {
                                Ok(msgs) => helpers::messages_to_updates_with_context(
                                    &msgs,
                                    &tool_info,
                                    helpers::TranslateOpts { cwd: Some(rt.host.cwd()), compaction_updates },
                                ),
                                Err(e) => {
                                    let err = AcpErr::Internal(e.to_string()).with_projector(&rt.host);
                                    let _ = manager.close_session(&rt.session_id).await;
                                    let _ = reply.send(Err(err));
                                    continue;
                                }
                            }
                        } else {
                            Vec::new()
                        };
                        let (replay_done, start_after) = {
                            let (tx, rx) = oneshot::channel();
                            (Some(tx), Some(rx))
                        };
                        let res = finish_acp_session(
                            &manager,
                            &rt,
                            &conn,
                            &cancels,
                            &forwarders,
                            start_after,
                            compaction_updates,
                        )
                        .await
                        .map(|(cfg_opts, cmds_update)| RestoredSession {
                            config_options: cfg_opts,
                            updates,
                            commands_update: cmds_update,
                            projector: mare_shared::wire_projection::hook(rt.host.lua()),
                            replay_done,
                        });
                        if reply.is_closed() || reply.send(res).is_err() {
                            let ext_sid = aliases.borrow_mut().remove_by_internal(&rt.session_id);
                            forwarders.close(&ext_sid);
                            cancels.close(&ext_sid);
                            let _ = manager.close_session(&rt.session_id).await;
                        }
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Req::ListSessions { cwd, cursor, reply } => {
                let active = manager.active_and_closing_ids();
                let projectors: Vec<WireProjector> = active
                    .iter()
                    .filter_map(|id| manager.session(id).ok())
                    .filter_map(|rt| mare_shared::wire_projection::hook(rt.host.lua()))
                    .collect();
                let active_scan = active.clone();
                let res = match tokio::task::spawn_blocking(move || {
                    list_sessions(cwd.as_deref(), cursor.as_deref(), &active_scan)
                })
                .await
                {
                    Ok(Ok(result)) => Ok((result, projectors)),
                    Ok(Err(error)) => Err(AcpErr::InvalidParams(error).with_hooks(projectors)),
                    Err(e) => Err(AcpErr::Internal(format!("session list worker failed: {e}")).with_hooks(projectors)),
                };
                let _ = reply.send(res);
            }
            Req::CloseSession { session, reply } => {
                if manager.is_shutting_down() {
                    let _ = reply.send(Err(AcpErr::Internal(SessionError::ShuttingDown.to_string())));
                    continue;
                }
                let internal_sid = aliases.borrow().resolve_internal(&session);
                let entry = match manager.session(&internal_sid) {
                    Ok(entry) => entry,
                    Err(err) => {
                        let _ = reply.send(Err(session_lookup_error(err, &session)));
                        continue;
                    }
                };
                let projector = mare_shared::wire_projection::hook(entry.host.lua());
                if let Err(err) = manager.begin_close(&internal_sid, true, move |res| {
                    let _ = reply.send(res.map_err(|error| AcpErr::Internal(error).with_hook(projector)));
                }) {
                    tracing::error!("unexpected begin_close failure for `{session}`: {err}");
                } else {
                    aliases.borrow_mut().remove_by_external(&session);
                    forwarders.close(&session);
                    cancels.close(&session);
                    pending_aborts.borrow_mut().remove(&session);
                }
            }
            Req::SetConfigOption { session, config_id, value, reply } => {
                let internal_sid = aliases.borrow().resolve_internal(&session);
                let entry = match manager.session(&internal_sid) {
                    Ok(entry) => entry,
                    Err(err) => {
                        let _ = reply.send(Err(session_lookup_error(err, &session)));
                        continue;
                    }
                };
                let projector = mare_shared::wire_projection::hook(entry.host.lua());
                match entry.enqueue_config(move |host| async move {
                    set_config_option(&host, &config_id, &value)
                        .await
                        .map(|options| (options, mare_shared::wire_projection::hook(host.lua())))
                        .map_err(|e| AcpErr::InvalidParams(e).with_projector(&host))
                }) {
                    Ok(rx) => {
                        tokio::task::spawn_local(async move {
                            let res = match rx.await {
                                Ok(res) => res,
                                Err(_) => {
                                    Err(AcpErr::Internal("session config worker closed".into()).with_hook(projector))
                                }
                            };
                            let _ = reply.send(res);
                        });
                    }
                    Err(_) => {
                        let _ = reply
                            .send(Err(AcpErr::Internal("session config worker closed".into()).with_hook(projector)));
                    }
                }
            }
            Req::Prompt { session, text, images, content, turn_cancel, prompt_gen, conn, reply } => {
                let internal_sid = aliases.borrow().resolve_internal(&session);
                let entry = match manager.session(&internal_sid) {
                    Ok(entry) => entry,
                    Err(err) => {
                        let _ = reply.send(Err(session_lookup_error(err, &session)));
                        continue;
                    }
                };
                // cancelled while it waited in the queue: never start it
                if turn_cancel.is_cancelled() {
                    let _ = reply.send(Ok("cancelled".into()));
                    continue;
                }
                if in_flight_prompts.borrow().contains_key(&session) {
                    let err = AcpErr::InvalidParams(format!("a prompt is already in flight for session `{session}`"))
                        .with_projector(&entry.host);
                    let _ = reply.send(Err(err));
                    continue;
                }
                in_flight_prompts.borrow_mut().insert(session.clone(), prompt_gen);
                struct PromptFlightGuard {
                    session: String,
                    map: Rc<RefCell<HashMap<String, u64>>>,
                }
                impl Drop for PromptFlightGuard {
                    fn drop(&mut self) {
                        self.map.borrow_mut().remove(&self.session);
                    }
                }
                let flight_guard = PromptFlightGuard { session: session.clone(), map: Rc::clone(&in_flight_prompts) };
                let host = entry.host.clone();
                let forwarders_for_prompt = forwarders.clone();
                let session_for_prompt = session.clone();
                let prev_abort = pending_aborts.borrow_mut().remove(&session);
                tokio::task::spawn_local(async move {
                    let _flight_guard = flight_guard;
                    if let Some(abort_handle) = prev_abort {
                        let _ = abort_handle.await;
                    }
                    if turn_cancel.is_cancelled() {
                        let _ = reply.send(Ok("cancelled".into()));
                        return;
                    }
                    // refused before anything is queued or saved
                    if !images.is_empty() {
                        let refusal = tokio::select! {
                            biased;
                            _ = turn_cancel.cancelled() => {
                                let _ = reply.send(Ok("cancelled".into()));
                                return;
                            }
                            res = image_refusal(&host) => res,
                        };
                        if let Err(err) = refusal {
                            let _ = reply.send(Err(err.with_projector(&host)));
                            return;
                        }
                    }
                    if turn_cancel.is_cancelled() {
                        let _ = reply.send(Ok("cancelled".into()));
                        return;
                    }
                    tokio::select! {
                        biased;
                        _ = turn_cancel.cancelled() => {
                            let _ = reply.send(Ok("cancelled".into()));
                            return;
                        }
                        () = forwarders_for_prompt.begin_turn(&session_for_prompt) => {}
                    }
                    if turn_cancel.is_cancelled() {
                        let _ = reply.send(Ok("cancelled".into()));
                        return;
                    }
                    // Route advertised executable slash commands through the shared `commands.run`
                    // service rather than treating them as plain chat prompts.
                    let slash_cmd = if images.is_empty() {
                        parse_slash_command(&text).map(|(name, _)| name.to_string())
                    } else {
                        None
                    };
                    let is_executable_cmd = match slash_cmd.as_deref() {
                        Some(name) => available_commands(&host).await.iter().any(|c| c.name == name),
                        None => false,
                    };
                    if is_executable_cmd {
                        let projector = mare_shared::wire_projection::hook(host.lua());
                        let cmd_input = if content.is_empty() { json!({}) } else { json!({ "content": content }) };
                        let stop = match call_service(&host, "commands", "run", vec![json!(text), cmd_input]).await {
                            Ok(val) => {
                                if let Some(reply_text) = command_result_text(&val) {
                                    let _ = send_update(
                                        &conn,
                                        &session_for_prompt,
                                        json!({
                                            "sessionUpdate": "agent_message_chunk",
                                            "content": { "type": "text", "text": reply_text },
                                        }),
                                        projector.as_slice(),
                                    );
                                }
                                // Yield once so any `ctx:spawn` prompt started by a command (e.g. a
                                // skill or prompt template) can claim its turn before checking.
                                tokio::task::yield_now().await;
                                if let Ok(act) = call_service(&host, "agent", "activity", vec![]).await
                                    && act.get("busy").and_then(J::as_bool) == Some(true)
                                    && let Some(turn_id) = act.get("turn_id").and_then(J::as_str)
                                {
                                    let res = wait_for_started_turn(&host, turn_id, &turn_cancel).await;
                                    map_turn_result(&res)
                                } else {
                                    Ok("end_turn".into())
                                }
                            }
                            Err(err) => Err(map_command_run_error(err)),
                        };
                        let stop = match forwarders_for_prompt.flush(&session_for_prompt, PROMPT_DRAIN_TIMEOUT).await {
                            Ok(()) => stop,
                            Err(e) => Err(AcpErr::Internal(e)),
                        };
                        let _ = reply.send(stop.map_err(|e| e.with_projector(&host)));
                        return;
                    }
                    // Open a lightweight lifecycle cursor before calling host.prompt so that if
                    // another turn (e.g. a background wakeup) is busy and our prompt is queued,
                    // we cannot miss its consumption, queue removal, or preceding-turn failure.
                    let mut watch = host.kernel.open_cursor(std::sync::Arc::new(|name| {
                        matches!(name, "item/completed" | "queue/changed" | "turn/completed").then_some(true)
                    }));
                    let mut prompt_opts = json!({ "source": "acp", "wait": false });
                    if !images.is_empty() {
                        prompt_opts["images"] = json!(images);
                    }
                    if !content.is_empty() {
                        prompt_opts["content"] = json!(content);
                    }
                    // Split submission (`wait: false`) from waiting so any `session/cancel` that
                    // arrives while `agent.prompt` is still preparing or claiming the turn is
                    // reconciled against the returned `turn_id` / `input_id` instead of firing
                    // `agent.cancel` on an idle host before the turn exists.
                    let accepted = host.prompt(&text, prompt_opts).await;
                    let res = match accepted {
                        Ok(v) if v["status"] == "queued" => {
                            if let Some(input_id) = v["input_id"].as_str() {
                                wait_for_queued_input(&host, &mut watch, input_id, &turn_cancel).await
                            } else {
                                Err(anyhow::anyhow!("queued prompt returned no input_id"))
                            }
                        }
                        Ok(v) if (v["status"] == "started" || v["status"].is_null()) && v["turn_id"].is_string() => {
                            let turn_id = v["turn_id"].as_str().unwrap();
                            wait_for_started_turn(&host, turn_id, &turn_cancel).await
                        }
                        other => other,
                    };
                    drop(watch);
                    let stop = map_turn_result(&res);
                    // Flush all updates queued on the session's persistent forwarder before replying.
                    let stop = match forwarders_for_prompt.flush(&session_for_prompt, PROMPT_DRAIN_TIMEOUT).await {
                        Ok(()) => stop,
                        Err(e) => Err(AcpErr::Internal(e)),
                    };
                    let _ = reply.send(stop.map_err(|e| e.with_projector(&host)));
                });
            }
        }
    }
    forwarders.close_all();
    manager.shutdown_all().await;
    if terminated.is_cancelled() {
        // the protocol thread may sit in a stdin read the client never ends; the exit ends it
        return Ok(terminate_code.get());
    }
    let _ = proto.join();
    Ok(0)
}

/// Exit status after a termination request (128 + SIGTERM), as the forced exit reports it.
const TERMINATED_EXIT_CODE: i32 = 143;
/// Exit status after an interrupt request (128 + SIGINT), as the forced exit reports it.
const INTERRUPTED_EXIT_CODE: i32 = 130;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additional_directories_error_mapping_uses_structured_codes() {
        for (code, message, expect_invalid_params) in [
            ("invalid_arguments", "not a directory", true),
            ("denied", "sandbox ceiling rejected", true),
            (mare_binding::ossandbox::SANDBOX_INVALID_ROOT, "symlink rejected", true),
            ("session.persist_failed", "permission denied writing journal", false),
            ("internal", "unexpected invalid state", false),
        ] {
            let err = anyhow::Error::new(mare_kernel::ServiceCallError::Coded(mare_kernel::ErrorValue {
                code: code.to_owned(),
                message: message.to_owned(),
                data: None,
            }));
            let mapped = map_additional_directories_error(err);
            assert_eq!(
                matches!(mapped, AcpErr::InvalidParams(_)),
                expect_invalid_params,
                "unexpected mapping for code={code}"
            );
        }
        let uncoded = anyhow::anyhow!("session write failed: permission denied");
        assert!(matches!(map_additional_directories_error(uncoded), AcpErr::Internal(_)));
    }

    #[test]
    fn session_trust_does_not_extend_cli_trust_to_other_workspaces() {
        let base_ws = tempfile::tempdir().unwrap();
        let other_ws = tempfile::tempdir().unwrap();
        let mut opts = HostOptions {
            cwd: base_ws.path().to_path_buf(),
            mode: Mode::Acp,
            config: J::Null,
            load_init: true,
            project_trusted: true,
            args: json!({ "trust": "trusted", "trust_source": "cli" }),
            no_user_plugins: false,
        };
        assert!(session_trust(&opts, base_ws.path()));
        assert!(!session_trust(&opts, other_ws.path()));

        let args_other = session_args(&opts, other_ws.path(), false, None);
        assert_ne!(args_other.get("trust_source").and_then(|v| v.as_str()), Some("cli"));
        assert_eq!(args_other.get("trust").and_then(|v| v.as_str()), Some("untrusted"));

        opts.args = json!({ "trust": "untrusted", "trust_source": "cli" });
        assert!(!session_trust(&opts, base_ws.path()));
        assert!(!session_trust(&opts, other_ws.path()));
    }

    #[test]
    fn session_args_preserves_undecided_and_pending_trust_sources() {
        let base_ws = tempfile::tempdir().unwrap();
        let other_ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other_ws.path().join(".mare/plugins")).unwrap();

        for (source, expect_pending) in [("undecided", false), ("pending", true)] {
            let opts = HostOptions {
                cwd: base_ws.path().to_path_buf(),
                mode: Mode::Acp,
                config: J::Null,
                load_init: true,
                project_trusted: false,
                args: json!({ "trust": "undecided", "trust_source": source, "trust_pending": expect_pending }),
                no_user_plugins: false,
            };
            let same = session_args(&opts, base_ws.path(), false, None);
            assert_eq!(same.get("trust_source").and_then(|v| v.as_str()), Some(source));
            assert_eq!(same.get("trust").and_then(|v| v.as_str()), Some("undecided"));
            assert_eq!(same.get("trust_pending").and_then(|v| v.as_bool()), Some(expect_pending));

            let other = session_args(&opts, other_ws.path(), false, None);
            assert_eq!(other.get("trust_source").and_then(|v| v.as_str()), Some("undecided"));
            assert_eq!(other.get("trust").and_then(|v| v.as_str()), Some("undecided"));
        }
    }
}
