//! Host shutdown ends every process tree mare started — grandchildren included,
//! whatever started it, and while Lua still holds it — and the runtime then shuts down at once,
//! with no `shutdown_timeout` to mask a pipe read a surviving grandchild keeps pending (Windows
//! reads child pipes on the blocking pool). One test per spawn mechanism: piped children
//! (`mare.proc.spawn` / `start` / `run`), a pseudo-terminal, an MCP stdio server. Each child is
//! `mare-test-mcp tree <pid file>` (or the scripted server with `grandchild`), whose own child
//! holds the same stdio.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::common::*;
use serde_json::json;

/// Run `body` on a runtime of its own (current-thread + LocalSet, as `mare` runs), then drop
/// both; the drop must finish promptly. Returns what `body` returned.
fn on_own_runtime<F, Fut, T>(body: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let (done, finished) = std::sync::mpsc::channel();
    let (dropped_tx, dropped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        let out = local.block_on(&rt, body());
        let _ = done.send(out);
        drop(local);
        drop(rt);
        let _ = dropped_tx.send(());
    });
    let out = finished.recv_timeout(Duration::from_secs(300)).expect("the test body failed or hung");
    let t0 = Instant::now();
    assert!(dropped.recv_timeout(Duration::from_secs(30)).is_ok(), "the runtime's shutdown hung");
    eprintln!("runtime dropped in {:?}", t0.elapsed());
    out
}

/// The pid `mare-test-mcp` wrote to `file` (waits for it).
async fn pid_in(file: &Path) -> u32 {
    let mut pid = 0;
    wait_until(WAIT_SECS, &format!("{}", file.display()), || {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        pid = if text.ends_with('\n') { text.trim().parse().unwrap_or(0) } else { 0 };
        async move { pid != 0 }
    })
    .await;
    pid
}

async fn host_with_paths(env: &TestEnv) -> std::sync::Arc<mare_host::Host> {
    let h = env.host().no_session().build().await;
    expose(&h);
    global(&h, "FAKE", fake_mcp_bin());
    global(&h, "DIR", env.home.path().to_string_lossy().to_string());
    h
}

/// Shut the host down while it still holds these trees: every pid is gone right after.
async fn shutdown_ends(h: &mare_host::Host, pids: &[u32]) {
    assert!(pids.iter().all(|&p| pid_alive(p)), "{pids:?}");
    h.shutdown().await;
    // the tree was ended before shutdown returned; the bound only absorbs the OS's bookkeeping
    wait_until(10, "every process to end with the host", || async { pids.iter().all(|&p| !pid_alive(p)) }).await;
}

#[test]
fn piped_children_end_with_the_host() {
    on_own_runtime(|| async {
        let env = TestEnv::new().await;
        let h = host_with_paths(&env).await;
        // a spawned process with a read pending, a background job, a run in flight
        exec(
            &h,
            r#"P = mare.proc.spawn({ cmd = FAKE, args = { "tree", DIR .. "/spawn.pid" } })
               READ = mare.task.spawn(function() P:read() end)
               J = mare.proc.start({ cmd = FAKE, args = { "tree", DIR .. "/start.pid" } })
               RUN = mare.task.spawn(function() mare.proc.run({ cmd = FAKE, args = { "tree", DIR .. "/run.pid" } }) end)"#,
        )
        .await;
        let roots: (u32, u32) = eval(&h, "return P:pid(), J:pid()").await;
        let mut pids = vec![roots.0, roots.1];
        for f in ["spawn.pid", "start.pid", "run.pid"] {
            pids.push(pid_in(&env.home.path().join(f)).await);
        }
        shutdown_ends(&h, &pids).await;
    })
}

#[test]
fn a_terminal_tree_ends_with_the_host() {
    on_own_runtime(|| async {
        let env = TestEnv::new().await;
        let h = host_with_paths(&env).await;
        exec(&h, r#"PT = mare.pty.spawn({ cmd = FAKE, args = { "tree", DIR .. "/pty.pid" } })"#).await;
        let root: u32 = eval(&h, "return PT:pid()").await;
        let grandchild = pid_in(&env.home.path().join("pty.pid")).await;
        shutdown_ends(&h, &[root, grandchild]).await;
    })
}

#[test]
fn an_mcp_stdio_server_tree_ends_with_the_host() {
    on_own_runtime(|| async {
        let env = TestEnv::new().await;
        let h = host_with_paths(&env).await;
        let file = env.home.path().join("mcp.pid");
        let cfg = json!({ "log": env.home.path().join("mcp.log"), "grandchild": file });
        let spec = json!({ "name": "fake", "command": fake_mcp_bin(), "args": ["scripted", cfg.to_string()] });
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        exec(&h, "CL = mare.mcp.connect(SPEC)").await;
        let grandchild = pid_in(&file).await;
        shutdown_ends(&h, &[grandchild]).await;
    })
}
