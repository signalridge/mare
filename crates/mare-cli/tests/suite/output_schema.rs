//! Structured final output (`--output-schema`, RPC `prompt { outputSchema }`, Lua `LlmRequest` /
//! `agent:prompt`, and provider request wire forwarding).

use std::process::Stdio;

use crate::common::*;
use serde_json::{Value, json};

fn sample_schema() -> Value {
    json!({
        "type": "object",
        "required": ["answer", "count"],
        "additionalProperties": false,
        "properties": {
            "answer": { "type": "string", "minLength": 1 },
            "count": { "type": "integer", "minimum": 1 }
        }
    })
}

fn run_cli(
    home: &std::path::Path,
    cwd: &std::path::Path,
    args: &[&str],
    faux_script: Option<&Value>,
) -> (Option<i32>, String, String) {
    let mut cmd = mare_cmd();
    cmd.args(args)
        .env("MARE_HOME", home)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let script_file = faux_script.map(|script| {
        let path = home.join("faux-script.json");
        std::fs::write(&path, script.to_string()).unwrap();
        cmd.env("MARE_FAUX_SCRIPT", &path);
        path
    });
    let out = cmd.run_output();
    drop(script_file);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn cli_output_schema_usage_errors() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();

    // Missing schema file -> exit 2 in both -p and --json.
    let (code, _, err) =
        run_cli(h, h, &["-p", "--model", "faux/echo", "--no-session", "--output-schema", "missing.json", "hi"], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--output-schema"), "{err}");

    let (code, out, err) = run_cli(
        h,
        h,
        &["--json", "--model", "faux/echo", "--no-session", "--output-schema", "missing.json", "hi"],
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    let last: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!((last["type"].as_str(), last["data"]["reason"].as_str()), (Some("settled"), Some("usage")), "{last}");

    // Invalid JSON in schema file -> exit 2.
    let bad_json = h.join("bad.json");
    std::fs::write(&bad_json, "{not json").unwrap();
    let (code, _, err) = run_cli(
        h,
        h,
        &["-p", "--model", "faux/echo", "--no-session", "--output-schema", bad_json.to_str().unwrap(), "hi"],
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("invalid JSON"), "{err}");

    // Invalid schema keyword -> exit 2.
    let bad_schema = h.join("bad-schema.json");
    std::fs::write(&bad_schema, json!({ "type": "bogus" }).to_string()).unwrap();
    let (code, _, err) = run_cli(
        h,
        h,
        &["-p", "--model", "faux/echo", "--no-session", "--output-schema", bad_schema.to_str().unwrap(), "hi"],
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("invalid schema"), "{err}");

    // Headless only: refused with --rpc and --mode rpc.
    let valid_schema = h.join("schema.json");
    std::fs::write(&valid_schema, sample_schema().to_string()).unwrap();
    let (code, _, _) = run_cli(h, h, &["--rpc", "--output-schema", valid_schema.to_str().unwrap()], None);
    assert_eq!(code, Some(2));
    let (code, _, err) = run_cli(h, h, &["--mode", "rpc", "--output-schema", valid_schema.to_str().unwrap()], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("headless"), "{err}");
}

#[test]
fn cli_output_schema_enforces_validation_in_print_and_json_modes() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let schema_path = h.join("schema.json");
    std::fs::write(&schema_path, sample_schema().to_string()).unwrap();
    let schema_arg = schema_path.to_str().unwrap();

    // 1. Valid JSON matching schema succeeds in -p and --json (with settled.structured).
    let valid_script = json!([{ "text": r#"{"answer":"ok","count":2}"# }]);
    let (code, out, err) = run_cli(
        h,
        h,
        &["-p", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "give json"],
        Some(&valid_script),
    );
    assert_eq!(code, Some(0), "{err}");
    let parsed: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed, json!({ "answer": "ok", "count": 2 }));

    let (code, out, err) = run_cli(
        h,
        h,
        &["--json", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "give json"],
        Some(&valid_script),
    );
    assert_eq!(code, Some(0), "{err}");
    let lines: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let settled = lines.last().unwrap();
    assert_eq!(settled["type"], "settled", "{settled}");
    assert_eq!(settled["data"]["exit_code"], 0, "{settled}");
    assert_eq!(settled["data"]["reason"], "completed", "{settled}");
    assert_eq!(settled["data"]["structured"], json!({ "answer": "ok", "count": 2 }), "{settled}");

    // 2. Non-JSON text (`faux/echo` echoing plain text) fails in both -p and --json instead of
    // returning event JSONL as a passing result.
    let (code, out, err) =
        run_cli(h, h, &["-p", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "hello"], None);
    assert_eq!(code, Some(1), "stderr={err}");
    assert_eq!(out, "", "invalid output must not be printed as -p stdout: {out}");
    assert!(err.contains("output schema validation failed"), "{err}");

    let (code, out, err) = run_cli(
        h,
        h,
        &["--json", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "hello"],
        None,
    );
    assert_eq!(code, Some(1), "stderr={err}");
    let lines: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.iter().any(|l| l["type"] == "turn.failed"), "{out}");
    let settled = lines.last().unwrap();
    assert_eq!(settled["type"], "settled", "{settled}");
    assert_eq!(settled["data"]["exit_code"], 1, "{settled}");
    assert_eq!(settled["data"]["reason"], "error", "{settled}");
    assert!(settled["data"]["structured"].is_null(), "{settled}");
    assert!(settled["data"]["error"]["code"].is_string(), "{settled}");
    assert!(
        settled["data"]["error"]["message"].as_str().unwrap().contains("output schema validation failed"),
        "{settled}"
    );

    // 3. Valid JSON that violates schema constraints fails validation too.
    let mismatch_script = json!([{ "text": r#"{"answer":"ok","count":0}"# }]);
    let (code, out, err) = run_cli(
        h,
        h,
        &["--json", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "give json"],
        Some(&mismatch_script),
    );
    assert_eq!(code, Some(1), "stderr={err}");
    let settled: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(settled["data"]["exit_code"], 1, "{settled}");
    assert!(settled["data"]["error"]["code"].is_string(), "{settled}");
    assert!(settled["data"]["error"]["message"].as_str().unwrap().contains("minimum"), "{settled}");
}

#[tokio::test]
async fn cli_output_schema_works_with_continue_turn() {
    local(async {
        let env = TestEnv::new().await;
        env.write_durable_session(
            "interrupted.jsonl",
            &[json!({
                "type": "message",
                "entry_id": "m1",
                "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": "continue me" }] }
            })],
        );
        let schema_path = env.cwd.join("schema.json");
        std::fs::write(&schema_path, sample_schema().to_string()).unwrap();

        let (code, out, err) = run_cli(
            env.home.path(),
            &env.cwd,
            &["-p", "--model", "faux/echo", "-c", "--output-schema", schema_path.to_str().unwrap()],
            Some(&json!([{ "text": r#"{"answer":"resumed","count":3}"# }])),
        );
        assert_eq!(code, Some(0), "{err}");
        let parsed: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(parsed, json!({ "answer": "resumed", "count": 3 }));
    })
    .await;
}

#[test]
fn rpc_prompt_output_schema_validates_and_enforces_constraints() {
    let home = tempfile::tempdir().unwrap();
    let faux_script = home.path().join("faux.json");
    std::fs::write(
        &faux_script,
        json!([
            { "text": r#"{"answer":"ok","count":5}"# },
            { "text": r#"{"answer":"","count":5}"# }
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    // Invalid schema is rejected with INVALID_PARAMS (-32602).
    let bad = c.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "hi", "output_schema": { "type": "nope" } }),
        |_| json!(null),
    );
    assert_eq!(bad["error"]["code"], -32602, "{bad}");

    // Schema with numbers that cannot be losslessly represented in Lua is rejected with INVALID_PARAMS (-32602)
    // instead of silently rounding to f64.
    let imprecise_schema: Value = serde_json::from_str(
        r#"{"type":"object","properties":{"n":{"type":"integer","multipleOf":200000000000000000000000000000000000004}}}"#,
    )
    .unwrap();
    let imprecise = c.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "hi", "output_schema": imprecise_schema }),
        |_| json!(null),
    );
    assert_eq!(imprecise["error"]["code"], -32602, "{imprecise}");
    assert!(imprecise["error"]["message"].as_str().unwrap().contains("losslessly"), "{imprecise}");

    // Valid structured turn returns `result.structured`.
    let schema = sample_schema();
    let started_ok = c.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "hi", "output_schema": schema, "request_id": "req-1" }),
        |_| json!(null),
    );
    assert!(started_ok["error"].is_null(), "{started_ok}");
    let ok = c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": started_ok["result"]["turn_id"] }), |_| {
        json!(null)
    });
    let res = &ok["result"];
    assert!(res["error"].is_null(), "{ok}");
    assert_eq!(res["structured"], json!({ "answer": "ok", "count": 5 }), "{ok}");

    // Reusing `request_id` with a different `output_schema` triggers `request_id_conflict`.
    let conflict = c.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "hi", "output_schema": { "type": "object" }, "request_id": "req-1" }),
        |_| json!(null),
    );
    assert_eq!(conflict["error"]["data"]["code"], "request_id_conflict", "{conflict}");

    // Output violating schema fails with `error_kind == "invalid_output"`.
    let started_fail =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "output_schema": schema }), |_| {
            json!(null)
        });
    assert!(started_fail["error"].is_null(), "{started_fail}");
    let failed =
        c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": started_fail["result"]["turn_id"] }), |_| {
            json!(null)
        });
    let fres = &failed["result"];
    assert_eq!(fres["error_kind"], "invalid_output", "{failed}");
    assert!(fres["structured"].is_null(), "{failed}");
    assert!(fres["error"]["message"].as_str().unwrap().contains("output schema validation failed"), "{failed}");

    c.shutdown_and_wait();
}

#[test]
fn rpc_prompt_rejects_output_schema_when_busy() {
    let home = tempfile::tempdir().unwrap();
    // A slow faux turn keeps the agent busy while we issue a second prompt with outputSchema.
    let faux_script = home.path().join("faux-busy.json");
    std::fs::write(&faux_script, json!([{ "text": "done", "delay_ms": 400 }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c2 = RpcClient::spawn_cmd(&mut cmd);
    let init2 = c2.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid2 = init2["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let first = c2.call_raw("agent.prompt", json!({ "session_id": sid2, "input": "first" }), |_| json!(null));
    assert_eq!(first["result"]["status"], "started", "{first}");
    let busy = c2.call_raw(
        "agent.prompt",
        json!({ "session_id": sid2, "input": "second", "output_schema": sample_schema() }),
        |_| json!(null),
    );
    assert_eq!(busy["error"]["code"], -32602, "{busy}");
    assert!(busy["error"]["message"].as_str().unwrap().contains("busy"), "{busy}");
    c2.call_raw("agent.cancel", json!({ "session_id": sid2 }), |_| json!(null));
    c2.shutdown_and_wait();
}

#[tokio::test]
async fn lua_agent_prompt_and_llm_request_validate_and_forward_to_all_providers() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        for (k, v) in [
            ("OPENAI_API_KEY", "test-openai"),
            ("ANTHROPIC_API_KEY", "test-anthropic"),
            ("DEEPSEEK_API_KEY", "test-deepseek"),
            ("GEMINI_API_KEY", "test-google"),
        ] {
            env.set_var(k, v);
        }
        let base = &server.base;
        let openai_sse = [
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": r#"{"answer":"openai","count":1}"# }]
                }
            }),
            json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 5, "output_tokens": 3 } }
            }),
        ]
        .iter()
        .map(|ev| format!("event: {}\ndata: {ev}\n\n", ev["type"].as_str().unwrap()))
        .collect::<String>();
        let compat_sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "content": r#"{"answer":"compat","count":2}"# }, "finish_reason": null }] }),
            json!({ "id": "c1", "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] })
        );
        let anthropic_sse = [
            ("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":1}}})),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":r#"{"answer":"anthropic","count":3}"#}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})),
            ("message_stop", json!({"type":"message_stop"})),
        ]
        .iter()
        .map(|(t, d)| format!("event: {t}\ndata: {d}\n\n"))
        .collect::<String>();
        let google_sse = format!(
            "data: {}\n\n",
            json!({
                "candidates": [{
                    "content": { "role": "model", "parts": [{ "text": r#"{"answer":"google","count":4}"# }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 2 }
            })
        );

        server.route("/openai", vec![Resp::sse(openai_sse)]);
        server.route("/compat", vec![Resp::sse(compat_sse)]);
        server.route("/anthropic", vec![Resp::sse(anthropic_sse)]);
        server.route("/google", vec![Resp::sse(google_sse)]);

        let h = env
            .host()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "providers": {
                    "compat": {
                        "wire": "openai-chat",
                        "base_url": format!("{base}/compat"),
                        "api_key": "test-compat",
                        "models": [{ "model_id": "chat-model", "context_window": 128000, "max_output": 8192 }]
                    }
                },
                "plugins": {
                    "provider-openai": { "base_url": format!("{base}/openai") },
                    "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                    "provider-google": { "base_url": format!("{base}/google") },
                    "llm": { "retries": 0 },
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());

        let raw: mlua::Value = eval(
            &h,
            r#"
            local schema = {
                type = "object",
                required = { "answer", "count" },
                additionalProperties = false,
                properties = {
                    answer = { type = "string", minLength = 1 },
                    count = { type = "integer", minimum = 1 },
                },
            }
            local bad_ok, bad_err = pcall(function()
                return A:prompt("hi", { output_schema = { type = "bogus" } })
            end)
            local bad_agent = { ok = bad_ok, error = bad_err and tostring(bad_err) or nil }
            local bad_llm = LLM:stream({
                model = assert(LLM:resolve("openai/gpt-6.1-terra")),
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                output_schema = { properties = "not-an-object" },
            }, function() end)

            LLM:set_model("openai/gpt-6.1-terra", { save = false })
            local started_openai = A:prompt("hi", { output_schema = schema })
            local ok_openai = A:wait(started_openai.turn_id)

            local function stream_one(ref)
                return LLM:stream({
                    model = assert(LLM:resolve(ref)),
                    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                    output_schema = schema,
                }, function() end)
            end
            local msg_compat = stream_one("compat/chat-model")
            local msg_anthropic = stream_one("anthropic/claude-sonnet-4-6")
            local msg_google = stream_one("google/gemini-2.5-flash")
            return {
                bad_agent = bad_agent,
                bad_llm = bad_llm,
                ok_openai = ok_openai,
                msg_compat = msg_compat,
                msg_anthropic = msg_anthropic,
                msg_google = msg_google,
            }
            "#,
        )
        .await;
        let result = mare_shared::lua::lua_to_json(h.lua(), raw).unwrap();

        assert_eq!(result["bad_agent"]["ok"], false, "{result}");
        assert!(
            result["bad_agent"]["error"].as_str().unwrap_or("").contains("invalid output_schema"),
            "{result}"
        );
        assert_eq!(result["bad_llm"]["stop_reason"], "error", "{result}");
        assert_eq!(result["bad_llm"]["error_kind"], "invalid", "{result}");
        assert!(result["ok_openai"]["error"].is_null(), "{result}");
        assert_eq!(result["ok_openai"]["structured"], json!({ "answer": "openai", "count": 1 }), "{result}");
        assert!(result["msg_compat"]["error"].is_null(), "{result}");
        assert!(result["msg_anthropic"]["error"].is_null(), "{result}");
        assert!(result["msg_google"]["error"].is_null(), "{result}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 4, "{reqs:?}");
        let expected_schema = sample_schema();

        let openai_body = reqs[0].json();
        assert_eq!(openai_body["text"]["format"]["type"], "json_schema", "{openai_body}");
        assert_eq!(openai_body["text"]["format"]["strict"], true, "{openai_body}");
        assert_eq!(openai_body["text"]["format"]["schema"], expected_schema, "{openai_body}");
        assert!(openai_body["text"].get("verbosity").is_some(), "params must preserve text.verbosity: {openai_body}");

        let compat_body = reqs[1].json();
        assert_eq!(compat_body["response_format"]["type"], "json_schema", "{compat_body}");
        assert_eq!(compat_body["response_format"]["json_schema"]["strict"], true, "{compat_body}");
        assert_eq!(compat_body["response_format"]["json_schema"]["schema"], expected_schema, "{compat_body}");

        let anthropic_body = reqs[2].json();
        assert_eq!(anthropic_body["output_config"]["format"]["type"], "json_schema", "{anthropic_body}");
        assert_eq!(
            anthropic_body["output_config"]["format"]["schema"],
            json!({
                "type": "object",
                "required": ["answer", "count"],
                "additionalProperties": false,
                "properties": {
                    "answer": { "type": "string", "description": "min length: 1." },
                    "count": { "type": "integer", "description": "minimum: 1." },
                },
            }),
            "{anthropic_body}"
        );

        let google_body = reqs[3].json();
        assert_eq!(google_body["generationConfig"]["responseMimeType"], "application/json", "{google_body}");
        assert_eq!(google_body["generationConfig"]["responseJsonSchema"], expected_schema, "{google_body}");

        h.shutdown().await;
    })
    .await
}

#[test]
fn cli_output_schema_enforces_exact_large_integer_and_fractional_bounds() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let schema_path = h.join("large-int-schema.json");
    std::fs::write(
        &schema_path,
        json!({
            "type": "object",
            "required": ["big", "min_i64", "max_i64", "frac"],
            "additionalProperties": false,
            "properties": {
                "big": { "type": "integer", "minimum": 9_007_199_254_740_993_i64, "multipleOf": 1 },
                "min_i64": { "type": "integer", "minimum": i64::MIN, "maximum": i64::MIN },
                "max_i64": { "type": "integer", "minimum": i64::MAX, "maximum": i64::MAX },
                "frac": { "type": "number", "minimum": -1.5, "maximum": 4_503_599_627_370_495.5_f64 }
            }
        })
        .to_string(),
    )
    .unwrap();
    let schema_arg = schema_path.to_str().unwrap();

    // Exact boundary values pass.
    let ok_payload = format!(r#"{{"big":9007199254740993,"min_i64":{},"max_i64":{},"frac":-1}}"#, i64::MIN, i64::MAX);
    let (code, out, err) = run_cli(
        h,
        h,
        &["-p", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "ok"],
        Some(&json!([{ "text": ok_payload }])),
    );
    assert_eq!(code, Some(0), "stderr={err}");
    let parsed: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed["big"], 9_007_199_254_740_993_i64);

    // 9007199254740992 must be rejected by minimum 9007199254740993.
    let bad_big = format!(r#"{{"big":9007199254740992,"min_i64":{},"max_i64":{},"frac":-1}}"#, i64::MIN, i64::MAX);
    let (code, out, err) = run_cli(
        h,
        h,
        &["--json", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "bad big"],
        Some(&json!([{ "text": bad_big }])),
    );
    assert_eq!(code, Some(1), "stderr={err}");
    let settled: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert!(settled["data"]["error"]["message"].as_str().unwrap().contains("minimum"), "{settled}");

    // i64::MAX - 1 and fractional overflow must be rejected.
    let bad_i64 = format!(
        r#"{{"big":9007199254740993,"min_i64":{},"max_i64":{},"frac":4503599627370496}}"#,
        i64::MIN,
        i64::MAX - 1
    );
    let (code, out, err) = run_cli(
        h,
        h,
        &["--json", "--model", "faux/echo", "--no-session", "--output-schema", schema_arg, "bad bounds"],
        Some(&json!([{ "text": bad_i64 }])),
    );
    assert_eq!(code, Some(1), "stderr={err}");
    let settled: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert!(
        settled["data"]["error"]["message"].as_str().unwrap().contains("output schema validation failed"),
        "{settled}"
    );
}

#[test]
fn rpc_extreme_exponent_schemas_do_not_panic_and_reject_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let faux_script = home.path().join("faux.json");
    std::fs::write(&faux_script, json!([{ "text": r#"{"x":0}"# }]).to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init =
        c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }), |_| {
            json!(null)
        });
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    // Extreme negative exponent in output_schema (raw JSON literal) must not panic the RPC task
    // and must be rejected because 1.0e-9223372036854775808 cannot be represented losslessly in Lua.
    let raw_prompt = format!(
        r#"{{"jsonrpc":"2.0","id":9001,"method":"agent.prompt","params":{{"session_id":{sid_json},"input":"hi","output_schema":{{"type":"object","default":1.0e-9223372036854775808,"properties":{{"x":{{"type":"number","multipleOf":1e-308,"minimum":-1e308,"maximum":1e308}}}}}}}}}}"#,
        sid_json = json!(sid)
    );
    c.write_raw_line(&raw_prompt);
    let resp = c.wait_raw(9001, |_| json!(null));
    assert_eq!(resp["error"]["code"], -32602, "expected invalidParams rejection: {resp}");

    // Exponent beyond i128 bounds must also be rejected cleanly without panicking.
    let raw_huge_exp = format!(
        r#"{{"jsonrpc":"2.0","id":9002,"method":"agent.prompt","params":{{"session_id":{sid_json},"input":"hi","output_schema":{{"type":"object","default":1.0e-999999999999999999999999999999999999999,"properties":{{"x":{{"type":"number"}}}}}}}}}}"#,
        sid_json = json!(sid)
    );
    c.write_raw_line(&raw_huge_exp);
    let huge_resp = c.wait_raw(9002, |_| json!(null));
    assert_eq!(huge_resp["error"]["code"], -32602, "expected invalidParams rejection: {huge_resp}");

    // Extreme negative exponent in services.register schema (exercises json_to_lua_lossless) must be rejected cleanly.
    let raw_svc = r#"{"jsonrpc":"2.0","id":9003,"method":"services.register","params":{"name":"ext","methods":{"ping":{"doc":"ping","parameters":{"type":"object","default":1.0e-9223372036854775808},"result":{"type":"object"}}}}}"#;
    c.write_raw_line(raw_svc);
    let svc_resp = c.wait_raw(9003, |_| json!(null));
    assert!(svc_resp.get("error").is_some(), "expected lossless rejection: {svc_resp}");

    // Non-lossless integer in services.register is cleanly rejected with an RPC error.
    let bad_svc = c.call_raw(
        "services.register",
        json!({
            "name": "badext",
            "methods": {
                "ping": {
                    "doc": "ping",
                    "parameters": { "type": "object", "properties": { "n": { "const": u64::MAX } } },
                    "result": { "type": "object" }
                }
            }
        }),
        |_| json!(null),
    );
    assert!(bad_svc.get("error").is_some(), "expected lossless rejection: {bad_svc}");

    // Connection remains healthy after rejected extreme-exponent requests:
    // exact-representable numbers in output_schema and services.register succeed.
    let ok_prompt = format!(
        r#"{{"jsonrpc":"2.0","id":9004,"method":"agent.prompt","params":{{"session_id":{sid_json},"input":"hi","output_schema":{{"type":"object","default":{{"x":0.5}},"properties":{{"x":{{"type":"number","multipleOf":1e-308,"minimum":-1e308,"maximum":1e308}}}}}}}}}}"#,
        sid_json = json!(sid)
    );
    c.write_raw_line(&ok_prompt);
    let started_ok = c.wait_raw(9004, |_| json!(null));
    assert!(started_ok.get("error").is_none(), "{started_ok}");
    let ok_resp =
        c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": started_ok["result"]["turn_id"] }), |_| {
            json!(null)
        });
    assert!(ok_resp.get("error").is_none(), "{ok_resp}");
    assert_eq!(ok_resp["result"]["structured"], json!({ "x": 0 }), "{ok_resp}");

    let ok_svc = c.call_raw(
        "services.register",
        json!({
            "name": "okext",
            "methods": {
                "ping": {
                    "doc": "ping",
                    "parameters": { "type": "object", "default": { "x": 0.5 } },
                    "result": { "type": "object" }
                }
            }
        }),
        |_| json!(null),
    );
    assert!(ok_svc.get("error").is_none(), "{ok_svc}");

    let list = c.call_raw("services.list", json!({}), |_| json!(null));
    assert!(list.get("error").is_none(), "{list}");

    c.shutdown_and_wait();
}
