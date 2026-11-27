use super::*;
use crate::rpc::rpc_harness::{HOST_LOCK, TestRpc, test_host};

/// The connection's holdings: every disposable of every connection fiber the rpc host plugin keeps.
/// Read from Rust through the host VM (a host test may look inside; plugins and fixtures may not).
const HELD: &str = r#"local n = 0
for _, fiber in pairs(refusal_test_ctx:get("rpc").fibers) do n = n + fiber._disposables:length() end
return n"#;

/// Review a-contracts-q (should-fix 1): a registration refused when its service starts — an
/// explicit replacement of a provision another owner holds, `already_provided` — disposes the
/// failed child fiber (and awaits it) before the refusal is the provide's error, so a client that
/// retries leaves nothing on its connection (`crates/mare-kernel/lua/mare_kernel/plugins/rpc.lua`,
/// the pcall'd await in `RpcService:provide`). The e2e test can only see the public service list;
/// this one counts what the connections hold.
#[tokio::test]
async fn a_refused_service_registration_leaves_nothing_on_the_connection() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let lua = host.lua();
            lua.globals().set("refusal_test_ctx", host.kernel.host_ctx().unwrap()).unwrap();
            // two connections: `a` provides `forward-probe`, `b` then asks to replace it — another
            // owner's provision, refused when b's service starts (already_provided)
            let mut a = TestRpc::connect(host.clone());
            let mut b = TestRpc::connect(host.clone());
            for conn in [&mut a, &mut b] {
                let init = conn
                    .call_id(1, "initialize", json!({"protocol_version": "3.0", "capabilities":{"callbacks":true}}))
                    .await;
                assert!(init.get("error").is_none(), "{init}");
            }
            let forward = json!({
                "parameters": {"type":"object","properties":{},"additionalProperties":false},
                "result": {"type":"object"},
                "order": [],
            });
            let provided =
                a.call_id(2, "services.register", json!({"name":"forward-probe","methods":{"forward":forward}})).await;
            assert!(provided.get("error").is_none(), "a provides forward-probe: {provided}");
            let obsolete = b
                .call_id(
                    8,
                    "services.register",
                    json!({"name":"forward-probe","methods":{"forward":{}},"replace":true}),
                )
                .await;
            assert_eq!(obsolete["error"]["data"]["code"], "invalid_arguments", "{obsolete}");

            let held = || async { lua.load(HELD).eval_async::<i64>().await.unwrap() };
            let before = held().await;
            for id in 2..5 {
                let reply =
                    b.call_id(id, "services.register", json!({"name":"forward-probe","methods":{"forward":{}}})).await;
                assert!(reply.to_string().contains("already_provided"), "attempt {id}: {reply}");
                assert_eq!(reply["error"]["data"]["code"], "already_provided", "attempt {id}: {reply}");
            }
            assert_eq!(held().await, before, "a refused registration left its failed service on the connection");

            for conn in [a, b] {
                let task = conn.disconnect();
                task.abort();
                let _ = task.await;
            }
            host.shutdown().await;
        })
        .await;
}
