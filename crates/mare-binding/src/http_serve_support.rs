// exec-ctx: not a tool entry (split from http_serve.rs; gated by exec_ctx::check at http_serve.rs:773)
use super::*;

pub(crate) struct CancelOnDrop(Option<CancellationToken>);

impl CancelOnDrop {
    pub(super) fn new(token: CancellationToken) -> Self {
        Self(Some(token))
    }

    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(tok) = self.0.take() {
            tok.cancel();
        }
    }
}

pub(super) struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    pub(super) fn new(handle: JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    pub(super) fn disarm(&mut self) -> Option<JoinHandle<()>> {
        self.0.take()
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

pub(super) struct ReceiverStreamBody {
    pub(super) rx: mpsc::Receiver<Result<Frame<Bytes>, std::io::Error>>,
    pub(super) _cancel_guard: CancelOnDrop,
}

impl Body for ReceiverStreamBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let res = self.rx.poll_recv(cx);
        if let Poll::Ready(None) = res {
            // Normal end of stream: `req.cancel` must only fire when the body is dropped early
            // (client disconnect/abort), not when hyper drops a fully-sent body.
            self._cancel_guard.disarm();
        }
        res
    }
}

pub fn empty_body() -> ServeBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

pub fn full_body(bytes: impl Into<Bytes>) -> ServeBody {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

pub fn header_contains_token(headers: &HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|s| s.split(',').any(|part| part.trim().eq_ignore_ascii_case(token)))
}

pub fn plain_status_response(status: StatusCode, msg: &'static str) -> hyper::Response<ServeBody> {
    hyper::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full_body(msg))
        .unwrap_or_else(|_| hyper::Response::new(empty_body()))
}

pub async fn acquire_permit(
    sem: &Arc<Semaphore>,
    shutdown: &CancellationToken,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => None,
        res = sem.clone().acquire_owned() => res.ok(),
    }
}

impl HttpServer {
    async fn close_async(&self) {
        self.shutdown.cancel();
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
        let me = tokio::task::try_id();
        for handle in self.tasks.drain() {
            if Some(handle.id()) != me {
                handle.abort();
                let _ = handle.await;
            }
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Ok(mut g) = self.task.try_lock()
            && let Some(handle) = g.take()
        {
            handle.abort();
        }
        for handle in self.tasks.drain() {
            handle.abort();
        }
    }
}

impl UserData for HttpServer {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("addr", |_, this, ()| Ok(this.addr.to_string()));
        m.add_method("port", |_, this, ()| Ok(this.addr.port()));
        m.add_async_method("close", |_, this, ()| async move {
            this.close_async().await;
            Ok(())
        });
    }
}

pub(super) fn parse_limit(opts: &Table, key: &str, default: usize) -> mlua::Result<Result<usize, String>> {
    match opts.get::<Option<i64>>(key)? {
        Some(n) if n <= 0 => Ok(Err(format!("{key} must be at least 1"))),
        Some(n) => Ok(Ok(usize::try_from(n).unwrap_or(usize::MAX).min(Semaphore::MAX_PERMITS))),
        None => Ok(Ok(default.min(Semaphore::MAX_PERMITS))),
    }
}

pub(super) async fn serve_with_binder<F, Fut>(lua: Lua, opts: Table, binder: F) -> mlua::Result<MultiValue>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: std::future::Future<Output = Result<(TcpListener, SocketAddr), String>>,
{
    serve_with_binder_and_hooks(
        lua,
        opts,
        binder,
        mare_shared::paths::host_token_file(),
        #[cfg(test)]
        None,
    )
    .await
}

pub(super) async fn accept_next(
    listener: &TcpListener,
    #[cfg(test)] test_hooks: Option<&ws::ServeTestHooks>,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    #[cfg(test)]
    if let Some(hooks) = test_hooks
        && hooks.inject_accept_error.swap(false, Ordering::AcqRel)
    {
        hooks.on_accept_error.notify_waiters();
        return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "injected accept error"));
    }
    listener.accept().await
}

/// Wait out an accept-error backoff step unless `shutdown` is cancelled (`false` = cancelled).
pub async fn accept_backoff_wait(prev: &mut std::time::Duration, shutdown: &CancellationToken) -> bool {
    *prev = if prev.is_zero() {
        std::time::Duration::from_millis(super::ACCEPT_BACKOFF_INITIAL_MS)
    } else {
        (*prev * 2).min(std::time::Duration::from_millis(super::ACCEPT_BACKOFF_MAX_MS))
    };
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => false,
        // sleep-ok: bounded exponential accept backoff after an accept error; the shutdown branch cancels it
        _ = tokio::time::sleep(*prev) => true,
    }
}
