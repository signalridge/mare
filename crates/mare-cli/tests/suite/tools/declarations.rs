//! What a request declares: a tool whose `enabled` reads a capability another plugin provides (a
//! query backend: session-search's shape) is declared once that capability is there — from the
//! first request on, and from the next request when it arrives mid-prompt.

use super::*;
use crate::suite::kernel::write_plugin;

/// A query backend that comes up late in its plugin's apply (query-disk's shape).
const LATE_BACKEND: &str = r#"return { name = "late-backend", inject = { "query" }, apply = function(ctx)
  mare.task.sleep(200)
  ctx.query:register("late-backend", { source = { capability = "in_process" },
    operations = { ["probe.read"] = function() return { value = "read ok" } end } })
end }"#;

/// `probe_read` is enabled only while a backend serves `probe.read`; `enable_backend` registers one.
const PROBE_TOOL: &str = r#"return { name = "probe-tool", inject = { "tools", "query" }, apply = function(ctx)
  ctx.tools:register("probe_read", { description = "read", parameters = { type = "object" }, side_effect = "read",
    enabled = function() return ctx.query:supports("probe.read") end,
    execute = function() return ctx.query:call("probe.read", {}).value end })
  ctx.tools:register("enable_backend", { description = "enable", parameters = { type = "object" }, side_effect = "read",
    execute = function()
      ctx.query:register("mid-backend", { source = { capability = "in_process" },
        operations = { ["probe.read"] = function() return { value = "read ok" } end } })
      return "enabled"
    end })
end }"#;

/// the tool results of a prompt's messages, in order
fn tool_results(r: &serde_json::Value) -> Vec<String> {
    r["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "tool_result")
        .map(|m| m["content"][0]["text"].as_str().unwrap_or("").to_string())
        .collect()
}

/// The host is handed out once every plugin has started: the built-in query-disk already serves
/// `session.read` when Host::new returns (the session-search "tool was not declared in this
/// request" race came from starting it after the host was handed out), and the first request
/// declares a tool that a backend registered late in another plugin's apply enables, and its call
/// runs. The model's call is scripted: undeclared, it is refused.
#[tokio::test]
async fn the_first_request_declares_a_tool_another_plugins_backend_enables() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "late-backend", LATE_BACKEND);
        write_plugin(&env, "probe-tool", PROBE_TOOL);
        let b = env
            .host()
            .builtins(&["query", "query-disk"])
            .faux(json!([{ "tool_calls": [{ "name": "probe_read", "arguments": {} }] }, { "text": "done" }]))
            .user_plugins()
            .no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        global(&h, "Q", h.service_table("query").unwrap());
        let built_in_reads: bool = eval(&h, "return Q:supports('session.read')").await;
        let r = h.prompt("read it", json!({})).await.unwrap();
        h.shutdown().await;
        assert!(built_in_reads, "query-disk's session.read is up when Host::new returns");
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(tool_results(&r), ["read ok"], "the first request declared the tool: {r}");
    })
    .await
}

/// A query backend that arrives within a prompt reaches the next request's tool declarations: the
/// tools' generation moves on `query/changed`, so the agent enumerates them again (it caches them
/// per generation).
#[tokio::test]
async fn a_query_backend_arriving_mid_prompt_reaches_the_next_requests_tools() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "probe-tool", PROBE_TOOL);
        let b = env
            .host()
            .builtins(&["query"])
            .faux(json!([
                { "tool_calls": [{ "name": "enable_backend", "arguments": {} }] },
                { "tool_calls": [{ "name": "probe_read", "arguments": {} }] },
                { "text": "done" }
            ]))
            .user_plugins()
            .no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        let r = h.prompt("enable then read", json!({})).await.unwrap();
        h.shutdown().await;
        assert!(r["error"].is_null(), "{r}");
        assert_eq!(tool_results(&r), ["enabled", "read ok"], "the next request declared the tool: {r}");
    })
    .await
}
