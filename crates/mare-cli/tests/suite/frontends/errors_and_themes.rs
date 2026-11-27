use super::*;

#[test]
fn structured_errors_reach_lua_and_rpc_callers_as_one_value() {
    use std::collections::{BTreeMap, BTreeSet};

    let probe_lua = r#"
local LONG = string.rep("x", 5000)
local KIND = {type="object",properties={kind={type="string"}},required={"kind"},additionalProperties=false}
return {name="err-probe",provide={"err-probe"},contracts={["err-probe"]={
  errors={
    too_large={doc="the input is over the limit",data={type="object",properties={max_chars={type="integer"},
      actual_chars={type="integer"}},required={"max_chars","actual_chars"},additionalProperties=false}},
    backend_down={doc="the backend does not answer"}},
  methods={
    raise={doc="raise one kind of error",expose="both",errors={"too_large","backend_down"},params=KIND,
      order={"kind"},result={type="string"}},
    quiet={doc="declares no errors of its own",expose="both",errors={},params=KIND,order={"kind"},
      result={type="string"}}}}},
  apply=function(ctx)
    local function raise(_,kind)
      if kind=="declared" then error({code="err-probe.too_large",message="input is too large",data={max_chars=4,actual_chars=9}})
      elseif kind=="returned" then return nil,{code="err-probe.too_large",message="returned, not raised",data={max_chars=1,actual_chars=2}}
      elseif kind=="core" then error({code="unavailable",message="backend is down"})
      elseif kind=="down" then error({code="err-probe.backend_down",message="no answer"})
      elseif kind=="undeclared" then error({code="err-probe.secret_kind",message="never declared s3cret",data={token="s3cret"}})
      elseif kind=="foreign" then error({code="other.too_large",message="another service's code s3cret"})
      elseif kind=="unencodable" then error({code="err-probe.too_large",message="bad data s3cret",data={f=function() end}})
      elseif kind=="baddata" then error({code="err-probe.too_large",message="data off its schema s3cret",data={max_chars="four",actual_chars=9}})
      elseif kind=="downdata" then error({code="err-probe.backend_down",message="data it does not declare s3cret",data={host="s3cret"}})
      elseif kind=="multiline" then error({code="err-probe.too_large",message="line one\nline two",data={max_chars=1,actual_chars=2}})
      elseif kind=="nocode" then error({kind="rate_limit",message="a table without a code s3cret"})
      elseif kind=="plain" then error("plain failure s3cret\nwith a second line")
      elseif kind=="long" then error(LONG.."s3cret")
      end
      return "ok"
    end
    local Probe=require("cordis").class("Probe",mare.Service)
    function Probe:constructor(c) Probe.super.constructor(self,c,"err-probe") end
    function Probe:raise(kind) return raise(self,kind) end
    function Probe:quiet(kind) return raise(self,kind) end
    ctx:plugin(Probe)
  end}
"#;

    let caller_lua = r#"
local KIND = {type="object",properties={kind={type="string"},method={type="string"},how={type="string"}},required={"kind","method","how"},additionalProperties=false}
return {name="err-caller",inject={"err-probe"},provide={"err-caller","err-outer"},contracts={
  ["err-caller"]={methods={catch={doc="call err-probe and report the error",expose="both",params=KIND,
    order={"kind","method","how"},result={type="object"}}}},
  ["err-outer"]={methods={wrap={doc="call err-probe.raise and let its error propagate",expose="both",
    params={type="object",properties={kind={type="string"}},required={"kind"},additionalProperties=false},
    order={"kind"},result={type="string"}}}}},
  apply=function(ctx)
    local function attempt(kind,method,how)
      local p=ctx["err-probe"]
      if how=="get" then p=ctx:get("err-probe") end
      if how=="exec_run" then
        return pcall(mare.exec.run,{tool="err-caller",tool_call_id="c1",sandbox="danger-full-access"},function() return p[method](p,kind) end)
      end
      return pcall(function() return p[method](p,kind) end)
    end
    local Caller=require("cordis").class("Caller",mare.Service)
    function Caller:constructor(c) Caller.super.constructor(self,c,"err-caller") end
    function Caller:catch(kind,method,how)
      local ok,e=attempt(kind,method,how)
      assert(not ok,"err-probe did not fail")
      assert(type(e)=="table","a Lua caller catches a table through "..how..", got "..type(e))
      return {code=e.code,message=e.message,data=e.data,text=tostring(e)}
    end
    ctx:plugin(Caller)
    local Outer=require("cordis").class("Outer",mare.Service)
    function Outer:constructor(c) Outer.super.constructor(self,c,"err-outer") end
    function Outer:wrap(kind) return ctx["err-probe"]:raise(kind) end
    ctx:plugin(Outer)
  end}
"#;

    let llm_lua = r#"
local MODE = "off"
return {name="err-llm",inject={"llm"},provide={"err-llm"},contracts={["err-llm"]={methods={mode={doc="set the mode",expose="both",
    params={type="object",properties={mode={type="string"}},required={"mode"},additionalProperties=false},
    order={"mode"},result={type="string"}}}}},
  apply=function(ctx)
    local lib=require("mare_lib")
    ctx:on("llm/model-changing",function(ev)
      if ev.model.ref~="faux/echo2" then return end
      if MODE=="refuse" then ev.refuse("echo2 is not admitted here") end
      if MODE=="crash" then error("listener crashed s3cret") end
    end)
    ctx.llm:register_provider({provider="errp",default_model="m",models={{model_id="m",name="M"}},
      is_configured=function() return true end,
      stream=function() error("no stream") end,
      login=function()
        if MODE=="decline" then lib.fail("llm.login_failed","the user declined") end
        error("login crashed s3cret")
      end})
    local Mode=require("cordis").class("Mode",mare.Service)
    function Mode:constructor(c) Mode.super.constructor(self,c,"err-llm") end
    function Mode:mode(mode) MODE=mode return mode end
    ctx:plugin(Mode)
  end}
"#;

    let internal = ["undeclared", "foreign", "unencodable", "baddata", "downdata", "nocode", "plain", "long"];

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/init.lua"), "").unwrap();
    std::fs::write(home.join("config/plugins/err-probe.lua"), probe_lua).unwrap();
    std::fs::write(home.join("config/plugins/err-caller.lua"), caller_lua).unwrap();
    std::fs::write(home.join("config/plugins/err-llm.lua"), llm_lua).unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, root.path());
    client.on_method = Some(Box::new(|c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            let kind = msg["params"]["arguments"]["kind"].as_str().unwrap_or("");
            let error = if kind == "coded" {
                json!({ "code": -32000, "message": "backend is down", "data": { "code": "unavailable" } })
            } else {
                json!({ "code": -32000, "message": "remote failure s3cret" })
            };
            c.send(json!({ "id": msg["id"], "error": error }));
            return true;
        }
        false
    }));

    let call_error = |c: &mut crate::common::rpc::RpcClient, service: &str, method: &str, arguments: Value| -> Value {
        c.call_err(&format!("{service}.{method}"), arguments)
    };
    let lua = |c: &mut crate::common::rpc::RpcClient, kind: &str, method: &str, how: &str| -> Value {
        c.call("err-caller.catch", json!({ "kind": kind, "method": method, "how": how }))
    };
    let set_mode = |c: &mut crate::common::rpc::RpcClient, value: &str| {
        c.call("err-llm.mode", json!({ "mode": value }));
    };
    let hidden = |error: &Value, label: &str| {
        assert!(error["code"] == -32603 && error["data"] == json!({ "code": "internal" }), "{label}: {error}");
        assert_eq!(error["message"], format!("{label} failed (details are in the log)"), "{label}: {error}");
        assert!(!error.to_string().contains("s3cret"), "{label}: {error}");
    };

    let initialized =
        client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let session = initialized["session"]["session_id"].as_str().unwrap().to_string();
    let listed = client.call("services.list", json!({}))["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "err-probe")
        .unwrap()
        .clone();
    assert_eq!(listed["errors"]["err-probe.too_large"]["doc"], "the input is over the limit", "{listed}");
    assert_eq!(
        listed["errors"]["err-probe.too_large"]["data"]["required"],
        json!(["max_chars", "actual_chars"]),
        "{listed}"
    );
    assert_eq!(listed["errors"]["err-probe.backend_down"], json!({ "doc": "the backend does not answer" }), "{listed}");
    assert_eq!(
        listed["methods"]["raise"]["errors"],
        json!(["err-probe.backend_down", "err-probe.too_large"]),
        "{listed}"
    );

    let error = call_error(&mut client, "err-probe", "raise", json!({ "kind": "declared" }));
    assert_eq!(error["code"], -32000, "{error}");
    assert_eq!(error["message"], "input is too large", "{error}");
    assert_eq!(
        error["data"],
        json!({ "code": "err-probe.too_large", "data": { "max_chars": 4, "actual_chars": 9 } }),
        "{error}"
    );

    for how in ["proxy", "get", "exec_run"] {
        let caught = lua(&mut client, "declared", "raise", how);
        assert_eq!(caught["code"], "err-probe.too_large", "{how}: {caught}");
        assert_eq!(caught["data"], json!({ "max_chars": 4, "actual_chars": 9 }), "{how}: {caught}");
        assert_eq!(caught["text"], "err-probe.too_large: input is too large", "{how}: {caught}");
    }
    for how in ["proxy", "get", "exec_run"] {
        let caught = lua(&mut client, "multiline", "raise", how);
        assert_eq!(caught["code"], "err-probe.too_large", "{how}: {caught}");
        assert_eq!(caught["message"], "line one\nline two", "{how}: {caught}");
        assert_eq!(caught["data"], json!({ "max_chars": 1, "actual_chars": 2 }), "{how}: {caught}");
    }
    assert_eq!(
        call_error(&mut client, "err-probe", "raise", json!({ "kind": "returned" }))["data"],
        json!({ "code": "err-probe.too_large", "data": { "max_chars": 1, "actual_chars": 2 } })
    );
    assert_eq!(lua(&mut client, "returned", "raise", "proxy")["data"], json!({ "max_chars": 1, "actual_chars": 2 }));
    assert_eq!(
        call_error(&mut client, "err-probe", "raise", json!({ "kind": "down" }))["data"],
        json!({ "code": "err-probe.backend_down" })
    );
    let error = call_error(&mut client, "err-probe", "raise", json!({ "kind": "core" }));
    assert!(error["code"] == -32000 && error["data"] == json!({ "code": "unavailable" }), "{error}");
    assert_eq!(lua(&mut client, "core", "raise", "proxy")["code"], "unavailable");
    assert_eq!(
        call_error(&mut client, "err-probe", "quiet", json!({ "kind": "declared" }))["data"],
        json!({ "code": "internal" })
    );
    assert_eq!(lua(&mut client, "declared", "quiet", "proxy")["code"], "internal");

    for kind in internal {
        hidden(&call_error(&mut client, "err-probe", "raise", json!({ "kind": kind })), "err-probe.raise");
        for how in ["proxy", "get", "exec_run"] {
            let caught = lua(&mut client, kind, "raise", how);
            assert!(
                caught["code"] == "internal" && caught.get("data").is_none_or(Value::is_null),
                "{kind} {how}: {caught}"
            );
            let msg = caught["message"].as_str().unwrap();
            assert!(msg.len() <= 1100 && !msg.contains('\n'), "{kind} {how}: {caught}");
        }
    }
    assert!(lua(&mut client, "plain", "raise", "proxy")["message"].as_str().unwrap().contains("plain failure"));

    hidden(&call_error(&mut client, "err-outer", "wrap", json!({ "kind": "plain" })), "err-outer.wrap");
    hidden(&call_error(&mut client, "err-outer", "wrap", json!({ "kind": "undeclared" })), "err-outer.wrap");
    hidden(&call_error(&mut client, "err-outer", "wrap", json!({ "kind": "declared" })), "err-outer.wrap");

    let remote = json!({
        "parameters": {
            "type": "object",
            "properties": { "kind": { "type": "string" } },
            "required": ["kind"],
            "additionalProperties": false,
        },
        "result": { "type": "string" },
        "order": ["kind"],
    });
    client.call("services.register", json!({ "name": "err-remote", "methods": { "echo": remote } }));
    let error = call_error(&mut client, "err-remote", "echo", json!({ "kind": "plain" }));
    assert!(error["code"] == -32603 && error["data"] == json!({ "code": "internal" }), "{error}");
    assert!(!error.to_string().contains("s3cret"), "{error}");
    let error = call_error(&mut client, "err-remote", "echo", json!({ "kind": "coded" }));
    assert!(error["code"] == -32000 && error["data"] == json!({ "code": "unavailable" }), "{error}");

    let set_echo2 = |c: &mut crate::common::rpc::RpcClient| -> Value {
        c.call_err("llm.set_model", json!({ "ref": "faux/echo2", "target": "session", "session_id": session }))
    };
    set_mode(&mut client, "refuse");
    let error = set_echo2(&mut client);
    assert_eq!(error["data"], json!({ "code": "llm.model_unavailable" }), "{error}");
    assert_eq!(error["message"], "echo2 is not admitted here", "{error}");
    set_mode(&mut client, "crash");
    hidden(&set_echo2(&mut client), "llm.set_model");
    set_mode(&mut client, "decline");
    let error = client.call_err("llm.login", json!({ "provider": "errp", "key": "k" }));
    assert!(
        error["data"] == json!({ "code": "llm.login_failed" }) && error["message"] == "the user declined",
        "{error}"
    );
    set_mode(&mut client, "crash");
    hidden(&client.call_err("llm.login", json!({ "provider": "errp", "key": "k" })), "llm.login");

    for (method, params) in
        [("theme.select", json!({ "theme": "no-such-theme" })), ("theme.preview", json!({ "theme": "no-such-theme" }))]
    {
        let error = client.call_err(method, params);
        assert_eq!(error["data"], json!({ "code": "theme.unknown_theme" }), "{method}: {error}");
    }

    let error = client.call_err("no.such_method", json!({}));
    assert!(error["code"] == -32601 && error["data"]["code"] == "method_not_found", "{error}");
    let removed_sc = client.call_err("services.call", json!({ "service": "err-probe" }));
    assert!(removed_sc["code"] == -32601 && removed_sc["data"]["code"] == "method_not_found", "{removed_sc}");
    let error = client.call_err("err-probe.raise", json!({}));
    assert!(error["code"] == -32602 && error["data"]["code"] == "invalid_arguments", "{error}");

    let out = mare_cmd().args(["--clean", "rpc", "schema"]).run_output();
    assert!(out.status.success(), "mare --clean rpc schema failed");
    let schema: Value = serde_json::from_slice(&out.stdout).unwrap();
    let numbers: BTreeSet<i64> =
        schema["errors"].as_object().unwrap().values().map(|e| e["number"].as_i64().unwrap()).collect();
    assert_eq!(numbers, BTreeSet::from([-32700, -32600, -32601, -32602, -32603, -32000, -32800]), "{numbers:?}");
    assert!(schema["errors"]["timed_out"]["number"] == -32000 && schema["errors"].get("timeout").is_none());

    client.shutdown_and_wait();
}

#[test]
fn theme_rpc_lifecycle_and_faux_continuity() {
    use std::collections::BTreeMap;

    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("home/config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "mare.setup { peer = { allow = {\"services.list\",\"theme.register\",\"theme.preview\",\"registrations.release\",\"llm.list_models\",\"llm.logout\"} } }\n",
    )
    .unwrap();
    let script = root.path().join("faux.json");
    std::fs::write(
        &script,
        serde_json::to_string(&json!([{ "text": "before theme" }, { "text": "after theme" }])).unwrap(),
    )
    .unwrap();
    let home = root.path().join("home");
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--yolo", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );

    let initialized =
        client.call("initialize", json!({ "protocol_version": "3.0", "client": { "name": "themes-rpc" } }));
    let sid = initialized["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("events.subscribe", json!({ "scope": "all", "include": ["theme/changed"] }));
    assert!(
        initialized["capabilities"]["methods"].as_array().unwrap().iter().any(|m| m == "theme.select"),
        "contract methods are advertised"
    );
    let listed: Vec<Value> = client.call("services.list", json!({}))["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["name"] == "theme")
        .cloned()
        .collect();
    assert!(
        !listed.is_empty()
            && listed[0]["methods"].get("select").is_some()
            && listed[0]["methods"].get("inspect").is_none(),
        "lua-only methods stay local"
    );
    assert!(client.call("theme.get", json!({}))["revision"].as_i64().unwrap() >= 0);
    assert!(client.call("theme.list", json!({}))["data"].is_array());
    let before = client.call("theme.get", json!({}));
    assert!(before["syntax"].as_str().unwrap().contains("<plist"), "frontends receive effective syntax document");

    {
        let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
        let init = peer.call("initialize", json!({ "protocol_version": initialized["protocol_version"] }));
        assert!(init.get("error").is_none(), "{init}");
        let peer_sid = init["result"]["session"]["session_id"].as_str().expect("peer session_id").to_string();
        let logout = peer.call("llm.logout", json!({ "provider": "x" }));
        assert!(logout["error"]["message"].as_str().is_some_and(|m| m.contains("not allowed")), "{logout}");
        let models = peer.call("llm.list_models", json!({}));
        assert!(!models["result"]["data"].is_null(), "access = peer is grantable");
        let reg = peer.call(
            "theme.register",
            json!({ "session_id": peer_sid, "definition": { "name": "connection-owned", "palette": {} } }),
        );
        assert!(reg.get("error").is_none(), "{reg}");
        let prev = peer.call("theme.preview", json!({ "theme": "connection-owned" }));
        assert!(prev.get("error").is_none(), "{prev}");
        assert_eq!(client.call("theme.get", json!({}))["active"], "connection-owned");
        client.call("options.set", json!({ "session_id": sid, "values": { "peer": { "allow": ["services.list", "theme.register", "registrations.release", "llm.list_models", "llm.logout"] } } }));
        let denied = peer.call("theme.preview", json!({ "theme": "dark" }));
        assert_eq!(denied["error"]["data"]["code"], "denied", "{denied}");
        assert_eq!(client.call("theme.get", json!({}))["active"], "connection-owned");
        client.call("options.set", json!({ "session_id": sid, "values": { "peer": { "allow": ["services.list", "theme.register", "theme.preview", "registrations.release", "llm.list_models", "llm.logout"] } } }));
        client.events.clear();
    }
    client.wait_event("theme/changed");
    assert_ne!(client.call("theme.get", json!({}))["active"], "connection-owned");
    assert!(
        client.call("theme.list", json!({}))["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["name"] != "connection-owned")
    );

    {
        let mut peer2 = crate::common::rpc::PeerClient::connect(&home, client.pid());
        let init = peer2.call("initialize", json!({ "protocol_version": initialized["protocol_version"] }));
        assert!(init.get("error").is_none(), "{init}");
        let reg = peer2.call(
            "theme.register",
            json!({ "session_id": sid, "definition": { "name": "peer-owned", "palette": {} } }),
        );
        assert!(reg.get("error").is_none(), "{reg}");
        let prev = peer2.call("theme.preview", json!({ "theme": "peer-owned" }));
        assert!(prev.get("error").is_none(), "{prev}");
        client.call("theme.preview", json!({ "theme": "dark" }));
        client.events.clear();
    }
    client.wait_event("theme/changed");
    assert!(
        client.call("theme.list", json!({}))["data"].as_array().unwrap().iter().all(|d| d["name"] != "peer-owned"),
        "peer2 disconnect did not unregister peer-owned"
    );
    assert_eq!(
        client.call("theme.get", json!({}))["preview"],
        "dark",
        "disposing old owner must not clear newer preview owner"
    );
    client.call("theme.cancel_preview", json!({}));
    let sid = client.call("status.get", json!({}))["session"]["session_id"].as_str().unwrap().to_string();
    let p1 = client.call("agent.prompt", json!({ "session_id": sid, "input": "before" }));
    client.call("agent.wait", json!({ "session_id": sid, "turn_id": p1["turn_id"] }));

    let registered = client.call(
        "theme.register",
        json!({
            "definition": {
                "name": "rpc-custom",
                "base": "dark",
                "styles": { "RpcOnly": { "fg": "#123456" } },
                "icons": { "file": "F" },
            }
        }),
    );
    let reg_keys: Vec<&str> = registered.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(reg_keys, vec!["registration_id"], "{registered}");

    let preview = client.call("theme.preview", json!({ "theme": "rpc-custom" }));
    assert!(preview["active"] == "rpc-custom" && preview["preference"] == before["preference"], "{preview}");
    assert_eq!(preview["icons"]["file"], "F");
    let legacy = client.call_err("theme.preview", json!({ "name": "dark" }));
    assert_eq!(legacy["data"]["code"], "invalid_arguments", "{legacy}");
    assert_eq!(client.call("theme.get", json!({}))["preview"], "rpc-custom");

    let restored = client.call("theme.cancel_preview", json!({}));
    assert!(restored["active"] == before["active"] && restored["styles"].get("RpcOnly").is_none(), "{restored}");

    let selected = client.call("theme.select", json!({ "theme": "rpc-custom", "save": false }));
    assert_eq!(selected["active"], "rpc-custom");

    let layer = client.call(
        "theme.register_override",
        json!({ "layer_id": "layer", "styles": { "RpcOverrideOnly": { "fg": "#abcdef" } } }),
    );
    assert!(client.call("theme.get", json!({}))["styles"].get("RpcOverrideOnly").is_some());
    assert_eq!(client.call("registrations.release", json!({ "registration_id": layer["registration_id"] })), json!({}));
    assert!(client.call("theme.get", json!({}))["styles"].get("RpcOverrideOnly").is_none());
    assert_eq!(
        client.call("registrations.release", json!({ "registration_id": layer["registration_id"] })),
        json!({}),
        "release is idempotent"
    );

    for (method, params) in [
        ("theme.select", json!({})),
        ("theme.select", json!({ "theme": 12 })),
        ("theme.select", json!({ "theme": "dark", "save": "yes" })),
        ("theme.register", json!({ "definition": { "name": "bad", "palette": { "red": "nonsense" } } })),
        ("theme.register", json!({ "definition": { "name": "badxml", "syntax": "bad xml", "palette": {} } })),
    ] {
        client.call_err(method, params);
    }
    assert_eq!(client.call("theme.get", json!({}))["active"], "rpc-custom");
    let p2 = client.call("agent.prompt", json!({ "session_id": sid, "input": "after" }));
    client.call("agent.wait", json!({ "session_id": sid, "turn_id": p2["turn_id"] }));
    client.call("registrations.release", json!({ "registration_id": registered["registration_id"] }));
    assert_eq!(client.call("theme.get", json!({}))["preference"], "auto");
    client.shutdown_and_wait();
}

#[tokio::test]
async fn theme_slash_command_exercises_direct_and_picker_paths() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["theme", "theme-default", "commands"]).build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local notes = {}
            local notify_tx, notify_rx = mare.task.channel()
            local orig_notify = ui.notify
            local orig_select = ui.select
            ui.notify = function(msg, level)
                notes[#notes + 1] = tostring(level) .. ":" .. tostring(msg)
                notify_tx:send(#notes)
            end

            local cmds = C.cordis:get("commands")
            local theme = C.cordis:get("theme")

            cmds:run("/theme dark")
            notify_rx:recv()
            assert(theme:get().preference == "dark", "direct /theme dark must save preference")

            cmds:run("/theme no-such-theme")
            notify_rx:recv()
            assert(notes[#notes]:find("warning:", 1, true), "unknown theme must warn: " .. notes[#notes])

            -- Picker cancel after preview highlight (wait until cancel_preview finishes)
            local highlighted = nil
            local cancelled_tx, cancelled_rx = mare.task.channel()
            local orig_cancel = theme.cancel_preview
            theme.cancel_preview = function(self, ...)
                local r = orig_cancel(self, ...)
                cancelled_tx:send(true)
                return r
            end
            ui.select = function(opts)
                assert(opts.selected >= 1)
                if opts.on_highlight and #opts.items >= 2 then
                    highlighted = opts.on_highlight(2)
                end
                return nil
            end
            cmds:run("/theme")
            cancelled_rx:recv()
            theme.cancel_preview = orig_cancel
            assert(type(highlighted) == "table", "on_highlight must return preview component")

            -- Picker confirm selection (auto = index 1)
            ui.select = function() return 1 end
            cmds:run("/theme")
            notify_rx:recv()
            assert(theme:get().preference == "auto", "picker selection 1 must restore auto")

            -- Picker error path
            ui.select = function() error("cancelled by harness") end
            cmds:run("/theme")
            notify_rx:recv()
            assert(notes[#notes]:find("warning:Theme preview failed", 1, true), notes[#notes])

            ui.notify = orig_notify
            ui.select = orig_select
            return true
        "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}
