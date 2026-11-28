use super::*;

pub(super) type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;
struct GuardedStream {
    inner: ByteStream,
    _guard: Option<ShimGuard>,
}

impl Stream for GuardedStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// The decoder plus the SSE last event ID buffer.
pub(super) struct SseStream {
    blocks: sse::Blocks<ByteStream>,
    last_id: String,
}

/// Error prefixes the `llm` classifier keys on: `transport:` = any failure
/// sending or reading (retryable, fresh connection); `malformed_response:` = a stream that can't
/// be parsed (typed protocols don't retry); `aborted` = the caller cancelled (never wrapped).
pub(super) const TRANSPORT: &str = "transport: ";
const MALFORMED: &str = "malformed_response: ";

/// SSE input must be UTF-8. The SSE decoder (crate::sse) only validates a line once it is
/// complete, so bytes that can never become valid (a sequence the decoder rejects, or a pending
/// remainder longer than any UTF-8 sequence) are caught here as they arrive, comments included:
/// stop the stream and flag a `malformed_response` instead.
pub(super) fn utf8_guarded(s: ByteStream, bad: std::sync::Arc<std::sync::atomic::AtomicBool>) -> ByteStream {
    let mut pending: Vec<u8> = Vec::new();
    Box::pin(s.scan((), move |_, item| {
        let out = match item {
            Ok(b) => {
                pending.extend_from_slice(&b);
                match std::str::from_utf8(&pending) {
                    Ok(_) => {
                        pending.clear();
                        Some(Ok(b))
                    }
                    Err(e) if e.error_len().is_some() || pending.len() - e.valid_up_to() > 4 => {
                        bad.store(true, std::sync::atomic::Ordering::SeqCst);
                        None
                    }
                    Err(e) => {
                        pending.drain(..e.valid_up_to());
                        Some(Ok(b))
                    }
                }
            }
            Err(e) => Some(Err(e)),
        };
        std::future::ready(out)
    }))
}

/// The SSE decoding pipeline over a raw body: the UTF-8 guard, then mare's decoder. Comment
/// lines (heartbeats) stamp the idle watchdog too, although their bytes already did.
fn sse_pipeline(
    body: ByteStream,
    caps: SseCaps,
    bad: std::sync::Arc<std::sync::atomic::AtomicBool>,
    last: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
) -> SseStream {
    let on_comment = Box::new(move |_: &[u8]| {
        if let Ok(mut l) = last.lock() {
            *l = std::time::Instant::now();
        }
    });
    SseStream { blocks: sse::Blocks::new(utf8_guarded(body, bad), caps, on_comment), last_id: String::new() }
}

/// Wrap a body stream so every chunk (heartbeat comments included) stamps `last`.
fn stamped(s: ByteStream, last: std::sync::Arc<std::sync::Mutex<std::time::Instant>>) -> ByteStream {
    Box::pin(s.inspect(move |_| {
        if let Ok(mut l) = last.lock() {
            *l = std::time::Instant::now();
        }
    }))
}

/// `next()` on a stream under the cancel/close token, overall request deadline, and idle watchdog:
/// wakes on cancel/`close()`, when the deadline passes, or when the idle period since the last
/// *byte* (not the last parsed event) has passed.
async fn next_watched<S: Stream + Unpin>(this: &Response, s: &mut S) -> mlua::Result<Option<S::Item>> {
    if this.close_token.is_cancelled() {
        return Err(rt_err("aborted"));
    }
    if this.idle.is_none() && this.deadline.is_none() {
        return tokio::select! {
            biased;
            _ = this.close_token.cancelled() => Err(rt_err("aborted")),
            v = s.next() => Ok(v),
        };
    }
    let mut next = s.next();
    loop {
        if this.close_token.is_cancelled() {
            return Err(rt_err("aborted"));
        }
        let now = tokio::time::Instant::now();
        let idle_deadline = this.idle.map(|idle| {
            this.last_activity.lock().map(|last| tokio::time::Instant::from_std(*last)).unwrap_or(now) + idle
        });
        let (next_deadline, idle_timeout) = match (idle_deadline, this.deadline) {
            (Some(idle), Some(deadline)) if deadline < idle => (deadline, None),
            (Some(idle), _) => (idle, this.idle),
            (None, Some(deadline)) => (deadline, None),
            (None, None) => unreachable!("checked above"),
        };
        if next_deadline <= now {
            return if let Some(idle) = idle_timeout {
                Err(rt_err(format!("{TRANSPORT}stream idle for {}s", idle.as_secs())))
            } else {
                Err(rt_err(format!("{TRANSPORT}request timed out")))
            };
        }
        let wait = tokio::time::sleep_until(next_deadline);
        tokio::select! {
            biased;
            _ = this.close_token.cancelled() => return Err(rt_err("aborted")),
            v = &mut next => return Ok(v),
            _ = wait => {}
        }
    }
}

/// The whole remaining body, under the cancel token and the idle watchdog.
async fn read_all(this: &Response) -> mlua::Result<Vec<u8>> {
    let mut g = this.body.lock().await;
    let mut out = Vec::new();
    if let Some(mut s) = g.take() {
        while let Some(c) = next_watched(this, &mut s).await? {
            let b = c.map_err(|e| {
                let safe = mare_shared::redact::redact_str(&e.to_string());
                if e.is_timeout() {
                    rt_err(format!("{TRANSPORT}request timed out: {safe}"))
                } else {
                    rt_err(format!("{TRANSPORT}{safe}"))
                }
            })?;
            out.extend_from_slice(&b);
        }
    }
    Ok(out)
}

fn headers_table(lua: &Lua, h: &[(String, String)]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in h {
        t.set(k.as_str(), v.as_str())?;
    }
    Ok(t)
}

impl UserData for Response {
    fn add_fields<F: mlua::UserDataFields<Self>>(f: &mut F) {
        f.add_field_method_get("status", |_, this| Ok(this.status));
        f.add_field_method_get("url", |_, this| Ok(this.url.clone()));
        f.add_field_method_get("ok", |_, this| Ok((200..300).contains(&this.status)));
        f.add_field_method_get("headers", |lua, this| headers_table(lua, &this.headers));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // read() -> next body chunk | nil
        m.add_async_method("read", |lua, this, ()| async move {
            let mut g = this.body.lock().await;
            let Some(s) = g.as_mut() else { return Ok(Value::Nil) };
            match next_watched(&this, s).await {
                Ok(Some(Ok(b))) => Ok(Value::String(lua.create_string(&b)?)),
                Ok(Some(Err(e))) => {
                    g.take();
                    let safe = mare_shared::redact::redact_str(&e.to_string());
                    if e.is_timeout() {
                        Err(rt_err(format!("{TRANSPORT}request timed out: {safe}")))
                    } else {
                        Err(rt_err(format!("{TRANSPORT}{safe}")))
                    }
                }
                Ok(None) => {
                    g.take();
                    Ok(Value::Nil)
                }
                Err(e) => {
                    g.take();
                    Err(e)
                }
            }
        });
        // text() -> whole remaining body. Same idle watchdog and `transport:` prefix as read(), so
        // a reset or a stalled error body (llm.http_error reads it) is retryable, not a hang.
        m.add_async_method("text", |lua, this, ()| async move {
            let out = read_all(&this).await?;
            lua.create_string(&out)
        });
        m.add_async_method("json", |lua, this, ()| async move {
            let out = read_all(&this).await?;
            let v: serde_json::Value = serde_json::from_slice(&out).map_err(|e| {
                rt_err(format!("{MALFORMED}invalid JSON body: {}", mare_shared::redact::redact_str(&e.to_string())))
            })?;
            json_to_lua(&lua, &v)
        });
        // next_event() -> {event, data, id} | nil  (SSE)
        m.add_async_method("next_event", |lua, this, ()| async move {
            let mut sg = this.sse.lock().await;
            if sg.is_none() {
                let body = this.body.lock().await.take();
                if let Some(b) = body {
                    *sg = Some(sse_pipeline(b, this.caps, this.bad_utf8.clone(), this.last_activity.clone()));
                } else {
                    return Ok(Value::Nil);
                }
            }
            let s = sg.as_mut().unwrap();
            loop {
                let next = match next_watched(&this, &mut s.blocks).await {
                    Ok(v) => v,
                    Err(e) => {
                        sg.take();
                        return Err(e);
                    }
                };
                if this.bad_utf8.load(std::sync::atomic::Ordering::SeqCst) {
                    sg.take();
                    return Err(rt_err(format!("{MALFORMED}stream is not valid UTF-8")));
                }
                let block = match next {
                    Some(Ok(block)) => block,
                    Some(Err(sse::Error::Body(e))) => {
                        sg.take();
                        return Err(rt_err(format!(
                            "{TRANSPORT}stream error: {}",
                            mare_shared::redact::redact_str(&e.to_string())
                        )));
                    }
                    // over a cap, or a field that isn't UTF-8: not retried
                    Some(Err(e)) => {
                        sg.take();
                        return Err(rt_err(format!("{MALFORMED}{}", mare_shared::redact::redact_str(&e.to_string()))));
                    }
                    None => {
                        sg.take();
                        return Ok(Value::Nil);
                    }
                };
                // a block without data (only id / retry / event) dispatches nothing
                let Some(ev) = sse::dispatch(&mut s.last_id, block) else { continue };
                let t = lua.create_table()?;
                t.set("event", ev.event)?;
                t.set("data", ev.data)?;
                if !ev.id.is_empty() {
                    t.set("id", ev.id)?;
                }
                return Ok(Value::Table(t));
            }
        });
        m.add_async_method("close", |_, this, ()| async move {
            this.close_token.cancel();
            this.body.lock().await.take();
            this.sse.lock().await.take();
            Ok(())
        });
    }
}

async fn send_open(
    lua: &Lua,
    p: &Parts,
    url: &str,
    tok: &Option<CancellationToken>,
) -> mlua::Result<(reqwest::Response, Option<ShimGuard>)> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    check_offline_url(url).map_err(rt_err)?;
    check_offline_proxy_for_url_with(url, p.route.proxy.as_deref(), &env).map_err(rt_err)?;
    let fresh;
    let c = if p.route != Route::default() || p.redirect != Redirect::Follow || p.max_redirects != MAX_REDIRECTS {
        fresh = route_client(&p.route, p.redirect, p.max_redirects, p.fresh)?;
        &fresh
    } else if p.fresh {
        fresh = fresh_client()?;
        &fresh
    } else {
        client()?
    };
    let rb = builder(c, p, url, p.method.clone(), true, false);
    crate::exec_gate::check_unpoisoned(lua)?;
    Ok((send(rb, p, tok).await?, None))
}

/// request{url, method="GET", headers={}, body=string, json=table, form=table, timeout_ms, cancel,
/// header_timeout_ms, idle_timeout_ms, fresh, proxy, connect_timeout_ms, sse_max_line_bytes,
/// sse_max_event_bytes, redirect="follow"|"manual"|"same-origin", max_redirects=10}
/// -> Response (status/url/ok/headers fields; read/text/json/next_event methods). `authz` = the
/// calling tool's context when its network is restricted (checked per hop, pinned).
pub(super) async fn request(lua: Lua, opts: Table, authz: Option<crate::exec_ctx::Active>) -> mlua::Result<Response> {
    let url: String = opts.get("url")?;
    let p = parts(&lua, &opts)?;
    let tok = opt_token(&Some(opts.clone()));
    let deadline = p.timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let recording = crate::http_record::snapshot(p.method.as_str(), &url, &p.headers, p.body.as_deref());
    let (resp, shim_guard) = match &authz {
        Some(a) => match &a.policy.network {
            crate::ossandbox::Network::Proxy(np) => send_authorized(&lua, a, np, &p, &url, &tok, deadline).await?,
            crate::ossandbox::Network::Open if p.block_private => {
                let np = crate::netauth::NetPolicy::block_private_open(crate::netauth::fake_ip_mode_from_lua(&lua));
                send_authorized(&lua, a, &np, &p, &url, &tok, deadline).await?
            }
            crate::ossandbox::Network::Open => send_open(&lua, &p, &url, &tok).await?,
        },
        None => send_open(&lua, &p, &url, &tok).await?,
    };
    let mut final_url = resp.url().clone();
    strip_userinfo(&mut final_url);
    let url = final_url.to_string();
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let idle =
        opts.get::<Option<f64>>("idle_timeout_ms")?.filter(|ms| *ms > 0.0).map(|ms| Duration::from_millis(ms as u64));
    let cap = |k: &str, d: usize| -> mlua::Result<usize> {
        Ok(opts.get::<Option<f64>>(k)?.filter(|n| *n >= 1.0).map_or(d, |n| n as usize))
    };
    let caps = SseCaps {
        line: cap("sse_max_line_bytes", sse::DEFAULT_CAP)?,
        event: cap("sse_max_event_bytes", sse::DEFAULT_CAP)?,
    };
    let last_activity = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let mut raw_stream: ByteStream =
        Box::pin(GuardedStream { inner: Box::pin(resp.bytes_stream()), _guard: shim_guard });
    if let Some(req) = recording {
        raw_stream = crate::http_record::tee(req, status, &headers, raw_stream);
    }
    Ok(Response {
        status,
        url,
        headers,
        body: AsyncMutex::new(Some(stamped(raw_stream, last_activity.clone()))),
        sse: AsyncMutex::new(None),
        close_token: tok.as_ref().map_or_else(CancellationToken::new, CancellationToken::child_token),
        bad_utf8: Default::default(),
        caps,
        idle,
        deadline,
        last_activity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_response(chunks: Option<Vec<Result<bytes::Bytes, reqwest::Error>>>, cancelled: bool) -> Response {
        let close_token = CancellationToken::new();
        if cancelled {
            close_token.cancel();
        }
        let body: Option<ByteStream> = chunks.map(|items| Box::pin(futures_util::stream::iter(items)) as ByteStream);
        Response {
            status: 200,
            url: "http://127.0.0.1/test".into(),
            headers: vec![],
            body: AsyncMutex::new(body),
            sse: AsyncMutex::new(None),
            close_token,
            bad_utf8: Default::default(),
            caps: SseCaps { line: sse::DEFAULT_CAP, event: sse::DEFAULT_CAP },
            idle: None,
            deadline: None,
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
        }
    }

    #[tokio::test]
    async fn response_reports_invalid_json_invalid_utf8_sse_empty_body_and_aborted_reads() {
        let lua = Lua::new();

        // Invalid JSON body produces a malformed error.
        let r_json = mock_response(Some(vec![Ok(bytes::Bytes::from_static(b"{not-json"))]), false);
        lua.globals().set("r_json", r_json).unwrap();
        let err = lua.load("return r_json:json()").eval_async::<Value>().await.unwrap_err().to_string();
        assert!(err.contains("invalid JSON body"), "{err}");

        // Invalid UTF-8 in an SSE stream produces a malformed UTF-8 error.
        let r_sse = mock_response(Some(vec![Ok(bytes::Bytes::from_static(b"data: \xff\xfe\n\n"))]), false);
        lua.globals().set("r_sse", r_sse).unwrap();
        let err_sse = lua.load("return r_sse:next_event()").eval_async::<Value>().await.unwrap_err().to_string();
        assert!(err_sse.contains("not valid UTF-8"), "{err_sse}");

        // Consumed/absent body returns nil on next_event().
        let r_empty = mock_response(None, false);
        lua.globals().set("r_empty", r_empty).unwrap();
        let ev: Value = lua.load("return r_empty:next_event()").eval_async().await.unwrap();
        assert!(ev.is_nil());

        // Pre-cancelled response fails reads with aborted.
        let r_abort = mock_response(Some(vec![Ok(bytes::Bytes::from_static(b"data"))]), true);
        lua.globals().set("r_abort", r_abort).unwrap();
        let err_ab = lua.load("return r_abort:read()").eval_async::<Value>().await.unwrap_err().to_string();
        assert!(err_ab.contains("aborted"), "{err_ab}");
    }
}
