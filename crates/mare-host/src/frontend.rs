//! What frontends call: services, prompt / steer / abort / submit, events and the state snapshot.

use anyhow::{Context, Result};
use mlua::{Function, MultiValue, Value};

use crate::Host;

pub(crate) const AGENT_SERVICE: &str = mare_shared::AGENT_SERVICE_NAME;
pub(crate) const SESSION_SERVICE: &str = mare_shared::SESSION_SERVICE_NAME;
pub(crate) const TOOLS: &str = mare_shared::TOOLS_SERVICE_NAME;

fn submit_error(message: impl Into<String>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("ok".into(), serde_json::Value::Bool(false));
    map.insert(mare_shared::MESSAGE_KEY.into(), serde_json::Value::String(message.into()));
    serde_json::Value::Object(map)
}
const MAX_POLICY_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Failure reading a local image path under the host's permissions & sandbox policy.
#[derive(Debug)]
pub enum PolicyImageError {
    InvalidArguments(String),
    Internal(String),
}

#[derive(serde::Deserialize)]
struct AgentStatusReply {
    status: Option<String>,
    turn_id: Option<String>,
}

#[derive(serde::Serialize)]
struct ContinueStatusReply {
    status: &'static str,
    error: mare_protocol::events::EventError,
}

impl Host {
    fn resolve_service_call(&self, service: &str, method: &str, args: MultiValue) -> Result<(Function, MultiValue)> {
        // a mare.Service, called on the service itself: its caller is the traced ctx
        let Some(Value::Table(t)) = self.kernel.service(service) else {
            anyhow::bail!("service `{service}` is not available");
        };
        let f: Function = t.get(method).with_context(|| format!("{service}:{method}"))?;
        let mut a = MultiValue::new();
        a.push_back(Value::Table(t));
        a.extend(args);
        Ok((f, a))
    }

    /// Call `service:method(...)` with the root ctx as caller.
    pub async fn call_service(&self, service: &str, method: &str, args: MultiValue) -> Result<MultiValue> {
        if (service == AGENT_SERVICE
            && matches!(
                method,
                "prompt" | "continue" | "steer" | "update_queued" | "delete_queued" | "append" | "notify"
            ))
            || (service == SESSION_SERVICE && matches!(method, "create" | "load" | "fork" | "record"))
            || service == TOOLS
        {
            mare_binding::exec_gate::check_unpoisoned(self.lua())?;
        }
        let (f, a) = self.resolve_service_call(service, method, args)?;
        match f.call_async::<MultiValue>(a).await {
            Ok(values) => Ok(values),
            Err(err) => {
                if let Some(msg) = mare_shared::lua_error::recall_code_message(self.lua(), &err) {
                    anyhow::bail!("{msg}");
                }
                Err(err.into())
            }
        }
    }

    async fn call_agent(&self, method: &str, args: serde_json::Value) -> Result<serde_json::Value> {
        self.call_agent_with_cancel(method, args, None).await
    }

    async fn call_agent_with_cancel(
        &self,
        method: &str,
        mut args: serde_json::Value,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<serde_json::Value> {
        if matches!(method, "prompt" | "continue")
            && self.kernel.service_method(AGENT_SERVICE, method).is_none()
            && let Some(reason) = self.blocked_reason()
        {
            return Ok(serde_json::json!({ "text": "", "error": reason, "not_sent": true }));
        }
        if let Some(obj) = args.as_object_mut()
            && let Some(schema) = obj.get("output_schema").filter(|s| !s.is_null())
        {
            let _ = obj.insert("output_schema".to_string(), serde_json::to_value(schema)?);
        }
        let owner = self.kernel.host_owner()?;
        let token = cancel.unwrap_or_default();
        self.kernel
            .call_service_export(owner, None, AGENT_SERVICE, method, args, token)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn call_agent_routed(
        &self,
        method: &str,
        input: Option<&str>,
        opts: serde_json::Value,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<serde_json::Value> {
        if matches!(method, "prompt" | "continue")
            && self.kernel.service_method(AGENT_SERVICE, method).is_none()
            && let Some(reason) = self.blocked_reason()
        {
            return Ok(serde_json::json!({ "text": "", "error": reason, "not_sent": true }));
        }
        if let Some(ref cancel_token) = cancel {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => anyhow::bail!("cancelled"),
                _ = self.kernel.reload_barrier() => {}
            }
        }
        let mut mv = MultiValue::new();
        if let Some(text) = input {
            mv.push_back(Value::String(self.lua().create_string(text)?));
        }
        let opts_val = if opts.get("messages").is_some() {
            mare_shared::lua::json_to_lua_lossless_bounded(
                self.lua(),
                &opts,
                mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES,
            )?
        } else {
            mare_shared::lua::json_to_lua_lossless(self.lua(), &opts)?
        };
        if let Some(ref cancel_token) = cancel
            && let Value::Table(ref t) = opts_val
        {
            t.set("cancel", self.lua().create_userdata(mare_binding::task::Token(cancel_token.clone()))?)?;
        }
        mv.push_back(opts_val);
        let call = async {
            let reply = self.call_service(AGENT_SERVICE, method, mv).await?;
            let first = reply.into_iter().next().unwrap_or(Value::Nil);
            Ok::<serde_json::Value, anyhow::Error>(mare_shared::lua::lua_to_json(self.lua(), first)?)
        };
        if let Some(cancel_token) = cancel {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => anyhow::bail!("cancelled"),
                res = call => res,
            }
        } else {
            call.await
        }
    }

    async fn wait_if_started(&self, reply: serde_json::Value, wait: bool) -> Result<serde_json::Value> {
        let parsed: Option<AgentStatusReply> = serde_json::from_value(reply.clone()).ok();
        if wait
            && let Some(parsed) = parsed
            && (parsed.status.as_deref() == Some("started") || parsed.status.is_none())
            && let Some(turn_id) = parsed.turn_id
        {
            return self.call_agent("wait", serde_json::json!({ "turn_id": turn_id })).await;
        }
        Ok(reply)
    }

    /// Run one prompt to completion (returns Turn). When `opts.wait == Some(false)` returns accepted immediately.
    pub async fn prompt(&self, text: &str, mut opts: serde_json::Value) -> Result<serde_json::Value> {
        let wait = opts.as_object_mut().and_then(|o| o.remove("wait")).and_then(|v| v.as_bool()).unwrap_or(true);
        let reply = self.call_agent_routed("prompt", Some(text), opts, None).await?;
        self.wait_if_started(reply, wait).await
    }

    /// Continue an interrupted turn from the existing context (no new user message).
    pub async fn continue_turn(&self) -> Result<serde_json::Value> {
        self.continue_turn_with(serde_json::json!({})).await
    }

    /// Continue an interrupted turn from the existing context with prompt options.
    pub async fn continue_turn_with(&self, mut opts: serde_json::Value) -> Result<serde_json::Value> {
        let wait = opts.as_object_mut().and_then(|o| o.remove("wait")).and_then(|v| v.as_bool()).unwrap_or(true);
        let reply_res = self.call_agent_routed("continue", None, opts, None).await;
        let reply = match reply_res {
            Ok(reply) => reply,
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("no_pending_work") || msg.contains("nothing to continue") {
                    return Ok(serde_json::to_value(ContinueStatusReply {
                        status: "no_pending_work",
                        error: mare_protocol::events::EventError {
                            code: "agent.no_pending_work".into(),
                            message: "nothing to continue".into(),
                            data: None,
                        },
                    })?);
                }
                return Err(err);
            }
        };
        if serde_json::from_value::<AgentStatusReply>(reply.clone()).ok().and_then(|r| r.status).as_deref()
            == Some("no_pending_work")
        {
            return Ok(serde_json::to_value(ContinueStatusReply {
                status: "no_pending_work",
                error: mare_protocol::events::EventError {
                    code: "agent.no_pending_work".into(),
                    message: "nothing to continue".into(),
                    data: None,
                },
            })?);
        }
        self.wait_if_started(reply, wait).await
    }

    pub async fn steer(&self, text: &str) -> Result<()> {
        self.steer_with(text, serde_json::json!({})).await
    }

    /// Queue a steering message with explicit options (e.g. `source`).
    pub async fn steer_with(&self, text: &str, opts: serde_json::Value) -> Result<()> {
        self.call_agent_routed("steer", Some(text), opts, None).await.map(|_| ())
    }

    /// `steer_with`, abandoned when `cancel` fires.
    pub async fn steer_with_cancel(
        &self,
        text: &str,
        opts: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        self.call_agent_routed("steer", Some(text), opts, Some(cancel)).await.map(|_| ())
    }

    pub async fn follow_up(&self, text: &str) -> Result<()> {
        self.follow_up_with(text, serde_json::json!({})).await
    }

    /// Queue a follow-up via `agent.prompt` (queues when busy).
    pub async fn follow_up_with(&self, text: &str, mut opts: serde_json::Value) -> Result<()> {
        if let Some(obj) = opts.as_object_mut() {
            obj.insert("when_busy".into(), serde_json::Value::String("queue".to_string()));
        }
        self.call_agent_routed("prompt", Some(text), opts, None).await.map(|_| ())
    }

    pub async fn follow_up_with_cancel(
        &self,
        text: &str,
        mut opts: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        if let Some(obj) = opts.as_object_mut() {
            obj.insert("when_busy".into(), serde_json::Value::String("queue".to_string()));
        }
        self.call_agent_routed("prompt", Some(text), opts, Some(cancel)).await.map(|_| ())
    }

    pub async fn abort(&self) -> Result<()> {
        self.abort_with_inputs().await.map(|_| ())
    }

    /// Cancel a child turn with the causal chain supplied by its owning agent tree.
    pub(crate) async fn abort_with_cause(&self, cause: &serde_json::Value) -> Result<()> {
        crate::startup::request_gate::purge_cancelled_leases(self.lua());
        let reason = format!("mare.cancel-cause:{cause}");
        let res = self.call_agent("cancel", serde_json::json!({ "clear_queue": true, "reason": reason })).await;
        tokio::task::yield_now().await;
        crate::startup::request_gate::purge_cancelled_leases(self.lua());
        res.map(|_| ())
    }

    /// Return the complete durable inputs removed by cancellation.
    pub async fn abort_with_inputs(&self) -> Result<serde_json::Value> {
        self.abort_with_inputs_for(None).await
    }

    /// Cancel the current turn via `agent.cancel` with `clear_queue = true`.
    pub async fn abort_with_inputs_for(&self, request_id: Option<&str>) -> Result<serde_json::Value> {
        crate::startup::request_gate::purge_cancelled_leases(self.lua());
        let mut args = serde_json::json!({ "clear_queue": true });
        if let Some(rid) = request_id.filter(|r| !r.is_empty()) {
            args["request_id"] = serde_json::Value::String(rid.to_string());
        }
        let res = self.call_agent("cancel", args).await;
        tokio::task::yield_now().await;
        crate::startup::request_gate::purge_cancelled_leases(self.lua());
        res
    }

    /// Fetch a session snapshot via the host's `session` service (`session:get(session_id)`).
    pub async fn session_snapshot(&self, session_id: &str) -> Result<serde_json::Value> {
        let arg = mare_shared::lua::json_to_lua(self.lua(), &serde_json::Value::String(session_id.to_owned()))?;
        let vals = self.call_service(SESSION_SERVICE, "get", MultiValue::from_vec(vec![arg])).await?;
        let first = vals.into_iter().next().unwrap_or(Value::Nil);
        Ok(mare_shared::lua::lua_to_json(self.lua(), first)?)
    }

    /// Fetch the active session summary as JSON via the host's `session` service (`session:info()`).
    pub async fn session_info_json(&self) -> Result<serde_json::Value> {
        let vals = self.call_service(SESSION_SERVICE, "info", MultiValue::new()).await?;
        let first = vals.into_iter().next().unwrap_or(Value::Nil);
        Ok(mare_shared::lua::lua_to_json(self.lua(), first)?)
    }

    /// Invoke an input submission method (`prompt` / `steer` / `follow_up`) on the host's `agent`
    /// service with structured JSON input and options.
    pub async fn invoke_agent_submit(
        &self,
        method: &str,
        input: serde_json::Value,
        opts: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let lua = self.lua();
        let mut mv = MultiValue::new();
        mv.push_back(mare_shared::lua::json_to_lua(lua, &input)?);
        mv.push_back(mare_shared::lua::json_to_lua(lua, &opts)?);
        let vals = self.call_service(AGENT_SERVICE, method, mv).await?;
        Ok(match vals.into_iter().next() {
            Some(Value::Nil) | None => serde_json::Value::Null,
            Some(v) => mare_shared::lua::lua_to_json(lua, v)?,
        })
    }

    /// Read and encode a local image file after adjudicating read access through the active
    /// `permissions` service's `tool/before` policy voter and OS sandbox ceiling.
    pub async fn read_policy_image(
        &self,
        raw_path: &str,
    ) -> std::result::Result<mare_binding::image::Encoded, PolicyImageError> {
        let unresolved = mare_binding::fs::resolve_path(&self.cwd, raw_path);
        let path = mare_binding::fs::normalize_lexical(&unresolved);
        let policy = check_permissions_read(self, &path).await?;
        mare_binding::fs::check_policy(policy.as_ref(), "read", &path, &self.cwd)
            .map_err(PolicyImageError::InvalidArguments)?;

        let cwd = self.cwd.clone();
        let path_for_open = unresolved.clone();
        let file = tokio::task::spawn_blocking(move || mare_binding::fs::open_no_follow(&path_for_open, Some(&cwd)))
            .await
            .map_err(|e| PolicyImageError::Internal(e.to_string()))?
            .map_err(|e| PolicyImageError::InvalidArguments(format!("cannot open image `{raw_path}`: {e}")))?;

        let opened = mare_binding::fs::opened_path(&file).map_err(|e| {
            PolicyImageError::InvalidArguments(format!("cannot verify opened image path `{raw_path}`: {e}"))
        })?;
        if !same_opened_path(&opened, &path) {
            return Err(PolicyImageError::InvalidArguments(format!(
                "image path `{raw_path}` resolved to unexpected target `{}`",
                opened.display()
            )));
        }
        mare_binding::fs::check_policy(policy.as_ref(), "read", &opened, &self.cwd)
            .map_err(PolicyImageError::InvalidArguments)?;
        let reopened = mare_binding::fs::opened_path(&file).map_err(|e| {
            PolicyImageError::InvalidArguments(format!("cannot re-verify opened image path `{raw_path}`: {e}"))
        })?;
        if !same_opened_path(&reopened, &opened) {
            return Err(PolicyImageError::InvalidArguments(format!(
                "image path `{raw_path}` changed during policy check"
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let handle_meta = file.metadata().map_err(|e| {
                PolicyImageError::InvalidArguments(format!("cannot stat opened image `{raw_path}`: {e}"))
            })?;
            let path_meta = std::fs::symlink_metadata(&opened).map_err(|e| {
                PolicyImageError::InvalidArguments(format!("image path `{raw_path}` changed on disk: {e}"))
            })?;
            if handle_meta.dev() != path_meta.dev() || handle_meta.ino() != path_meta.ino() {
                return Err(PolicyImageError::InvalidArguments(format!(
                    "image path `{raw_path}` identity changed during policy check"
                )));
            }
        }

        let shown = raw_path.to_string();
        let bytes = tokio::task::spawn_blocking(move || {
            mare_binding::fs::read_file_capped(file, Some(MAX_POLICY_IMAGE_BYTES), &shown)
        })
        .await
        .map_err(|e| PolicyImageError::Internal(e.to_string()))?
        .map_err(|e| PolicyImageError::InvalidArguments(e.to_string()))?;
        mare_binding::image::load_bytes(&bytes, 2000).map_err(|e| PolicyImageError::Internal(e.to_string()))
    }

    /// Abort only the named turn via `agent.cancel`.
    pub async fn abort_turn(&self, turn_id: &str) -> Result<()> {
        self.call_agent("cancel", serde_json::json!({ "turn_id": turn_id, "clear_queue": false })).await.map(|_| ())
    }

    pub async fn is_busy(&self) -> bool {
        matches!(
            self.call_agent("get", serde_json::json!({}))
                .await
                .ok()
                .and_then(|v| v.get("busy").and_then(|b| b.as_bool())),
            Some(true)
        )
    }

    /// User input from an interactive frontend (commands, steer, prompt...). Handled by plugins.
    pub async fn submit(&self, text: &str) {
        self.submit_with(text, vec![]).await
    }

    /// Submit with image attachments (`{type="image", data, mime}` blocks).
    pub async fn submit_with(&self, text: &str, images: Vec<serde_json::Value>) {
        self.submit_draft(text, images, vec![]).await
    }

    /// Submit a draft with exact attachment marker identities kept separate from model blocks.
    pub async fn submit_draft(
        &self,
        text: &str,
        images: Vec<serde_json::Value>,
        image_markers: Vec<serde_json::Value>,
    ) {
        let _ = self.submit_draft_outcome(text, images, image_markers).await;
    }

    /// Submit a slash command and return the settled outcome (`{ ok, message?, result? }`).
    pub async fn submit_command(&self, text: &str) -> serde_json::Value {
        self.submit_draft_outcome(text, vec![], vec![]).await
    }

    async fn submit_draft_outcome(
        &self,
        text: &str,
        images: Vec<serde_json::Value>,
        image_markers: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        if let Some(reason) = mare_binding::exec_gate::poison_reason(self.lua())
            && !matches!(text.trim(), "/quit" | "/exit" | "/status")
        {
            self.kernel.emit_json("host/notice", serde_json::json!({"level": "error", "text": reason})).await;
            return submit_error(reason);
        }
        // a typed command waits out a reload in progress (not /quit, /exit, /status); typed text
        // is a prompt, which a reload in progress refuses
        let command = text.trim_start().starts_with('/');
        if command && !matches!(text.trim(), "/quit" | "/exit" | "/status") {
            self.kernel.reload_barrier().await;
        }
        let t = match self.lua().create_table() {
            Ok(t) => t,
            Err(e) => return submit_error(e.to_string()),
        };
        let _ = t.set("text", text);
        if !images.is_empty()
            && let Ok(v) = mare_shared::lua::json_to_lua(self.lua(), &serde_json::Value::Array(images))
        {
            let _ = t.set(mare_shared::IMAGES_KEY, v);
        }
        if let Ok(markers) = mare_shared::lua::json_to_lua(self.lua(), &serde_json::Value::Array(image_markers)) {
            let _ = t.set("image_markers", markers);
        }
        self.kernel.emit("input/submit", MultiValue::from_vec(vec![Value::Table(t.clone())])).await;
        if !t.get::<bool>("handled").unwrap_or(false) {
            return submit_error(format!("unhandled command: {text}"));
        }
        if let Ok(v) = t.get::<Value>("result")
            && !v.is_nil()
            && let Ok(json) = mare_shared::lua::lua_to_json(self.lua(), v)
        {
            return json;
        }
        serde_json::json!({ "ok": true })
    }

    pub async fn emit_json(&self, event: &str, payload: serde_json::Value) {
        self.kernel.emit_json(event, payload).await
    }

    pub async fn bail_json(&self, event: &str, payload: serde_json::Value) -> Option<serde_json::Value> {
        self.kernel.bail_json(event, payload).await
    }

    /// Snapshot of agent state for internal callers and `agent.get`.
    pub async fn state(&self) -> serde_json::Value {
        self.call_agent("get", serde_json::json!({}))
            .await
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }
}

fn effective_verdict_policy(
    host: &Host,
    verdict: &mlua::Table,
) -> std::result::Result<Option<mare_binding::ossandbox::Policy>, PolicyImageError> {
    let lua = host.lua();
    let grant_val: Value = verdict.get("sandbox_grant").unwrap_or(Value::Nil);
    let mut policy = if !grant_val.is_nil() {
        mare_binding::ossandbox::Policy::from_lua(lua, &grant_val, &host.cwd).map_err(|e| {
            PolicyImageError::InvalidArguments(format!("image path attachment denied: invalid sandbox_grant: {e}"))
        })?
    } else {
        mare_binding::ossandbox::current(lua)
    };
    if let Ok(Some(sandboxes)) = verdict.get::<Option<mlua::Table>>("sandboxes") {
        for pair in sandboxes.sequence_values::<Value>() {
            let v = pair.map_err(|e| {
                PolicyImageError::InvalidArguments(format!("image path attachment denied: invalid sandbox: {e}"))
            })?;
            if v.is_nil() {
                continue;
            }
            let constraint = mare_binding::ossandbox::Policy::from_lua(lua, &v, &host.cwd).map_err(|e| {
                PolicyImageError::InvalidArguments(format!("image path attachment denied: invalid sandbox: {e}"))
            })?;
            policy = match (policy, constraint) {
                (Some(a), Some(b)) => Some(a.intersect(&b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
        }
    }
    let ceilinged = mare_binding::ossandbox::enforce_ceiling(lua, policy);
    Ok(mare_binding::deny_read::keep(lua, ceilinged))
}

async fn check_permissions_read(
    host: &Host,
    path: &std::path::Path,
) -> std::result::Result<Option<mare_binding::ossandbox::Policy>, PolicyImageError> {
    let Some((provider_plugin, _)) = host.kernel.service_provider(mare_shared::PERMISSIONS_SERVICE_NAME) else {
        return Err(PolicyImageError::InvalidArguments(
            "image path attachment denied: permissions policy cannot adjudicate read access".into(),
        ));
    };
    // a voter of tool/before adjudicates the read: a plain observer of it does not
    if host.kernel.voter_count("tool/before") == 0 {
        return Err(PolicyImageError::InvalidArguments(
            "image path attachment denied: permissions policy cannot adjudicate read access".into(),
        ));
    }
    let lua = host.lua();
    let lua_err = |e: mlua::Error| PolicyImageError::Internal(e.to_string());
    let args = lua.create_table().map_err(lua_err)?;
    args.set("path", path.to_string_lossy().into_owned()).map_err(lua_err)?;
    let call = lua.create_table().map_err(lua_err)?;
    call.set("id", "rpc-image-attachment").map_err(lua_err)?;
    call.set("name", "read").map_err(lua_err)?;
    call.set("arguments", args).map_err(lua_err)?;
    call.set("cwd", host.cwd.to_string_lossy().into_owned()).map_err(lua_err)?;

    let info = lua.create_table().map_err(lua_err)?;
    info.set("side_effect", "read").map_err(lua_err)?;
    info.set("read_only", true).map_err(lua_err)?;
    info.set("unconfined", false).map_err(lua_err)?;

    let verdict = host
        .kernel
        .policy("tool/before", MultiValue::from_vec(vec![Value::Table(call), Value::Table(info)]))
        .await
        .map_err(|e| PolicyImageError::InvalidArguments(format!("image path attachment denied: {e}")))?;
    let decision: Option<String> = verdict.get("decision").ok();
    if decision.as_deref() != Some("allow") {
        let reason: Option<String> = verdict.get("reason").ok();
        return Err(PolicyImageError::InvalidArguments(
            reason.unwrap_or_else(|| "image path attachment denied by permissions policy".into()),
        ));
    }
    let approved_by_provider = verdict
        .get::<mlua::Table>("approved_by")
        .ok()
        .map(|t| t.sequence_values::<String>().any(|r| r.as_deref().ok() == Some(provider_plugin.as_str())))
        .unwrap_or(false);
    if !approved_by_provider {
        return Err(PolicyImageError::InvalidArguments(
            "image path attachment denied: permissions provider did not adjudicate read access".into(),
        ));
    }
    effective_verdict_policy(host, &verdict)
}

fn same_opened_path(opened: &std::path::Path, expected: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        dunce::simplified(opened).as_os_str().eq_ignore_ascii_case(dunce::simplified(expected).as_os_str())
    }
    #[cfg(target_os = "macos")]
    {
        if opened == expected {
            return true;
        }
        for alias in ["var", "tmp", "etc"] {
            if let Ok(rest) = expected.strip_prefix(std::path::Path::new("/").join(alias)) {
                return opened == std::path::Path::new("/private").join(alias).join(rest);
            }
        }
        false
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        opened == expected
    }
}
