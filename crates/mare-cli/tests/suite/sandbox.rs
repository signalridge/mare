//! OS sandbox primitive: path checks, levels and escalation, refusal without a
//! backend, structured denials, terminals under a policy and (seatbelt) the network proxy.
#![cfg(unix)]

use std::sync::Arc;

use crate::common::*;
use mare_host::Host;

mod backends;
mod caps_matrix;
#[cfg(target_os = "linux")]
#[path = "sandbox_churn.rs"]
mod churn;
mod deny_read;
mod policy;
mod protection;

async fn mechanism(h: &Host) -> String {
    eval(h, "return mare.proc.sandbox.capabilities().mechanism").await
}

async fn host(env: &TestEnv) -> Arc<Host> {
    env.host().no_session().build().await
}

/// Explicit platform acceptance: requires `cc` and `git` on PATH and the expected backend.
#[tokio::test]
#[ignore = "platform matrix: requires cc, git and MARE_TEST_EXPECT_BACKEND"]
async fn unix_toolchain_matrix() {
    local(async {
        let expected =
            std::env::var("MARE_TEST_EXPECT_BACKEND").expect("set MARE_TEST_EXPECT_BACKEND to the expected backend");
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let mech = mechanism(&h).await;
        assert_eq!(mech, expected, "matrix cell did not select its required backend");
        let (code, output, failed): (i64, String, Option<String>) = eval(
            &h,
            r#"local r = mare.proc.run({ cmd = "sh", args = { "-c", [[
                set -eu
                export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
                printf 'int main(void) { return 0; }\n' > hello.c
                cc hello.c -o hello
                ./hello
                git init -q .
                git status --porcelain
                echo matrix-ok
            ]] }, sandbox = { level = "workspace-write", network = true }, timeout_ms = 120000 })
            return r.code, r.stdout .. r.stderr, r.sandbox_failed"#,
        )
        .await;
        assert_eq!(failed, None, "sandbox failed: {output}");
        assert_eq!(code, 0, "toolchain failed: {output}");
        assert!(output.contains("matrix-ok"), "{output}");
        h.shutdown().await;
    })
    .await
}
