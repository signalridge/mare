//! The configurable stdio server (`tests/suite/mcp.rs` documents the options): every message it
//! receives is appended to `log` as `<method>` (`notify <method> <requestId>` for
//! notifications), and it writes `start` on each launch.

use crate::{append, die, grandchild, is_request, messages, method, print_line, reply, text_arg};
use serde_json::{Value, json};
use std::time::Duration;

/// Python truthiness of a config value (absent and null are false).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `%s` of a JSON value the way Python prints it (`None` for null or absent).
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(v) => v.to_string(),
    }
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| die(&format!("tool names {v}")))
        .iter()
        .map(|n| n.as_str().unwrap_or_else(|| die(&format!("tool name {n}"))).to_string())
        .collect()
}

fn tool(cfg: &Value, n: &str, changed: bool) -> Value {
    let mut result = json!({ "name": n, "description": format!("tool {n}"),
                             "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } });
    if (truthy(cfg.get("invalid_initial")) && n == "broken")
        || (truthy(cfg.get("invalid_after_change")) && n == "echo" && changed)
    {
        result["inputSchema"]["properties"]["text"]["$ref"] = json!("https://example.invalid/text.json");
    }
    if truthy(cfg.get("invalid_large")) && n == "huge" {
        result["inputSchema"]["properties"]["text"]["const"] = json!(u64::MAX);
    }
    if truthy(cfg.get("initial_schema_required")) && n == "echo" {
        result["inputSchema"]["required"] = json!(["text"]);
    }
    if truthy(cfg.get("change_definition")) && n == "echo" {
        result["annotations"] = json!({ "readOnlyHint": !changed });
        if changed {
            result["inputSchema"]["required"] = json!(["text"]);
        }
    }
    if truthy(cfg.get("change_empty_default")) && n == "echo" {
        result["inputSchema"]["properties"]["text"]["default"] = if changed { json!([]) } else { json!({}) };
    }
    result
}

pub fn run(cfg_text: &str) {
    let cfg: Value = serde_json::from_str(cfg_text).unwrap_or_else(|e| die(&format!("config: {e}")));
    let log = cfg["log"].as_str().unwrap_or_else(|| die("config without a log")).to_string();
    append(&log, "start");
    if let Some(pids) = cfg.get("pid_log").and_then(Value::as_str) {
        append(pids, &std::process::id().to_string());
    }
    for l in cfg.get("stderr").and_then(Value::as_array).into_iter().flatten() {
        eprintln!("{}", l.as_str().unwrap_or_else(|| die("stderr line")));
    }
    // a child of its own holding the stdio pipes; its pid goes to this file
    let _grandchild = cfg.get("grandchild").and_then(Value::as_str).map(grandchild);
    if truthy(cfg.get("env_dump")) {
        let env: serde_json::Map<String, Value> = std::env::vars_os()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), Value::String(v.to_string_lossy().into_owned())))
            .collect();
        let path = cfg["env_dump"].as_str().unwrap_or_else(|| die("env_dump"));
        std::fs::write(path, Value::Object(env).to_string()).unwrap_or_else(|e| die(&format!("env_dump: {e}")));
    }
    let mut tools = cfg.get("tools").map_or_else(|| vec!["echo".to_string()], names);
    let mut changed = false;
    let mut human_calls = std::collections::HashMap::<u64, (Value, bool)>::new();
    let mut next_human_id = 10000_u64;
    for m in messages() {
        if m.get("method").is_none() {
            if let Some((call_id, stalled)) = m["id"].as_u64().and_then(|id| human_calls.remove(&id)) {
                let action = m["result"]["action"].as_str().unwrap_or("invalid");
                append(&log, &format!("elicitation {action}"));
                if !stalled {
                    print_line(&reply(&call_id, Some(json!({"content": [{"type": "text", "text": action}]}))));
                }
            }
            continue;
        }
        if !is_request(&m) {
            let params = m.get("params").filter(|p| truthy(Some(p)));
            let req = params.and_then(|p| p.get("requestId"));
            append(&log, &format!("notify {} {}", py_str(m.get("method")), py_str(req)));
            continue;
        }
        let meth = method(&m).to_string();
        append(&log, &meth);
        let id = &m["id"];
        let p = &m["params"];
        let r = match meth.as_str() {
            "initialize" => {
                let mut caps =
                    cfg.get("capabilities").cloned().unwrap_or_else(|| json!({ "tools": { "listChanged": true } }));
                if truthy(cfg.get("rich")) {
                    caps["resources"] = json!({});
                    caps["prompts"] = json!({});
                }
                Some(json!({ "protocolVersion": "2025-06-18", "capabilities": caps,
                             "serverInfo": { "name": "fake", "version": "1" },
                             "instructions": cfg.get("instructions").cloned().unwrap_or(json!("")) }))
            }
            "resources/list" => {
                Some(json!({ "resources": [{ "uri": "mem://a", "name": "a", "mimeType": "text/plain" }] }))
            }
            "resources/templates/list" => {
                Some(json!({ "resourceTemplates": [{ "uriTemplate": "mem://{x}", "name": "x" }] }))
            }
            "resources/read" => {
                let uri = p["uri"].as_str().unwrap_or_else(|| die("resources/read uri"));
                Some(json!({ "contents": [{ "uri": uri, "text": format!("body of {uri}") }] }))
            }
            "prompts/list" => Some(json!({ "prompts": [
                { "name": "greet", "description": "say hi", "arguments": [{ "name": "who", "required": true }] },
                { "name": "img_prompt", "description": "send image", "arguments": [] }
            ] })),
            "prompts/get" => {
                if p["name"].as_str() == Some("img_prompt") {
                    Some(json!({
                        "messages": [{
                            "role": "user",
                            "content": { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" }
                        }]
                    }))
                } else {
                    let who = p["arguments"]["who"].as_str().unwrap_or_else(|| die("prompts/get who"));
                    Some(
                        json!({ "messages": [{ "role": "user", "content": { "type": "text", "text": format!("Hello {who}") } }] }),
                    )
                }
            }
            "tools/list" => {
                if truthy(cfg.get("tools_method_not_found")) {
                    print_line(
                        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }),
                    );
                    continue;
                }
                if changed && truthy(cfg.get("fail_refresh")) {
                    print_line(
                        &json!({ "jsonrpc":"2.0", "id":id, "error":{"code":-32603,"message":"refresh failed"} }),
                    );
                    continue;
                }
                let delay = cfg.get("list_delay").and_then(Value::as_f64).unwrap_or(0.0);
                std::thread::sleep(Duration::from_secs_f64(delay));
                // `list_gate`: hold the answer until the test creates this file (a controllable
                // delay; bounded so a forgotten gate cannot hang a run)
                if let Some(gate) = cfg.get("list_gate").and_then(Value::as_str) {
                    let t0 = std::time::Instant::now();
                    while !std::path::Path::new(gate).exists() && t0.elapsed() < Duration::from_secs(300) {
                        // sleep-ok: poll interval while waiting for the test to create the gate file (bounded at 300 s)
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                let mut r = json!({ "tools": tools.iter().map(|n| tool(&cfg, n, changed)).collect::<Vec<_>>() });
                if truthy(cfg.get("pages")) {
                    r["nextCursor"] = json!("more");
                }
                Some(r)
            }
            "tools/call" => match p["name"].as_str().unwrap_or_else(|| die("tools/call name")) {
                "elicit" | "elicit_slow" => {
                    next_human_id += 1;
                    human_calls.insert(next_human_id, (id.clone(), p["name"] == "elicit_slow"));
                    print_line(&json!({"jsonrpc": "2.0", "id": next_human_id, "method": "elicitation/create",
                        "params": {"mode": "form", "message": "Your name?", "requestedSchema": {
                            "type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}}));
                    continue;
                }
                "elicit_url" => {
                    next_human_id += 1;
                    human_calls.insert(next_human_id, (id.clone(), false));
                    print_line(&json!({"jsonrpc": "2.0", "id": next_human_id, "method": "elicitation/create",
                        "params": {"mode": "url", "message": "Open link", "url": "https://example.test/auth", "elicitationId": "elicit-url-1"}}));
                    continue;
                }
                "crash" => std::process::exit(3),
                "slow" => {
                    // answers after 10 min: never, within a test
                    let id = id.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(600));
                        print_line(&reply(&id, Some(json!({ "content": [{ "type": "text", "text": "late" }] }))));
                    });
                    continue;
                }
                "change" => {
                    tools = names(&cfg["changed_tools"]);
                    changed = true;
                    print_line(&reply(id, Some(json!({ "content": [{ "type": "text", "text": "changed" }] }))));
                    for _ in 0..3 {
                        print_line(&json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
                    }
                    continue;
                }
                "progress" => {
                    if let Some(token) = p.get("_meta").and_then(|m| m.get("progressToken")).filter(|t| !t.is_null()) {
                        print_line(&json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": 1,
                                "total": 2,
                                "message": "step 1"
                            }
                        }));
                    }
                    Some(json!({ "content": [{ "type": "text", "text": "progress-done" }] }))
                }
                "progress_burst" => {
                    if let Some(token) = p.get("_meta").and_then(|m| m.get("progressToken")).filter(|t| !t.is_null()) {
                        for i in 1..=100 {
                            print_line(&json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": token,
                                    "progress": i,
                                    "total": 100,
                                    "message": format!("burst {i}")
                                }
                            }));
                        }
                    }
                    Some(json!({ "content": [{ "type": "text", "text": "burst-done" }] }))
                }
                "big" => Some(json!({
                    "content": [{ "type": "text", "text": format!("{}MIDDLE{}", "A".repeat(30000), "Z".repeat(30000)) },
                                { "type": "resource_link", "uri": "mem://a", "name": "a" },
                                { "type": "audio", "data": "AAAA", "mimeType": "audio/wav" }],
                    "structuredContent": { "n": 1 } })),
                "image" => Some(json!({
                    "content": [{ "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" }] })),
                _ => Some(
                    json!({ "content": [{ "type": "text", "text": format!("echo: {}", text_arg(&m, Some(""))) }] }),
                ),
            },
            _ => None,
        };
        print_line(&reply(id, r));
    }
}
