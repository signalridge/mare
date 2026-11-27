//! Built-in tools through the tools service (pi tool tests: read/edit/write/ls, truncation,
//! bash exit codes; Codex tool cancellation).

use crate::common::*;
use serde_json::json;

#[cfg(unix)]
mod abort_release;
mod apply_patch;
mod bash_shell;
mod call_tool;
mod declarations;
mod deferred;
mod edit;
mod execution;
mod read_write_ls;
mod reload;
mod result_sanitization;
mod shell_lifecycle;

/// Run one tool call through the tools service and return its result as JSON.
pub(super) async fn call(h: &mare_host::Host, name: &str, args: serde_json::Value) -> serde_json::Value {
    global(
        h,
        "CALL",
        mare_shared::lua::json_to_lua(h.lua(), &json!({ "id": "t1", "name": name, "arguments": args })).unwrap(),
    );
    let v: mlua::Value = eval(h, "return T:execute(CALL, {})").await;
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

pub(super) fn text(r: &serde_json::Value) -> String {
    match &r["content"] {
        serde_json::Value::String(s) => s.clone(),
        c => c.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
    }
}

pub(super) async fn host(env: &TestEnv) -> std::sync::Arc<mare_host::Host> {
    env.host().builtins(&["tools-apply-patch"]).no_session().build_exposed().await
}

/// The tool list (prompt `<tools>`, request declarations) follows the plugin rows, not the order
/// in which plugins' fibers activated: `a-early` (an earlier row) waits for a service `b-late`
/// provides, so it activates last, yet its tool is listed first. A list that moved with
/// activation timing would also change the provider's cached prompt prefix between runs.
#[tokio::test]
async fn tool_list_follows_plugin_rows_not_activation_order() {
    local(async {
        let env = TestEnv::new().await;
        let plugin = |name: &str, body: &str| {
            let dir = env.config_dir().join("plugins").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("init.lua"), body).unwrap();
        };
        let tool = |name: &str| format!(r#"ctx.tools:register("{name}", {{ description = "{name}", parameters = {{ type = "object" }}, execute = function() return "" end }})"#);
        plugin(
            "a-early",
            &format!(r#"return {{ name = "a-early", inject = {{ "tools", "late-svc" }}, apply = function(ctx) {} end }}"#, tool("early_tool")),
        );
        plugin(
            "b-late",
            &format!(
                r#"return {{ name = "b-late", inject = {{ "tools" }}, provide = {{ "late-svc" }}, contracts = {{ ["late-svc"] = {{ methods = {{ v = {{ doc = "v", result = {{ type = "object" }} }} }} }} }}, apply = function(ctx) require("mare_lib").service(ctx, "late-svc", {{ v = function() end }}) {} end }}"#,
                tool("late_tool")
            ),
        );
        let h = env.host().user_plugins().no_session().build().await;
        global(&h, "T", h.service_table("tools").unwrap());
        let names: Vec<String> = eval(
            &h,
            r#"local out = {}
            for _, d in ipairs(T:list()) do
              if d.name == "early_tool" or d.name == "late_tool" then out[#out + 1] = d.name end
            end
            return out"#,
        )
        .await;
        assert_eq!(names, ["early_tool", "late_tool"]);
        h.shutdown().await;
    })
    .await
}
