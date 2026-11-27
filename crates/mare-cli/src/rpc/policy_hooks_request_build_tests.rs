//! `request/build` through RPC host hooks on the real agent path: primary and fallback requests,
//! model descriptions with functional parameter rules, numeric re-encoding by the hook client,
//! fail-closed conversion (checked waterfall), and the RPC event stream (`events.subscribe`).
pub(super) use super::tests::{HOST_LOCK, rpc_read, rpc_send};
pub(super) use super::*;

pub(super) type Reader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
pub(super) type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;

pub(super) struct Rig {
    pub(super) host: Arc<Host>,
    pub(super) server: tokio::task::JoinHandle<()>,
    pub(super) cr: Reader,
    pub(super) cw: Writer,
    pub(super) _dir: tempfile::TempDir,
}

/// A host with `primary/m` → `fallback/m`, an RPC client with `capabilities.callbacks` subscribed to
/// `request/build` (waterfall), and optionally an RPC event subscription.
pub(super) async fn rig(events: Option<serde_json::Value>) -> Rig {
    rig_with_peer_hooks(events, false).await
}

pub(super) async fn rig_with_peer_hooks(events: Option<serde_json::Value>, peer_hooks: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new(mare_host::HostOptions {
        cwd: dir.path().to_owned(),
        mode: mare_plugin::Mode::Print,
        config: json!({
            "model": "primary/m",
            "fallback_models": ["fallback/m"],
            "plugins": { "permissions": { "enabled": false } },
            "peer": { "allow": if peer_hooks { vec!["agent.prompt", "agent.wait", "hooks.subscribe"] } else { vec!["agent.prompt", "agent.wait"] } },
            "public_events": if peer_hooks { vec!["request/build"] } else { vec![] },
        }),
        load_init: false,
        project_trusted: false,
        args: json!({"no_session":true}),
        no_user_plugins: true,
    })
    .await
    .unwrap();
    let (client, server) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(server);
    let server = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut cr = BufReader::new(cr);
    let mut call = async |id: i64, method: &str, params: serde_json::Value, cr: &mut Reader| {
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
        loop {
            let msg = rpc_read(cr).await;
            if msg["id"] == id {
                assert!(msg["error"].is_null(), "{method}: {msg}");
                break;
            }
        }
    };
    call(1, "initialize", json!({"protocol_version": "3.0", "capabilities":{"callbacks":true}}), &mut cr).await;
    call(
        2,
        "hooks.subscribe",
        json!({"scope":"all","event":"request/build","prepend":true,"callbacks":["handle"]}),
        &mut cr,
    )
    .await;
    if let Some(mut filter) = events {
        if let Some(obj) = filter.as_object_mut()
            && !obj.contains_key("scope")
            && !obj.contains_key("session_id")
        {
            obj.insert("scope".into(), json!("all"));
        }
        call(3, "events.subscribe", filter, &mut cr).await;
    }
    let lua = host.lua();
    lua.globals().set("rb_ctx", host.kernel.host_ctx().unwrap()).unwrap();
    lua.globals().set("rb_llm", host.service_table("llm").unwrap()).unwrap();
    Rig { host, server, cr, cw, _dir: dir }
}

/// A background client's read: no deadline (the test bounds the request), `None` at EOF.
pub(super) async fn read_until_eof(cr: &mut Reader) -> Option<serde_json::Value> {
    let mut line = String::new();
    (cr.read_line(&mut line).await.unwrap() > 0).then(|| serde_json::from_str(&line).unwrap())
}

/// Send `status.get` and read until its reply; `stream.failed`, EOF or a timeout fail the test.
async fn stream_intact_through_reply(cr: &mut Reader, cw: &mut Writer) {
    rpc_send(cw, json!({"jsonrpc":"2.0","id":"barrier","method":"status.get","params":{}})).await;
    let read = async {
        loop {
            let mut line = String::new();
            let n = cr.read_line(&mut line).await.unwrap();
            assert!(n > 0, "EOF before the barrier reply: the event stream was closed");
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_ne!(msg["method"], methods::STREAM_FAILED, "{msg}");
            if msg["id"] == "barrier" {
                assert!(msg["error"].is_null(), "{msg}");
                return;
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), read).await.expect("no barrier reply");
}

pub(super) async fn finish(host: Arc<Host>, server: tokio::task::JoinHandle<()>) {
    server.abort();
    let _ = server.await;
    host.shutdown().await;
}

/// Models with functional parameter rules (`wire`, `options[*].wire`, bool maps) and a float
/// cost reach the RPC hook as JSON; a hook client that re-encodes `1.0` as `1` (JavaScript
/// `JSON.parse`/`JSON.stringify`) still gets the original model back, the provider sees its
/// functions, and the `events.subscribe` stream delivers both builds without failing.
#[tokio::test]
async fn rpc_request_build_preserves_model_functional_rules_and_streams_events() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } =
                rig(Some(json!({"include": ["request/prepare", "request/build"]}))).await;
            host.lua()
                .load(
                    r#"
                    SEEN = {}
                    rb_llm:register_provider({
                        provider = "primary",
                        is_configured = function() return true end,
                        models = { {
                            model_id = "m", context_window = 100000, max_output = 4096,
                            cost = { input = 1.0 },
                            extra_array = mare.json.array(),
                            params = {
                                custom = { default = true, wire = { responses = function(req) req.test = true end } },
                                mode = {
                                    type = "enum", default = "fast",
                                    options = mare.json.array({
                                        { value = "fast", wire = { chat = function(req) req.fast = true end } },
                                        "deep",
                                    }),
                                },
                                flag = {
                                    type = "bool", default = true,
                                    wire = { responses = {
                                        [true] = function(req) req.flag = true end,
                                        [false] = { ["flag"] = false },
                                    } },
                                },
                            },
                        } },
                        stream = function(req)
                            assert(type(req.model.params.custom.wire.responses) == "function")
                            assert(type(req.model.params.mode.options[1].wire.chat) == "function")
                            assert(type(req.model.params.flag.wire.responses[true]) == "function")
                            assert(mare.json.encode(req.model.extra_array) == "[]")
                            SEEN[#SEEN + 1] = "primary:" .. tostring(req.system)
                            error({ message = "quota exhausted", kind = "quota", retryable = false })
                        end,
                    })
                    rb_llm:register_provider({
                        provider = "fallback",
                        is_configured = function() return true end,
                        models = { {
                            model_id = "m", context_window = 100000, max_output = 4096,
                            cost = { input = 2.0 },
                            params = { rule = { default = "on", wire = { chat = function(req) req.fallback = true end } } },
                        } },
                        stream = function(req)
                            assert(type(req.model.params.rule.wire.chat) == "function")
                            SEEN[#SEEN + 1] = "fallback:" .. tostring(req.system)
                            return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "ok" } } }
                        end,
                    })
                    rb_llm:set_model("primary/m", { save = false })
                    "#,
                )
                .exec_async().await
                .unwrap();

            let client = tokio::task::spawn_local(async move {
                let (mut hooks, mut events) = (Vec::new(), Vec::new());
                while hooks.len() < 2 || events.iter().filter(|e: &&String| e.starts_with("request/build")).count() < 2 {
                    let msg = read_until_eof(&mut cr).await.expect("EOF while the request runs");
                    assert_ne!(msg["method"], methods::STREAM_FAILED, "{msg}");
                    if msg["method"] == methods::CALLBACK_CALL && msg["params"]["arguments"]["event"] == "request/build" {
                        let mut req = msg["params"]["arguments"]["args"][0].clone();
                        let provider = req["model"]["provider"].as_str().unwrap().to_string();
                        // what a JavaScript client sends back for an untouched 1.0 / 2.0
                        let cost = req["model"]["cost"]["input"].as_f64().unwrap();
                        assert!(req["model"]["cost"]["input"].is_f64(), "{req}");
                        req["model"]["cost"]["input"] = json!(cost as i64);
                        req["system"] = json!(format!("rpc-edited-{provider}"));
                        hooks.push(provider);
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":req}})).await;
                    } else if msg["method"] == methods::EVENT {
                        let data = &msg["params"]["data"];
                        assert!(data["model"]["params"].is_object(), "{msg}");
                        events.push(format!("{}:{}", msg["params"]["type"].as_str().unwrap(), data["model"]["provider"]));
                    }
                }
                // keep the connection open: EOF on the client would cancel the running turn
                (hooks, events, cr, cw)
            });

            let result = host.prompt("hello", json!({})).await.unwrap();
            assert!(result["error"].is_null(), "{result}");
            assert_eq!(result["text"], "ok", "{result}");
            let seen: Vec<String> = host.lua().load("return SEEN").eval().unwrap();
            assert_eq!(seen, vec!["primary:rpc-edited-primary", "fallback:rpc-edited-fallback"]);
            assert!(host.kernel.problems().is_empty(), "{:?}", host.kernel.problems());
            let (hooks, events, mut cr, mut cw) =
                tokio::time::timeout(std::time::Duration::from_secs(30), client).await.unwrap().unwrap();
            assert_eq!(hooks, vec!["primary", "fallback"]);
            assert_eq!(
                events,
                vec![r#"request/prepare:"primary""#, r#"request/build:"primary""#, r#"request/build:"fallback""#],
            );
            // Barrier: every reply waits until the events queued before it are delivered
            // (`flush_events`), and a failed stream sends `stream.failed` and never this reply.
            // So everything the finished request queued (agent/end included) is read up to here.
            stream_intact_through_reply(&mut cr, &mut cw).await;
            finish(host, server).await;
        })
        .await;
}

/// A `request/build` value the RPC hook cannot receive (here a non-JSON field added by an earlier
/// in-process listener, as a peer's too-deep `output_schema` would cause) stops the request: no
/// provider call on the primary path, and no fallback call after a fallback-worthy failure.
#[tokio::test]
async fn rpc_request_build_conversion_failure_stops_primary_and_fallback_requests() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            for broken in ["primary", "fallback"] {
                let rig = rig(None).await;
                rig.host.lua().globals().set("BROKEN", broken).unwrap();
                rig.host
                    .lua()
                    .load(
                        r#"
                        SENT = {}
                        for _, name in ipairs({ "primary", "fallback" }) do
                            rb_llm:register_provider({
                                provider = name,
                                is_configured = function() return true end,
                                models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                                stream = function()
                                    SENT[#SENT + 1] = name
                                    if name == "primary" then
                                        error({ message = "quota exhausted", kind = "quota", retryable = false })
                                    end
                                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "sent" } } }
                                end,
                            })
                        end
                        rb_ctx:on("request/build", function(req, next)
                            if req.model.provider == BROKEN then
                                req.not_json = function() end
                            end
                            return next(req)
                        end, { prepend = true })
                        rb_llm:set_model("primary/m", { save = false })
                        "#,
                    )
                    .exec_async().await
                    .unwrap();
                let Rig { host, server, mut cr, mut cw, _dir } = rig;
                // answers every hook call unchanged; a call never sent never gets answered
                let client = tokio::task::spawn_local(async move {
                    while let Some(msg) = read_until_eof(&mut cr).await {
                        if msg["method"] == methods::CALLBACK_CALL {
                            let value = msg["params"]["arguments"]["args"][0].clone();
                            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":value}})).await;
                        }
                    }
                });
                let result = host.prompt("hello", json!({})).await.unwrap();
                client.abort();
                let error = result["error"]["message"].as_str().unwrap_or_default();
                assert!(error.contains("request/build") && error.contains("conversion failed"), "{broken}: {result}");
                let sent: Vec<String> = host.lua().load("return SENT").eval().unwrap();
                let expected: Vec<&str> = if broken == "primary" { vec![] } else { vec!["primary"] };
                assert_eq!(sent, expected, "{broken}: {result}");
                let _ = client.await;
                finish(host, server).await;
            }
        })
        .await;
}

/// Reproduced on the real RPC path: a restricted peer submits a valid
/// `outputSchema` whose deepest value sits at depth 64 (63 nested objects under `default`).
/// It compiles on its own, but inside the `request/build` value it is at depth 65, so the
/// primary's RPC hook cannot receive it. The request stops there: no hook call, and neither the
/// primary nor the fallback provider is asked.
#[tokio::test]
async fn peer_deep_output_schema_stops_request_build_without_provider_requests() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            host.lua()
                .load(
                    r#"
                    SENT = {}
                    for _, name in ipairs({ "primary", "fallback" }) do
                        rb_llm:register_provider({
                            provider = name,
                            is_configured = function() return true end,
                            models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                            stream = function()
                                SENT[#SENT + 1] = name
                                error({ message = "quota exhausted", kind = "quota", retryable = false })
                            end,
                        })
                    end
                    rb_llm:set_model("primary/m", { save = false })
                    "#,
                )
                .exec_async()
                .await
                .unwrap();
            let hook_calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let calls = hook_calls.clone();
            let primary = tokio::task::spawn_local(async move {
                while let Some(msg) = read_until_eof(&mut cr).await {
                    if msg["method"] == methods::CALLBACK_CALL {
                        calls.set(calls.get() + 1);
                        let value = msg["params"]["arguments"]["args"][0].clone();
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":value}})).await;
                    }
                }
            });

            let mut deepest = json!(1);
            for _ in 0..63 {
                deepest = json!({ "a": deepest });
            }
            let schema = json!({ "type": "object", "default": deepest });
            let lua = host.lua();
            let compiled = mare_shared::lua::json_to_lua(lua, &schema).unwrap();
            mare_shared::lua::strict_lua_to_json(lua, compiled.clone()).expect("depth 64 on its own is valid JSON");
            let in_request = lua.create_table().unwrap();
            in_request.set("output_schema", compiled).unwrap();
            assert!(mare_shared::lua::strict_lua_to_json(lua, mlua::Value::Table(in_request)).is_err());

            let (client, peer_server) = tokio::io::duplex(1 << 20);
            let (sr, sw) = tokio::io::split(peer_server);
            let peer_task = tokio::task::spawn_local(serve_peer(host.clone(), sr, sw, None));
            let (pr, mut pw) = tokio::io::split(client);
            let mut pr = BufReader::new(pr);
            let sid = super::super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0"}}),
            )
            .await;
            while rpc_read(&mut pr).await["id"] != 1 {}
            rpc_send(
                &mut pw,
                json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt",
                       "params":{"session_id":sid,"input":"go","output_schema":schema}}),
            )
            .await;
            let start_reply = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                loop {
                    let msg = read_until_eof(&mut pr).await.expect("EOF before the prompt reply");
                    if msg["id"] == 2 {
                        return msg;
                    }
                }
            })
            .await
            .expect("no prompt reply");
            let reply = if let Some(turn_id) = start_reply["result"]["turn_id"].as_str() {
                rpc_send(
                    &mut pw,
                    json!({"jsonrpc":"2.0","id":3,"method":"agent.wait",
                           "params":{"session_id":sid,"turn_id":turn_id}}),
                )
                .await;
                tokio::time::timeout(std::time::Duration::from_secs(60), async {
                    loop {
                        let msg = read_until_eof(&mut pr).await.expect("EOF before the wait reply");
                        if msg["id"] == 3 {
                            return msg;
                        }
                    }
                })
                .await
                .expect("no wait reply")
            } else {
                start_reply
            };
            let text = reply.to_string();
            assert!(!text.contains("invalid output_schema"), "the schema itself is valid: {reply}");
            assert!(
                (text.contains("request/build") && text.contains("conversion failed"))
                    || (reply["error"]["data"]["code"] == "invalid_arguments"
                        && text.contains("schema validation resource limit exceeded")),
                "{reply}"
            );
            let sent: Vec<String> = lua.load("return SENT").eval().unwrap();
            assert!(sent.is_empty(), "provider requests were sent: {sent:?}");

            assert_eq!(hook_calls.get(), 0, "the primary's hook received the request");
            primary.abort();
            let _ = primary.await;
            drop((pr, pw));
            peer_task.abort();
            let _ = peer_task.await;
            finish(host, server).await;
        })
        .await;
}

/// A checked waterfall over a model table (function-valued parameter
/// rules) through an RPC hook (request/build, the protocol's remote waterfall). Returned as is
/// (`{ next = true, value = M }`), the next listener and the caller get the original table,
/// functions included; changed, the waterfall fails instead of passing on a model that silently
/// lost its functions.
#[tokio::test]
async fn rpc_waterfall_over_a_model_table_round_trips_or_fails() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            let change = std::rc::Rc::new(std::cell::Cell::new(false));
            let rename = change.clone();
            let client = tokio::task::spawn_local(async move {
                while let Some(msg) = read_until_eof(&mut cr).await {
                    if msg["method"] == methods::CALLBACK_CALL {
                        let mut value = msg["params"]["arguments"]["args"][0].clone();
                        if rename.get() {
                            value["name"] = json!("renamed");
                        }
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"next":true,"value":value}}))
                            .await;
                    }
                }
            });
            // a listener after the RPC hook sees what the hook handed on
            host.lua()
                .load(
                    r#"NEXT_SAW = nil
                    rb_ctx:on("request/build", function(m, next)
                        NEXT_SAW = type(m.params.custom.wire.responses) == "function"
                        return next(m)
                    end)"#,
                )
                .exec()
                .unwrap();
            let run = r#"
                NEXT_SAW = nil
                local M = { id = "m", provider = "p", name = "M",
                    params = { custom = { default = true, wire = { responses = function(r) r.x = true end } } } }
                local ok, out = pcall(function() return rb_ctx:get("bus"):waterfall_checked("request/build", M) end)
                if not ok then return "error: " .. tostring(out) end
                return (out == M and type(out.params.custom.wire.responses) == "function") and "same" or "changed"
            "#;
            let same: String = host.lua().load(run).eval_async().await.unwrap();
            assert_eq!(same, "same", "returned as is: the original model");
            let next: Option<bool> = host.lua().load("return NEXT_SAW").eval().unwrap();
            assert_eq!(next, Some(true), "the next listener got the original table, functions included");
            change.set(true);
            let changed: String = host.lua().load(run).eval_async().await.unwrap();
            assert!(changed.starts_with("error:") && changed.contains("functions"), "{changed}");
            client.abort();
            finish(host, server).await;
        })
        .await;
}

/// a policy vote is not a replacement value. With a current model whose parameter
/// rules hold functions, an RPC `request/ready` hook voting `{"decision":"allow"}` lets the request
/// go: the vote is not required to carry the model back.
#[tokio::test]
async fn rpc_policy_vote_allows_a_request_whose_model_rules_hold_functions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"hooks.subscribe",
                       "params":{"scope":"all","event":"request/ready","callbacks":["handle"]}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 9 {}
            host.lua()
                .load(
                    r#"
                    rb_llm:register_provider({
                        provider = "primary",
                        is_configured = function() return true end,
                        models = { { model_id = "m", context_window = 100000, max_output = 4096,
                            params = { custom = { default = true, wire = { responses = function(r) r.x = true end } } } } },
                        stream = function()
                            return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "sent" } } }
                        end,
                    })
                    rb_llm:set_model("primary/m", { save = false })
                    "#,
                )
                .exec_async().await
                .unwrap();
            let votes = std::rc::Rc::new(std::cell::Cell::new(0));
            let counted = votes.clone();
            let client = tokio::task::spawn_local(async move {
                while let Some(msg) = read_until_eof(&mut cr).await {
                    if msg["method"] == methods::CALLBACK_CALL {
                        let args = &msg["params"]["arguments"];
                        let answer = if args["event"] == "request/ready" {
                            counted.set(counted.get() + 1);
                            json!({"decision":"allow"})
                        } else {
                            args["args"][0].clone()
                        };
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":answer}})).await;
                    }
                }
            });
            let result = host.prompt("hello", json!({})).await.unwrap();
            assert!(result["error"].is_null(), "{result}");
            assert_eq!(result["text"], "sent", "{result}");
            assert!(votes.get() > 0, "the RPC policy hook voted");
            client.abort();
            finish(host, server).await;
        })
        .await;
}

/// An out-of-process policy voter on request/ready is asked before the provider request and its
/// deny stops the turn (it was registered but never asked when the host's decision folded only the
/// engine's listeners — the remote voter was bypassed).
#[tokio::test]
async fn rpc_policy_voter_on_request_ready_is_asked_and_its_deny_stops_the_turn() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"hooks.subscribe",
                       "params":{"scope":"all","event":"request/ready","callbacks":["handle"]}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 9 {}
            let streamed = std::rc::Rc::new(std::cell::Cell::new(false));
            let flag = streamed.clone();
            let streamed_fn = host
                .lua()
                .create_function(move |_, ()| {
                    flag.set(true);
                    Ok(())
                })
                .unwrap();
            host.lua().globals().set("STREAMED", streamed_fn).unwrap();
            host.lua()
                .load(
                    r#"
                    rb_llm:register_provider({
                        provider = "primary",
                        is_configured = function() return true end,
                        models = { { model_id = "m", context_window = 100000, max_output = 4096 } },
                        stream = function()
                            STREAMED()
                            return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "sent" } } }
                        end,
                    })
                    rb_llm:set_model("primary/m", { save = false })
                    "#,
                )
                .exec_async().await
                .unwrap();
            let votes = std::rc::Rc::new(std::cell::Cell::new(0));
            let counted = votes.clone();
            let client = tokio::task::spawn_local(async move {
                while let Some(msg) = read_until_eof(&mut cr).await {
                    if msg["method"] == methods::CALLBACK_CALL {
                        let args = &msg["params"]["arguments"];
                        let answer = if args["event"] == "request/ready" {
                            assert_eq!(args["mode"], "policy", "{msg}");
                            counted.set(counted.get() + 1);
                            json!({"decision":"deny","reason":"remote says no"})
                        } else {
                            args["args"][0].clone()
                        };
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":answer}})).await;
                    }
                }
            });
            let result = host.prompt("hello", json!({})).await.unwrap();
            assert!(votes.get() > 0, "the remote voter on request/ready was not asked: {result}");
            assert!(!streamed.get(), "the provider request went out despite the deny: {result}");
            assert!(result["error"].to_string().contains("remote says no"), "{result}");
            client.abort();
            finish(host, server).await;
        })
        .await;
}

/// A policy subscription to something that is not a decision point is a parameter error naming
/// the points (not an internal error).
#[tokio::test]
async fn rpc_policy_subscription_to_an_unknown_point_is_invalid_params() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":9,"method":"hooks.subscribe",
                       "params":{"scope":"all","event":"custom/not-a-point","callbacks":["handle"]}}),
            )
            .await;
            let reply = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 9 {
                    break msg;
                }
            };
            assert_eq!(reply["error"]["code"], json!(codes::INVALID_PARAMS), "{reply}");
            let message = reply["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains("does not allow remote participants"), "{reply}");
            finish(host, server).await;
        })
        .await;
}

#[path = "policy_hooks_next_tests.rs"]
mod next_tests;
