use super::*;
use crate::common::rpc::RpcClient;
use std::collections::BTreeMap;

/// reference implementation: cancellation releases admission; the RPC cases also
/// prove that a cancelled side request cannot block a subsequent session switch.
#[test]
fn rpc_cancelled_summary_allows_immediate_retry_and_session_create() {
    cancellation_case(true);
}

#[test]
fn rpc_cancelled_regular_allows_immediate_retry_and_session_create() {
    cancellation_case(false);
}

fn cancellation_case(summarize_only: bool) {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    let plugins = config.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        config.join("init.lua"),
        r#"mare.setup {
                public_events = { "probe/*" },
                plugins = { compact = { auto = false, keep_recent_tokens = 1,
                    recent_user_tokens = 0, stages = { "local" } } },
            }"#,
    )
    .unwrap();
    std::fs::write(
        plugins.join("compact-cancel-probe.lua"),
        r#"return { name = "compact-cancel-probe", inject = { "llm", "bus" },
            contracts = { probe = { owner = "compact-cancel-probe", events = {
                ["summary-started"] = { doc = "probe", mode = "emit", expose = "both", delivery = "required", ignorable = false, data = { type = "object", additionalProperties = true }, example = {} },
            } } }, apply = function(ctx)
                local provider = ctx.llm:provider("faux")
                local stream, blocked = provider.stream, false
                provider.stream = function(req, sink)
                    if req.purpose == "compact" and not blocked then
                        blocked = true
                        ctx.bus:emit("probe/summary-started", {})
                        req.cancel:wait()
                        error("cancelled")
                    end
                    return stream(req, sink)
                end
            end }"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "text": "first ".repeat(400) },
            { "text": "second ".repeat(400) },
            { "text": "## Summary\nretry complete" }
        ])
        .to_string(),
    )
    .unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_owned(), home.path().to_string_lossy().into_owned()),
        ("MARE_FAUX_SCRIPT".to_owned(), script.to_string_lossy().into_owned()),
    ]);
    let mut rpc = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, home.path());
    rpc.call("initialize", json!({"protocol_version": "3.0"}));
    rpc.call("events.subscribe", json!({ "scope": "all", "include": ["probe/summary-started"] }));
    let session = rpc.call("session.create", json!({}));
    let session_id = session["session_id"].as_str().unwrap().to_owned();
    for text in ["msg 1", "msg 2"] {
        let st = rpc.call("agent.prompt", json!({ "session_id": session_id, "input": text }));
        rpc.call("agent.wait", json!({ "session_id": session_id, "turn_id": st["turn_id"] }));
    }

    let params = if summarize_only {
        json!({ "session_id": session_id, "summarize_only": true, "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "branch work" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "done" }] }
            ] })
    } else {
        json!({ "session_id": session_id })
    };
    let id = rpc.request("compact.run", params);
    rpc.wait_event("probe/summary-started");
    let concurrent = rpc.call_err("compact.run", json!({ "session_id": session_id }));
    assert_eq!(concurrent["data"]["code"], "compact.not_compacted", "summarize_only={summarize_only}: {concurrent}");
    rpc.notify("$/cancel_request", json!({ "id": id }));
    rpc.wait(id).expect_err("cancelled compaction must return an error");

    let retry = rpc.call("compact.run", json!({ "session_id": session_id }));
    assert_eq!(retry["status"], "compacted", "retry after cancel failed: {retry}");
    let next = rpc.call("session.create", json!({}));
    assert_ne!(next["session_id"].as_str().unwrap(), session_id);
    rpc.shutdown_and_wait();
}
