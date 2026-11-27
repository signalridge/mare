//! The persistent shell's lifetime: it belongs to the tools plugin and ends with it (
//! nothing mare started outlives it). Every platform: on Windows a shell left running holds its
//! output pipes open, and the runtime's blocking pipe reader then keeps mare from exiting.

use super::{call, host, text};
use crate::common::*;
use serde_json::json;

/// The OS process id of the persistent shell itself (`$$` runs in it; MSYS maps its own pid to
/// the Windows one through /proc).
const SHELL_PID: &str = if cfg!(windows) { "cat /proc/$$/winpid" } else { "echo $$" };

#[tokio::test]
async fn the_persistent_shell_ends_when_the_host_shuts_down() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let r = call(&h, "bash", json!({ "command": SHELL_PID })).await;
        let pid: u32 = text(&r).lines().next().unwrap_or("").trim().parse().unwrap_or_else(|_| panic!("{r}"));
        // the same shell serves the next call
        let r = call(&h, "bash", json!({ "command": SHELL_PID })).await;
        assert_eq!(text(&r).lines().next().unwrap_or("").trim(), pid.to_string(), "{r}");
        assert!(pid_alive(pid));
        // shutdown disposes the tools plugin, which stops its shell (the host is still held here,
        // so nothing is left to garbage collection)
        h.shutdown().await;
        wait_until(WAIT_SECS, "the persistent shell to end with the host", || async { !pid_alive(pid) }).await;
    })
    .await
}
