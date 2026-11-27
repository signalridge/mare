use super::*;

// The backpressure regression runs alone under nextest. Track live bytes rather than RSS so
// repeated canceled submissions reveal retained SDK allocations without depending on the OS.
struct CountingAllocator;
static LIVE_BYTES: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let ptr = unsafe { std::alloc::System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { std::alloc::System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        let next = unsafe { std::alloc::System.realloc(ptr, layout, size) };
        if !next.is_null() {
            LIVE_BYTES.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        next
    }
}

fn progress_test_client() -> (
    Arc<Inner>,
    Service,
    tokio::io::DuplexStream,
    tokio::io::BufReader<tokio::io::DuplexStream>,
    mpsc::UnboundedReceiver<ProgressToken>,
) {
    use rmcp::service::serve_directly_with_ct;
    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (remote_write, client_read) = tokio::io::duplex(65536);
    let (client_write, remote_read) = tokio::io::duplex(65536);
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    let mut handler = inner.handler();
    handler.progress_seen = Some(seen_tx);
    let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
        handler.progress_tracker.clone(),
    );
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(handler, tracked, None, CancellationToken::new());
    (inner, service, remote_write, tokio::io::BufReader::new(remote_read), seen_rx)
}

async fn start_progress_test_call(
    inner: &Arc<Inner>,
    service: &Service,
    reader: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
) -> (tokio::task::JoinHandle<Result<serde_json::Value, String>>, serde_json::Value) {
    use tokio::io::AsyncBufReadExt;
    let peer = service.peer().clone();
    let sender = inner.clone();
    let task = tokio::spawn(async move {
        call_tool(peer, "echo".into(), serde_json::Map::new(), Duration::from_millis(100), &sender, Some(0)).await
    });
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let request = serde_json::from_str(&line).unwrap();
    (task, request)
}

async fn send_test_progress(
    writer: &mut tokio::io::DuplexStream,
    seen: &mut mpsc::UnboundedReceiver<ProgressToken>,
    token: serde_json::Value,
) {
    use tokio::io::AsyncWriteExt;
    let event = serde_json::json!({ "jsonrpc":"2.0", "method":"notifications/progress", "params":{"progressToken":token,"progress":1} });
    writer.write_all(format!("{event}\n").as_bytes()).await.unwrap();
    seen.recv().await.unwrap();
    tokio::task::yield_now().await;
}

async fn send_progress_notifications(writer: &mut tokio::io::DuplexStream, prefix: &str, count: usize) {
    use tokio::io::AsyncWriteExt;
    for i in 0..count {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": format!("{prefix}-{i}"), "progress": 1, "total": 1 }
        });
        writer.write_all(format!("{notif}\n").as_bytes()).await.unwrap();
    }
    writer.flush().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canceled_backpressured_submissions_do_not_accumulate_progress_watchers() {
    // Isolate allocation accounting even when `cargo test` runs other tests concurrently.
    const CHILD: &str = "MARE_TEST_MCP_BACKPRESSURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mcp::conn::tests::canceled_backpressured_submissions_do_not_accumulate_progress_watchers",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage, serve_directly_with_ct};
    use rmcp::transport::Transport;
    use std::sync::{Condvar, Mutex as ThreadMutex};
    struct FrozenTransport {
        entered: Option<oneshot::Sender<()>>,
        latch: Arc<(ThreadMutex<bool>, Condvar)>,
    }
    impl Transport<RoleClient> for FrozenTransport {
        type Error = std::io::Error;
        fn send(
            &mut self,
            _: TxJsonRpcMessage<RoleClient>,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
            std::future::ready(Ok(()))
        }
        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let (lock, cond) = &*self.latch;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cond.wait(released).unwrap();
            }
            None
        }
        async fn close(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    struct Release(Arc<(ThreadMutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            let (lock, cond) = &*self.0;
            *lock.lock().unwrap() = true;
            cond.notify_all();
        }
    }
    let latch = Arc::new((ThreadMutex::new(false), Condvar::new()));
    let _release = Release(latch.clone());
    let (entered_tx, entered_rx) = oneshot::channel();
    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(
        inner.handler(),
        FrozenTransport { entered: Some(entered_tx), latch },
        None,
        CancellationToken::new(),
    );
    entered_rx.await.unwrap();
    let peer = service.peer().clone();
    // rmcp's Peer channel holds 1024 messages; its service loop is paused in receive.
    for _ in 0..1024 {
        let params = CallToolRequestParams::new("filler").with_arguments(serde_json::Map::new());
        peer.send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    }
    let before = LIVE_BYTES.load(Ordering::SeqCst);
    for _ in 0..4000 {
        let pending = tokio::task::unconstrained(call_tool(
            peer.clone(),
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(30),
            &inner,
            Some(0),
        ));
        tokio::pin!(pending);
        assert!(futures_util::poll!(pending.as_mut()).is_pending(), "submission did not experience queue backpressure");
    }
    assert_eq!(inner.progress.receiver_count(), 0, "canceled submissions retained subscribers");
    let result =
        call_tool(peer.clone(), "timeout".into(), serde_json::Map::new(), Duration::from_millis(1), &inner, Some(0))
            .await;
    assert!(result.unwrap_err().contains("could not be submitted"));
    assert!(inner.dispatch_gate.try_lock().is_ok(), "submission timeout retained the gate");
    assert_eq!(inner.progress.receiver_count(), 0, "timed-out submission retained its subscriber");
    let after = LIVE_BYTES.load(Ordering::SeqCst);
    let growth = after - before;
    eprintln!("retained bytes after canceled submissions: {growth}");
    assert!(growth < 64 * 1024, "canceled submissions retained {growth} bytes");
}

mod progress_and_timeouts;
mod queue_and_reconnect;
