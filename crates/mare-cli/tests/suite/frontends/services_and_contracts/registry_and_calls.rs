use super::*;

#[test]
fn generic_service_rpc_registration_callbacks_and_owned_lifecycle() {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    enum ServiceReply {
        Result(Value),
        Silent,
    }
    type ServiceHandler = Box<dyn FnMut(&mut crate::common::rpc::RpcClient, &Value) -> ServiceReply + Send>;

    let callbacks: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let default_handler = || -> ServiceHandler {
        Box::new(|_, msg| {
            let reg_id = msg["params"]["registration_id"].as_str().unwrap();
            let text = msg["params"]["arguments"]["text"].as_str().unwrap();
            ServiceReply::Result(json!(format!("{reg_id}:{text}")))
        })
    };
    let handler: Arc<Mutex<ServiceHandler>> = Arc::new(Mutex::new(default_handler()));

    let schema = |properties: Value, required: &[&str]| -> Value {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    };

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        "mare.setup { peer = { allow = {\"services.list\",\"registrations.release\",\"rpc-demo.echo\",\"rpc-wide.open\"} } }\n",
    )
    .unwrap();
    std::fs::write(
        home.join("config/plugins/variant-probe.lua"),
        r#"local variant={oneOf={
  {type="object",properties={kind={const="a"},x={type="array"}},required={"kind","x"},additionalProperties=false},
  {type="object",properties={kind={const="b"},x={type="object"}},required={"kind","x"},additionalProperties=false}}}
return {name="variant-probe",provide={"variant-probe"},contracts={["variant-probe"]={methods={get={doc="variant",expose="both",
    params={type="object",properties={kind={type="string"}},required={"kind"},additionalProperties=false},
    order={"kind"},result=variant},
    take={doc="shapes",expose="both",params={type="object",properties={x={type="object"},ys={type="array"}},required={"x","ys"},additionalProperties=false},
      order={"x","ys"},result={type="string"}}}}},apply=function(ctx)
    local Probe=require("cordis").class("Probe",mare.Service)
    function Probe:constructor(c) Probe.super.constructor(self,c,"variant-probe") end
    function Probe:get(kind) return {kind=kind,x={}} end
    function Probe:take(x,ys) return "taken" end
    ctx:plugin(Probe)
end}
"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--yolo", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    let cb_clone = callbacks.clone();
    let h_clone = handler.clone();
    client.on_method = Some(Box::new(move |c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            cb_clone.lock().unwrap().push(msg.clone());
            let reply = (h_clone.lock().unwrap())(c, msg);
            if let ServiceReply::Result(val) = reply {
                c.send(json!({ "id": msg["id"], "result": val }));
            }
            return true;
        }
        false
    }));

    let owner_only = json!({
        "parameters": schema(json!({ "text": { "type": "string" } }), &["text"]),
        "result": { "type": "string" },
        "order": ["text"],
    });
    let mut echo = owner_only.clone();
    echo["access"] = json!("peer");
    let roundtrip = json!({
        "parameters": schema(json!({ "value": {} }), &["value"]),
        "result": {},
        "order": ["value"],
    });
    let definition = json!({
        "name": "rpc-demo",
        "methods": { "echo": echo, "roundtrip": roundtrip },
    });

    let refused = |c: &mut crate::common::rpc::RpcClient, method: &str, params: Value, text: Option<&str>| {
        let err = c.call_err(method, params);
        if let Some(expected) = text {
            assert!(err.to_string().contains(expected), "expected {expected:?} in {err}");
        }
    };
    let invoke = |c: &mut crate::common::rpc::RpcClient, method: &str, arguments: Value| -> Value {
        c.call(&format!("rpc-demo.{method}"), arguments)
    };

    let initialized = client.call(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 1500 }),
    );
    let advertised = initialized["capabilities"]["methods"].as_array().unwrap();
    for method in ["services.list", "services.register", "registrations.release"] {
        assert!(advertised.iter().any(|m| m == method), "{initialized}");
    }
    for removed in ["services.call", "services.unregister"] {
        assert!(!advertised.iter().any(|m| m == removed), "{removed} should be removed: {initialized}");
    }
    for (kind, x) in [("a", json!([])), ("b", json!({}))] {
        let got = client.call("variant-probe.get", json!({ "kind": kind }));
        assert_eq!(got, json!({ "kind": kind, "x": x }), "{got}");
    }
    assert_eq!(client.call("variant-probe.take", json!({ "x": {}, "ys": [] })), json!("taken"));
    refused(&mut client, "variant-probe.take", json!({ "x": [], "ys": [] }), Some("invalid_arguments"));
    refused(&mut client, "variant-probe.take", json!({ "x": {}, "ys": {} }), Some("invalid_arguments"));

    let either = json!({ "oneOf": [{ "oneOf": [{}] }, { "type": "array", "const": [1] }] });
    let echo_v = json!({
        "parameters": schema(json!({ "v": either }), &["v"]),
        "result": either,
        "order": ["v"],
    });
    let either_service =
        client.call("services.register", json!({ "name": "rpc-either", "methods": { "echo": echo_v } }));
    *handler.lock().unwrap() = Box::new(|_, msg| ServiceReply::Result(msg["params"]["arguments"]["v"].clone()));
    let forwarded = client.call("rpc-either.echo", json!({ "v": {} }));
    assert_eq!(callbacks.lock().unwrap().last().unwrap()["params"]["arguments"], json!({ "v": {} }));
    assert_eq!(forwarded, json!({}));
    *handler.lock().unwrap() = default_handler();
    assert_eq!(
        client.call("registrations.release", json!({ "registration_id": either_service["registration_id"] })),
        json!({})
    );

    for (field, value) in [("access", "owner"), ("scope", "global"), ("expose", "sideways")] {
        let mut bad_method = echo.clone();
        bad_method[field] = json!(value);
        refused(&mut client, "services.register", json!({ "name": "rpc-bad", "methods": { "m": bad_method } }), None);
    }
    assert!(
        client.call("services.list", json!({}))["data"].as_array().unwrap().iter().all(|row| row["name"] != "rpc-bad")
    );
    client.call("services.register", json!({ "name": "rpc-wide", "methods": { "open": echo, "owner": owner_only } }));

    let first = client.call("services.register", definition.clone());
    let first_id = first["registration_id"].as_str().unwrap().to_string();
    assert_eq!(invoke(&mut client, "echo", json!({ "text": "hello" })), json!(format!("{first_id}:hello")));
    assert_eq!(
        client.call("services.list", json!({}))["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "rpc-demo")
            .unwrap()["methods"]["echo"]["unconfined"],
        json!(true)
    );
    refused(&mut client, "rpc-demo.echo", json!({ "text": 12 }), None);
    refused(&mut client, "rpc-demo.echo", json!({ "text": "hello", "unknown": true }), None);
    refused(&mut client, "rpc-demo._private", json!({}), None);
    refused(&mut client, "services.register", definition.clone(), Some("already_provided"));
    assert_eq!(invoke(&mut client, "echo", json!({ "text": "new" })), json!(format!("{first_id}:new")));

    let usage_methods = ["get", "totals", "context_tokens", "set_context", "cost_text", "rate_limits", "quota_lines"];
    refused(&mut client, "services.register", json!({ "name": "usage", "methods": { "get": {} } }), None);
    let bad_usage_methods: serde_json::Map<String, Value> = usage_methods
        .iter()
        .map(|&m| (m.to_string(), if m == "get" { json!({ "result": { "type": "string" } }) } else { json!({}) }))
        .collect();
    refused(
        &mut client,
        "services.register",
        json!({ "name": "usage", "methods": bad_usage_methods }),
        Some("differs"),
    );
    // one slot per service on the cordis tree: a remote backend cannot take the built-in
    // usage service while it runs (the built-in must be turned off first)
    let empty: serde_json::Map<String, Value> = usage_methods.iter().map(|&m| (m.to_string(), json!({}))).collect();
    refused(&mut client, "services.register", json!({ "name": "usage", "methods": empty }), Some("already_provided"));
    let switched_to = client.call("session.create", json!({}));
    let session = switched_to["session_id"].as_str().unwrap().to_string();
    *handler.lock().unwrap() = Box::new(|_, msg| ServiceReply::Result(msg["params"]["arguments"]["text"].clone()));
    let mut where_method = echo.clone();
    where_method["scope"] = json!("session");
    client.call("services.register", json!({ "name": "rpc-session", "methods": { "where": where_method } }));
    refused(&mut client, "rpc-session.where", json!({ "text": "x" }), Some("session_id"));
    client.call("rpc-session.where", json!({ "text": "x", "session_id": session }));
    assert_eq!(callbacks.lock().unwrap().last().unwrap()["params"]["session_id"], session);
    *handler.lock().unwrap() = default_handler();

    refused(
        &mut client,
        "services.register",
        json!({ "name": "theme", "methods": { "get": {} } }),
        Some("registration"),
    );
    refused(
        &mut client,
        "services.register",
        json!({ "name": "llm", "methods": { "get_model": {} } }),
        Some("cannot be provided over RPC"),
    );
    let mut bad_schema_echo = echo.clone();
    bad_schema_echo["parameters"] = json!({ "type": "object", "bogus": true });
    let mut bad_order_echo = echo.clone();
    bad_order_echo["order"] = json!(["missing"]);
    for bad in [
        json!({ "name": "_private", "methods": { "echo": echo } }),
        json!({ "name": "allowed.private", "methods": { "echo": echo } }),
        json!({ "name": "bad-schema", "methods": { "echo": bad_schema_echo } }),
        json!({ "name": "bad-order", "methods": { "echo": bad_order_echo } }),
    ] {
        refused(&mut client, "services.register", bad, None);
    }

    let before = callbacks.lock().unwrap().len();
    *handler.lock().unwrap() = Box::new(|_, msg| ServiceReply::Result(msg["params"]["arguments"]["value"].clone()));
    for value in [json!(null), json!([]), json!({}), json!({ "a": [], "b": null }), json!("日本語")] {
        assert_eq!(invoke(&mut client, "roundtrip", json!({ "value": value })), value);
    }
    assert_eq!(callbacks.lock().unwrap().len(), before + 5);

    *handler.lock().unwrap() = Box::new(|_, _| ServiceReply::Result(json!(12)));
    refused(&mut client, "rpc-demo.echo", json!({ "text": "wrong result" }), Some("invalid service result"));

    *handler.lock().unwrap() = Box::new(|c, msg| {
        c.send(json!({ "id": msg["id"] }));
        ServiceReply::Silent
    });
    refused(&mut client, "rpc-demo.echo", json!({ "text": "missing reply value" }), Some("\"internal\""));

    *handler.lock().unwrap() = Box::new(|_, _| ServiceReply::Silent);
    refused(&mut client, "rpc-demo.echo", json!({ "text": "timeout" }), Some("timed out"));

    let request = client.request("rpc-demo.echo", json!({ "text": "cancel" }));
    let callback = loop {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            break msg;
        }
        client.dispatch(msg);
    };
    client.send(json!({ "method": "$/cancel_request", "params": { "id": request } }));
    let mut cancelled = false;
    loop {
        let reply = client.read();
        if reply.get("method").and_then(Value::as_str) == Some("$/cancel_request")
            && reply["params"]["id"] == callback["id"]
        {
            cancelled = true;
        }
        if reply.get("id").and_then(Value::as_u64) == Some(request) {
            assert_eq!(reply["error"]["code"], json!(-32800), "{reply}");
            break;
        }
        client.dispatch(reply);
    }
    assert!(cancelled, "request cancellation did not cancel its reverse callback");
    client.send(json!({ "id": callback["id"], "result": "late" }));
    *handler.lock().unwrap() = default_handler();
    assert_eq!(
        invoke(&mut client, "echo", json!({ "text": "after cancel" })),
        json!(format!("{first_id}:after cancel"))
    );

    {
        let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
        peer.call(
            "initialize",
            json!({ "protocol_version": initialized["protocol_version"], "capabilities": { "callbacks": true } }),
        );
        let listed = peer.call("services.list", json!({}))["result"]["data"].clone();
        let rows: Vec<(&str, Vec<&str>)> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let mut methods: Vec<&str> = row["methods"].as_object().unwrap().keys().map(String::as_str).collect();
                methods.sort();
                (row["name"].as_str().unwrap(), methods)
            })
            .collect();
        // `status.get` is in the peer default contract grant, so a peer sees it alongside `allow`ed remote ops.
        assert_eq!(
            rows,
            vec![("rpc-demo", vec!["echo"]), ("rpc-wide", vec!["open"]), ("status", vec!["get"])],
            "{listed}"
        );
        let owner_call = peer.call("rpc-wide.owner", json!({ "text": "x" }));
        assert!(
            matches!(owner_call["error"]["data"]["code"].as_str(), Some("denied" | "method_not_found")),
            "{owner_call}"
        );
        let opened = peer.call("rpc-wide.open", json!({ "text": "x" }));
        assert_eq!(opened["error"]["data"]["code"], "timed_out", "{opened}");
        assert!(
            peer.call("rpc-demo.roundtrip", json!({ "value": "secret" })).get("error").is_some(),
            "the direct route applies the grant"
        );
        assert_eq!(peer.call("registrations.release", json!({ "registration_id": first_id }))["result"], json!({}));
        let peer_reg = peer.call("services.register", json!({ "name": "peer-demo", "methods": { "echo": echo } }));
        assert_eq!(peer_reg["error"]["code"], json!(-32601), "services.register must be primary-only: {peer_reg}");
        assert!(
            client.call("services.list", json!({}))["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["name"] != "peer-demo")
        );
    }

    *handler.lock().unwrap() = Box::new(|_, _| ServiceReply::Silent);
    let inflight = client.request("rpc-demo.echo", json!({ "text": "withdraw capability" }));
    let callback = loop {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            break msg;
        }
        client.dispatch(msg);
    };
    let withdrawn = client.request(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": false, "ui": false, "approvals": false, "elicitation": false } }),
    );
    let mut pending = std::collections::HashSet::from([inflight, withdrawn]);
    while !pending.is_empty() {
        let reply = client.read();
        if let Some(id) = reply.get("id").and_then(Value::as_u64)
            && pending.remove(&id)
        {
            if id == inflight {
                assert!(reply.get("error").is_some(), "{reply}");
            } else {
                assert_eq!(reply["result"]["capabilities"]["callbacks"], json!(false), "{reply}");
            }
        } else {
            client.dispatch(reply);
        }
    }
    client.send(json!({ "id": callback["id"], "result": "late after withdrawal" }));
    assert!(
        client.call("services.list", json!({}))["data"].as_array().unwrap().iter().all(|row| row["name"] != "rpc-demo")
    );
    refused(&mut client, "services.register", definition.clone(), Some("capability"));
    client.call(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": true, "ui": false, "approvals": false, "elicitation": false } }),
    );
    client.call("services.register", definition);
    assert_eq!(client.call("registrations.release", json!({ "registration_id": first_id })), json!({}));
    client.shutdown_and_wait();
}

/// A Lua provider calls a service a client registered over RPC: the empty Lua table reaches the
/// client as an array (the contract's schema says array); the Lua caller keeps its own identity
/// (not the RPC caller's); and a confined Lua wrapper cannot reach an unconfined remote backend.
/// Split out of the generic registration test above, where these ran through the variant-probe
/// plugin's `forward` method and the service-probe plugin's `run`.
#[test]
fn a_native_lua_provider_calls_a_service_a_client_registered() {
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.plugin('status', { enabled = false })").unwrap();
    std::fs::write(
        home.join("config/plugins/remote-callers.lua"),
        r#"return {name="remote-callers",provide={"forward-probe","lua-probe","status"},contracts={
  ["forward-probe"]={methods={forward={doc="call a remote provider",expose="both",result={type="object"}}}},
  ["lua-probe"]={methods={run={doc="probe",expose="both",
    params={type="object",properties={text={type="string"},confined={type="boolean"}},required={"text"},additionalProperties=false},
    order={"..."},result={type="string"}},
    from_row={doc="call the remote service from the row context",expose="both",
    params={type="object",properties={text={type="string"}},required={"text"},additionalProperties=false},
    order={"text"},result={type="string"}}}}},apply=function(ctx)
    local Forward=require("cordis").class("Forward",mare.Service)
    function Forward:constructor(c) Forward.super.constructor(self,c,"forward-probe") end
    function Forward:forward() return ctx:get("rpc-array"):take({}) end
    ctx:plugin(Forward)
    local Probe=require("cordis").class("Probe",mare.Service)
    function Probe:constructor(c) Probe.super.constructor(self,c,"lua-probe") self.child_ctx=c end
    function Probe:run(arguments)
        local function invoke()return self.child_ctx:get("rpc-demo"):echo(arguments.text)end
        if arguments.confined then
            return mare.exec.run({tool="service-probe",tool_call_id="confined",sandbox={level="read-only",network=false}},invoke)
        end
        return invoke()
    end
    function Probe:from_row(text) return ctx:get("rpc-demo"):echo(text) end
    ctx:plugin(Probe)
    local Status=require("cordis").class("Status",mare.Service)
    function Status:constructor(c) Status.super.constructor(self,c,"status") end
    -- Exercise an RPC service read from the root context.
    function Status:get()
        local root=self.ctx[require("cordis").Context.root]
        local rpc=root:get("rpc-demo")
        return {state="idle",message=rpc and rpc:echo("root") or "",session={},cwd="",pid=0,seq=0}
    end
    ctx:plugin(Status)
end}
"#,
    )
    .unwrap();
    // a remote service called while the caller's apply runs: its row's owner is the caller already
    std::fs::write(
        home.join("config/plugins/apply-caller.lua"),
        r#"return {name="apply-caller",inject={"rpc-demo","commands"},apply=function(ctx)
    ctx["rpc-demo"]:echo("apply")
    ctx.commands:register("apply-again",{run=function() ctx["rpc-demo"]:echo("apply-again") end})
end}
"#,
    )
    .unwrap();
    std::fs::write(
        home.join("config/plugins/other-apply-caller.lua"),
        r#"return {name="other-apply-caller",inject={"rpc-demo"},apply=function(ctx)
    ctx["rpc-demo"]:echo("other-apply")
end}
"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--yolo", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    let callbacks: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let seen = callbacks.clone();
    client.on_method = Some(Box::new(move |c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            seen.lock().unwrap().push(msg.clone());
            let args = &msg["params"]["arguments"];
            let result = match args.get("text").and_then(Value::as_str) {
                Some(text) => json!(format!("{}:{text}", msg["params"]["registration_id"].as_str().unwrap())),
                None => args.clone(),
            };
            c.send(json!({ "id": msg["id"], "result": result }));
            return true;
        }
        false
    }));
    client.call(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 1500 }),
    );
    let items = json!({
        "parameters": {
            "type": "object",
            "properties": { "items": { "type": "array" } },
            "required": ["items"],
            "additionalProperties": false,
        },
        "result": { "type": "object" },
        "order": ["items"],
    });
    let array_service = client.call("services.register", json!({ "name": "rpc-array", "methods": { "take": items } }));
    assert_eq!(client.call("forward-probe.forward", json!({})), json!({ "items": [] }));
    let forward_caller = callbacks.lock().unwrap().last().unwrap()["params"]["caller_id"].clone();
    assert!(forward_caller.as_str().is_some_and(|id| !id.is_empty()), "forward caller: {forward_caller}");
    assert_eq!(
        client.call("registrations.release", json!({ "registration_id": array_service["registration_id"] })),
        json!({})
    );

    let echo = json!({
        "parameters": {
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false,
        },
        "result": { "type": "string" },
        "order": ["text"],
        "access": "peer",
    });
    let services_before = client.call("services.list", json!({}));
    // Refusing another owner's provision leaves the visible services unchanged, even on retries.
    let forward = json!({});
    for _ in 0..3 {
        let err = client.call_err(
            "services.register",
            json!({ "name": "forward-probe", "methods": { "forward": forward.clone() } }),
        );
        assert!(err.to_string().contains("already_provided"), "{err}");
    }
    assert_eq!(
        client.call("services.list", json!({})),
        services_before,
        "a refused registration changed the visible services"
    );
    // a method named `constructor` would be the provider class's constructor (the class machinery
    // reads it): refused as a method name, like any name that is not one
    let ctor = client
        .call_err("services.register", json!({ "name": "ctor-probe", "methods": { "constructor": items.clone() } }));
    assert!(ctor.to_string().contains("invalid service method"), "{ctor}");
    let first = client.call("services.register", json!({ "name": "rpc-demo", "methods": { "echo": echo } }));
    let first_id = first["registration_id"].as_str().unwrap().to_string();
    client.call("rpc-demo.echo", json!({ "text": "hello" }));
    let caller_for = |text: &str| {
        let messages = callbacks.lock().unwrap();
        let callback = messages
            .iter()
            .find(|message| message["params"]["arguments"]["text"] == text)
            .unwrap_or_else(|| panic!("missing service callback for {text}"));
        callback["params"]["caller_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| panic!("service callback for {text} has no caller id: {callback}"))
            .to_string()
    };
    let rpc_caller = caller_for("hello");
    let apply_caller = caller_for("apply");
    let other_caller = caller_for("other-apply");
    client.call("commands.run", json!({ "line": "/apply-again" }));
    assert_eq!(caller_for("apply-again"), apply_caller, "the same plugin must keep its owner after apply");
    client.call("lua-probe.from_row", json!({ "text": "row" }));
    let row_caller = caller_for("row");
    assert_eq!(client.call("lua-probe.run", json!({ "text": "Lua" })), json!(format!("{first_id}:Lua")));
    assert_eq!(caller_for("Lua"), row_caller, "the child service must use its parent row's owner");
    assert_ne!(apply_caller, other_caller, "different plugins must have different owners");
    assert_ne!(apply_caller, row_caller, "apply-caller and remote-callers must have different owners");
    assert_ne!(other_caller, row_caller, "other-apply-caller and remote-callers must have different owners");
    assert_ne!(apply_caller, rpc_caller, "apply-caller must differ from the connection");
    assert_ne!(other_caller, rpc_caller, "other-apply-caller must differ from the connection");
    assert_ne!(row_caller, rpc_caller, "native Lua caller identity was replaced by the RPC caller");
    let count = callbacks.lock().unwrap().len();
    let err = client.call_err("lua-probe.run", json!({ "text": "must stay confined", "confined": true }));
    assert!(err.to_string().contains("unconfined"), "expected \"unconfined\" in {err}");
    assert_eq!(callbacks.lock().unwrap().len(), count, "a local wrapper bypassed remote-backend confinement");
    // status.get calls its service from the host's root context: there is no caller, and the
    // lookup must terminate even though the root's parent context is its own.
    assert_eq!(client.call("status.get", json!({}))["message"], json!(format!("{first_id}:root")));
    assert!(callbacks.lock().unwrap().last().unwrap()["params"]["caller_id"].is_null(), "the host is no caller");
    client.shutdown_and_wait();
}

#[test]
fn contract_methods_session_addressing_and_indirect_peer_authorization() {
    use std::collections::{BTreeMap, BTreeSet};

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"mare.setup { peer = { allow = {"llm.list_models", "usage.get", "options.set", "commands.run", "llm.logout", "llm.set_effort"} } }
"#,
    )
    .unwrap();
    let script = root.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--yolo", "--model", "faux/echo"], &env, root.path());
    let initialized = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "ui": true } }));
    let session = initialized["session"]["session_id"].as_str().expect("non-empty session_id").to_string();
    assert!(!session.is_empty(), "{initialized}");

    let usage = client.call("usage.get", json!({ "session_id": session }));
    let keys: BTreeSet<&str> = usage.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["totals", "context_tokens", "rate_limits"]), "{usage}");
    let err = client.call_err("usage.get", json!({}));
    assert!(err.to_string().contains("session_id"), "{err}");
    let err = client.call_err("usage.get", json!({ "session_id": "no-such-session" }));
    assert!(err.to_string().contains("not open"), "{err}");
    assert_eq!(client.call("llm.get_model", json!({ "session_id": session }))["ref"], "faux/echo");

    assert_eq!(
        client.call("llm.login", json!({ "provider": "openai", "method": "api_key", "key": "sk-contract-test" }),),
        json!({})
    );

    let openai_logged_in = |c: &mut crate::common::rpc::RpcClient| -> bool {
        let providers = c.call("llm.list_providers", json!({}));
        providers["data"].as_array().unwrap().iter().find(|p| p["provider"] == "openai").unwrap()["logged_in"]
            .as_bool()
            .unwrap()
    };
    assert!(openai_logged_in(&mut client));

    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    peer.call("initialize", json!({ "protocol_version": initialized["protocol_version"] }));
    assert!(peer.call("llm.list_models", json!({})).get("result").is_some());

    let denied = peer.call("llm.logout", json!({ "provider": "openai" }));
    assert_eq!(denied["error"]["data"]["code"], "denied", "{denied}");

    let model = peer.call("options.set", json!({ "values": { "model": "faux/echo" } }));
    assert_eq!(model["error"]["code"], json!(-32601), "options.set must be primary-only: {model}");

    let ran = peer.call("commands.run", json!({ "line": "/logout openai" }));
    assert!(ran["error"]["message"].as_str().is_some_and(|m| m.contains("not allowed")), "{ran}");

    let effort = peer.call("llm.set_effort", json!({ "effort": "high", "target": "session", "session_id": session }));
    assert_eq!(effort.get("result"), Some(&json!({})), "{effort}");
    assert!(openai_logged_in(&mut client), "direct peer logout attempts must leave credential intact");

    client.notices.clear();
    client.call("commands.run", json!({ "line": "/logout openai" }));
    while !client.notices.iter().any(|n| n.to_string().contains("Logged out")) {
        let msg = client.read();
        client.dispatch(msg);
    }
    assert!(!openai_logged_in(&mut client));

    drop(peer);
    client.shutdown_and_wait();
}

/// Direct contract RPC on a mare.Service's typed JSON method: the bridge calls the method with its
/// arguments after self (no caller argument), on the service as the connection reads it, so
/// `self.ctx` is the connection's fiber, not the provider.
#[test]
fn services_call_reaches_a_mare_service_method_as_the_connection() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        r#"mare.setup { peer = { allow = {"hooks.subscribe", "svc-probe.echo"} } }"#,
    )
    .unwrap();
    std::fs::write(
        home.join("config/plugins/svc-probe.lua"),
        r#"return {name="svc-probe",provide={"svc-probe"},contracts={["svc-probe"]={errors={refused={doc="refused"}},
    methods={echo={doc="echo",expose="both",access="peer",errors={"refused"},
    params={type="object",properties={text={type="string"}},required={"text"},additionalProperties=false},
    order={"text"},result={type="object"}}}}},apply=function(ctx)
    local lib = require("mare_lib")
    local Probe = require("cordis").class("Probe", mare.Service)
    function Probe:constructor(c) Probe.super.constructor(self, c, "svc-probe") end
    function Probe:echo(text)
        if text == "refuse" then lib.fail("svc-probe.refused", "refused on purpose") end
        return {text=text, caller=lib.caller_name(self.ctx) or ""}
    end
    ctx:plugin(Probe)
end}
"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--yolo", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    let initialized =
        client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let hook = json!({ "scope": "all", "event": "status/changed", "callbacks": ["handle"] });
    let primary_hook = client.call("hooks.subscribe", hook.clone());
    assert!(primary_hook["registration_id"].is_string(), "{primary_hook}");
    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    let peer_init = peer.call(
        "initialize",
        json!({ "protocol_version": initialized["protocol_version"],
        "capabilities": { "callbacks": true } }),
    );
    assert!(peer_init["error"].is_null(), "{peer_init}");
    let peer_hook = peer.call("hooks.subscribe", hook);
    assert_eq!(peer_hook["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {peer_hook}");
    let value = client.call("svc-probe.echo", json!({ "text": "hi" }));
    assert_eq!(value["text"], json!("hi"), "{value}");
    let primary_caller = value["caller"].as_str().unwrap().to_string();
    assert!(primary_caller.starts_with("rpc:"), "primary connection: {value}");
    let value = peer.call("svc-probe.echo", json!({ "text": "hi" }));
    assert!(value["error"].is_null(), "{value}");
    assert_eq!(value["result"]["text"], json!("hi"), "{value}");
    let peer_caller = value["result"]["caller"].as_str().unwrap().to_string();
    assert!(peer_caller.starts_with("rpc:"), "peer connection: {value}");
    assert_ne!(primary_caller, peer_caller);
    // a declared code raised inside the method keeps its code through the service's guard
    let err = client.call_err("svc-probe.echo", json!({ "text": "refuse" }));
    assert_eq!(err["data"]["code"], json!("svc-probe.refused"), "{err}");
    drop(peer);
    client.shutdown_and_wait();
}

#[test]
fn workspace_scoped_service_contract_requires_and_validates_cwd() {
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let ws = root.path().join("ws");
    let other_ws = root.path().join("other-ws");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&other_ws).unwrap();
    #[cfg(unix)]
    let ws_alias = {
        let link = root.path().join("ws-link");
        std::os::unix::fs::symlink(&ws, &link).unwrap();
        link
    };
    #[cfg(not(unix))]
    let ws_alias = ws.clone();

    std::fs::write(
        home.join("config/plugins/ws-probe.lua"),
        r#"return {
  name = "ws-probe",
  provide = { "ws-probe" },
  contracts = {
    ["ws-probe"] = {
      methods = {
        echo = {
          doc = "workspace scoped probe",
          expose = "both",
          scope = "workspace",
          params = { type = "object", properties = { msg = { type = "string" } }, required = { "msg" }, additionalProperties = false },
          order = { "msg" },
          result = { type = "string" },
        },
      },
    },
  },
  apply = function(ctx)
    local S = require("cordis").class("WsProbe", mare.Service)
    function S:constructor(c) S.super.constructor(self, c, "ws-probe") end
    function S:echo(msg) return "ws:" .. msg end
    ctx:plugin(S)
  end,
}
"#,
    )
    .unwrap();

    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &env, &ws);
    let _ = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));

    // Wire schema injects required `cwd` for `Scope::Workspace`.
    let listed = client.call("services.list", json!({}));
    let entry = listed["data"].as_array().unwrap().iter().find(|r| r["name"] == "ws-probe").expect("ws-probe listed");
    assert_eq!(entry["methods"]["echo"]["scope"], "workspace", "{entry}");
    let required = entry["methods"]["echo"]["parameters"]["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "cwd"), "expected cwd in required: {entry}");

    // Missing `cwd` fails invalid_arguments; non-open `cwd` fails unknown_session; canonical alias succeeds.
    let missing = client.call_err("ws-probe.echo", json!({ "msg": "hi" }));
    assert!(missing.to_string().contains("cwd") || missing.to_string().contains("invalid"), "{missing}");

    let wrong = client.call_err("ws-probe.echo", json!({ "cwd": other_ws, "msg": "hi" }));
    assert!(wrong.to_string().contains("unknown_session"), "{wrong}");

    let ok = client.call("ws-probe.echo", json!({ "cwd": ws_alias, "msg": "hi" }));
    assert_eq!(ok, json!("ws:hi"), "{ok}");

    client.shutdown_and_wait();
}

/// `llm.register_provider` accepts `tool_update = false` (to explicitly disable inherited
/// provider-level `tool_update`) as well as `"addition_only"`, while rejecting `true`.
#[test]
fn llm_register_provider_accepts_tool_update_false_and_rejects_true() {
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--yolo", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));

    let registered = client.call(
        "llm.register_provider",
        json!({
            "provider": "rpc-tool-update",
            "default_model": "m-false",
            "models": [
                { "model_id": "m-false", "tool_update": false },
                { "model_id": "m-add", "tool_update": "addition_only" }
            ]
        }),
    );
    assert!(registered["registration_id"].is_string(), "{registered}");

    let bad = client.call_err(
        "llm.register_provider",
        json!({
            "provider": "rpc-tool-update-bad",
            "models": [
                { "model_id": "m-true", "tool_update": true }
            ]
        }),
    );
    assert!(
        bad.to_string().contains("invalid_arguments") || bad.to_string().contains("tool_update"),
        "tool_update = true must be rejected: {bad}"
    );

    client.call("registrations.release", json!({ "registration_id": registered["registration_id"] }));
    client.shutdown_and_wait();
}
