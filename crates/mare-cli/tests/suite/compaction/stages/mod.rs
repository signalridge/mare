//! The compaction stages: automatic and manual runs, order, limits and what a failed run leaves.

use super::*;

const PREPARE_REQUEST: &str = r#"
function PREPARE_REQUEST(model)
    model = model or L:current()
    local messages = A:list_messages().data
    local context, tools = C.cordis:get("context"), C.cordis:get("tools")
    return C.cordis:get("compact"):prepare_request({ model = model, messages = messages,
        system = context:system_prompt(model), tools = tools:definitions(model) }, messages)
end
"#;

/// What every request carries besides the messages (system prompt + tool declarations): the
/// compaction estimate counts it (Codex history.rs), so token thresholds in tests sit above it.
pub(super) async fn overhead(env: &TestEnv) -> i64 {
    overhead_for(env.host().builtins(&["compact", "usage"])).await
}

pub(super) async fn overhead_for(builder: HostBuilder) -> i64 {
    let h = builder.faux(json!([])).no_session().build().await;
    global(&h, "K", h.service_table("compact").unwrap());
    let o: i64 = eval(&h, "return K:measure()").await;
    h.shutdown().await;
    o
}

mod admission;
mod recovery;
mod retention;
