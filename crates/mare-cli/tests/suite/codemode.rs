//! code mode `cells.*` contract + host callback + RPC parity.

use crate::common::*;
use serde_json::json;
use std::process::Stdio;

fn spawn_rpc_with_probe(home: &std::path::Path) -> RpcClient {
    let plugins = home.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("codemode_probe.lua"),
        r#"return {
  name = "codemode-probe",
   inject = { "tools" },
  apply = function(ctx)
    ctx.tools:register("echo_tool", {
      description = "Echo probe tool for codemode host callback",
      side_effect = "read",
      parameters = {
        type = "object",
        additionalProperties = false,
        required = { "msg" },
        properties = { msg = { type = "string" } },
      },
      execute = function(args)
        return { content = "echo:" .. tostring(args.msg), value = { echoed = args.msg } }
      end,
    })
  end,
}"#,
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home)
        .current_dir(home)
        .stderr(Stdio::null());
    RpcClient::spawn_cmd(&mut cmd)
}

#[test]
fn rpc_sandbox_run_host_callback_and_capabilities() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn_rpc_with_probe(home.path());

    let init = rpc.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    let sid = init["session"]["session_id"].as_str().unwrap().to_owned();

    let catalog = rpc.call("cells.list_tools", json!({ "session_id": sid }));
    assert!(
        catalog["data"].as_array().unwrap().iter().any(|entry| entry["name"].as_str() == Some("echo_tool")),
        "cells.list_tools missing echo_tool: {catalog}"
    );

    let denied = rpc.call(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "no-cap",
            "code": "return { has_echo = tools.echo_tool ~= nil }",
            "capabilities": []
        }),
    );
    assert_eq!(denied["status"], "completed", "{denied}");
    assert_eq!(denied["result"]["has_echo"], false, "{denied}");

    let ok = rpc.call(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "with-cap",
            "code": "text('before'); print('log'); return tools.echo_tool({ msg = 'hello-c16' })",
            "capabilities": ["echo_tool"]
        }),
    );
    assert_eq!(ok["cell_id"], "with-cap", "{ok}");
    assert_eq!(ok["status"], "completed", "{ok}");
    assert!(ok.get("is_error").is_none() && ok.get("cancelled").is_none(), "{ok}");
    assert_eq!(ok["result"]["echoed"], "hello-c16", "{ok}");
    let items = ok["content"].as_array().unwrap();
    assert!(items.iter().any(|item| item["text"].as_str() == Some("before")), "{ok}");
    assert!(items.iter().any(|item| item["text"].as_str().unwrap_or("").contains("log")), "{ok}");

    rpc.shutdown_and_wait();
}

#[test]
fn rpc_sandbox_start_wait_list_cancel_and_reset() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn_rpc_with_probe(home.path());

    let init = rpc.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    let sid = init["session"]["session_id"].as_str().unwrap().to_owned();

    let started = rpc.call(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "cell-1",
            "code": "text('started'); yield_control(); sleep(5000); return 'done'",
            "yield_time_ms": 1000
        }),
    );
    assert_eq!(started["cell_id"], "cell-1", "{started}");
    assert_eq!(started["status"], "running", "{started}");

    let listed = rpc.call("cells.list", json!({ "session_id": sid }));
    assert!(
        listed["data"].as_array().unwrap().iter().any(|c| c["cell_id"] == "cell-1" && c["status"] == "running"),
        "{listed}"
    );

    let cancelled = rpc.call("cells.cancel", json!({ "session_id": sid, "cell_id": "cell-1" }));
    assert_eq!(cancelled, json!({}), "{cancelled}");

    let waited = rpc.call("cells.wait", json!({ "session_id": sid, "cell_id": "cell-1", "yield_time_ms": 2000 }));
    assert_eq!(waited["status"], "cancelled", "{waited}");

    let status = rpc.call("cells.get", json!({ "session_id": sid }));
    assert!(status.is_object(), "{status}");

    let reset = rpc.call("cells.reset", json!({ "session_id": sid }));
    assert_eq!(reset, json!({}), "{reset}");

    rpc.shutdown_and_wait();
}

fn spawn_rpc_with_args(home: &std::path::Path, extra_args: &[&str]) -> RpcClient {
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo"]);
    cmd.args(extra_args);
    cmd.env("MARE_HOME", home).current_dir(home).stderr(Stdio::null());
    RpcClient::spawn_cmd(&mut cmd)
}

#[test]
fn rpc_sandbox_run_denies_nested_write_without_yolo_or_approvals() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn_rpc_with_args(home.path(), &[]);

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |req| {
        panic!("unexpected server request without approvals capability: {req}");
    });
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();

    let target = home.path().join("unapproved.txt");
    let res = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "write-unapproved",
            "code": "return tools.write({ path = 'unapproved.txt', content = 'should-not-write' })",
            "capabilities": ["write"]
        }),
        |req| panic!("unexpected server request without approvals capability: {req}"),
    );
    assert!(res.get("error").is_none(), "RPC transport error: {res}");
    assert_eq!(res["result"]["status"], "failed", "expected nested write to be denied: {res}");
    assert!(!target.exists(), "unapproved nested write must not create {}", target.display());

    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_sandbox_run_nested_write_requests_approval_under_default_permissions() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn_rpc_with_args(home.path(), &[]);

    let init = rpc.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }),
        |req| {
            panic!("unexpected server request during initialize: {req}");
        },
    );
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();

    let target = home.path().join("default-perms.txt");
    let mut approval_tools = Vec::new();
    let denied = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "write-denied-default",
            "code": "return tools.write({ path = 'default-perms.txt', content = 'denied-content' })",
            "capabilities": ["write"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            approval_tools.push(req["params"]["tool"].as_str().unwrap_or("").to_string());
            json!({ "decision": "deny", "reason": "denied by test" })
        },
    );
    assert!(denied.get("error").is_none(), "RPC transport error: {denied}");
    assert_eq!(denied["result"]["status"], "failed", "expected denied write to error: {denied}");
    assert_eq!(approval_tools, vec!["write"]);
    assert!(!target.exists(), "denied nested write must not create {}", target.display());

    // Even if approved interactively, default read-only sandbox still blocks file writes.
    let approved_ro = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "write-approved-readonly",
            "code": "return tools.write({ path = 'default-perms.txt', content = 'ro-blocked' })",
            "capabilities": ["write"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            approval_tools.push(req["params"]["tool"].as_str().unwrap_or("").to_string());
            json!({ "decision": "allow" })
        },
    );
    assert!(approved_ro.get("error").is_none(), "RPC transport error: {approved_ro}");
    assert_eq!(approved_ro["result"]["status"], "failed", "read-only sandbox must still block write: {approved_ro}");
    assert_eq!(approval_tools, vec!["write", "write"]);
    assert!(!target.exists(), "read-only sandbox must not create {}", target.display());

    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_sandbox_run_nested_write_obeys_rpc_approval_flow() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn_rpc_with_args(home.path(), &["--permission", "auto", "--approval-policy", "untrusted"]);

    let init = rpc.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }),
        |req| {
            panic!("unexpected server request during initialize: {req}");
        },
    );
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();

    let target = home.path().join("approved.txt");
    let mut approval_tools = Vec::new();
    let denied = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "write-denied",
            "code": "return tools.write({ path = 'approved.txt', content = 'denied-content' })",
            "capabilities": ["write"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            let tool = req["params"]["tool"].as_str().unwrap_or("").to_string();
            let decision = if tool == "write" { "deny" } else { "allow" };
            approval_tools.push(tool);
            json!({ "decision": decision, "reason": "test decision" })
        },
    );
    assert!(denied.get("error").is_none(), "RPC transport error: {denied}");
    assert_eq!(denied["result"]["status"], "failed", "expected denied write to error: {denied}");
    assert_eq!(approval_tools, vec!["_sandbox_script", "write"]);
    assert!(!target.exists(), "denied nested write must not create {}", target.display());

    let approved = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "write-approved",
            "code": "return tools.write({ path = 'approved.txt', content = 'approved-content' })",
            "capabilities": ["write"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            approval_tools.push(req["params"]["tool"].as_str().unwrap_or("").to_string());
            json!({ "decision": "allow" })
        },
    );
    assert!(approved.get("error").is_none(), "RPC transport error: {approved}");
    assert_eq!(approved["result"]["status"], "completed", "expected approved write to succeed: {approved}");
    assert_eq!(approval_tools, vec!["_sandbox_script", "write", "_sandbox_script", "write"]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved-content");

    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_codemode_mcp_tier_enforces_isolation_approvals_and_disabled_list() {
    let home = tempfile::tempdir().unwrap();
    let cm_log = home.path().join("cm.log");
    let def_log = home.path().join("def.log");
    let unapproved_log = home.path().join("unapproved.log");
    let bin = serde_json::to_string(fake_mcp_bin()).unwrap();
    let cm_cfg = json!({ "log": cm_log, "tools": ["echo", "blocked"] }).to_string();
    let def_cfg = json!({ "log": def_log, "tools": ["echo"] }).to_string();
    let unapproved_cfg = json!({ "log": unapproved_log, "tools": ["echo"] }).to_string();

    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        format!(
            r#"
mare.setup {{ model = "faux/echo" }}
mare.plugin("mcp", {{
  servers = {{
    cm = {{
      command = {bin},
      args = {{ "scripted", {} }},
      tier = "codemode",
      disabled_tools = {{ "blocked" }},
    }},
    def = {{
      command = {bin},
      args = {{ "scripted", {} }},
      tier = "deferred",
    }},
  }}
}})
"#,
            serde_json::to_string(&cm_cfg).unwrap(),
            serde_json::to_string(&def_cfg).unwrap(),
        ),
    )
    .unwrap();
    std::fs::write(
        home.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "unapproved": {
                    "command": fake_mcp_bin(),
                    "args": ["scripted", unapproved_cfg],
                    "tier": "codemode"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let mut rpc = spawn_rpc_with_args(home.path(), &["--permission", "auto", "--approval-policy", "untrusted"]);

    let init = rpc.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }),
        |req| {
            panic!("unexpected server request during initialize: {req}");
        },
    );
    assert!(init.get("error").is_none(), "{init}");

    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();
    let catalog = rpc.call_raw("cells.list_tools", json!({ "session_id": sid }), |req| {
        panic!("unexpected approval request during cells.list_tools: {req}");
    });
    let names: Vec<&str> =
        catalog["result"]["data"].as_array().unwrap().iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"mcp__cm__echo"), "missing codemode MCP tool: {names:?}");
    assert!(!names.contains(&"mcp__cm__blocked"), "disabled tool leaked into catalog: {names:?}");
    assert!(!names.contains(&"mcp__def__echo"), "deferred tool leaked into codemode catalog: {names:?}");
    assert!(!names.contains(&"mcp__unapproved__echo"), "unapproved project server leaked: {names:?}");
    assert!(!unapproved_log.exists(), "unapproved project server must not start");

    let filtered = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "filtered-caps",
            "code": "return { blocked = tools.mcp__cm__blocked == nil, def = tools.mcp__def__echo == nil, unapproved = tools.mcp__unapproved__echo == nil }",
            "capabilities": ["mcp__cm__blocked", "mcp__def__echo", "mcp__unapproved__echo"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            json!({ "decision": "allow" })
        },
    );
    assert_eq!(filtered["result"]["status"], "completed", "{filtered}");
    assert_eq!(filtered["result"]["result"], json!({ "blocked": true, "def": true, "unapproved": true }));

    let mut asked = Vec::new();
    let denied = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "mcp-denied",
            "code": "return tools.mcp__cm__echo({ text = 'hi-denied' })",
            "capabilities": ["mcp__cm__echo"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            let tool = req["params"]["tool"].as_str().unwrap_or("").to_string();
            let decision = if tool == "mcp__cm__echo" { "deny" } else { "allow" };
            asked.push(tool);
            json!({ "decision": decision, "reason": "denied by test" })
        },
    );
    assert_eq!(denied["result"]["status"], "failed", "denied MCP call must fail: {denied}");
    assert_eq!(asked, vec!["_sandbox_script", "mcp__cm__echo"]);
    let cm_lines = std::fs::read_to_string(&cm_log).unwrap_or_default();
    assert_eq!(
        cm_lines.lines().filter(|l| *l == "tools/call").count(),
        0,
        "denied MCP call must not reach server: {cm_lines}"
    );

    let allowed = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "mcp-allowed",
            "code": "return tools.mcp__cm__echo({ text = 'hi-allowed' })",
            "capabilities": ["mcp__cm__echo"]
        }),
        |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            asked.push(req["params"]["tool"].as_str().unwrap_or("").to_string());
            json!({ "decision": "allow" })
        },
    );
    assert_eq!(allowed["result"]["status"], "completed", "approved MCP call must succeed: {allowed}");
    assert_eq!(allowed["result"]["result"]["content"][0]["text"], "echo: hi-allowed", "{allowed}");
    assert_eq!(asked, vec!["_sandbox_script", "mcp__cm__echo", "_sandbox_script", "mcp__cm__echo"]);
    let cm_lines_after = std::fs::read_to_string(&cm_log).unwrap_or_default();
    assert_eq!(cm_lines_after.lines().filter(|l| *l == "tools/call").count(), 1);

    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[cfg(target_os = "linux")]
fn retained_worker_pids(parent_pid: u32) -> Vec<u32> {
    let path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.split_whitespace()
        .filter_map(|tok| tok.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|bytes| bytes.windows(b"__lua-vm-worker".len()).any(|w| w == b"__lua-vm-worker"))
                .unwrap_or(false)
        })
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn persistent_sandbox_worker_is_closed_on_worker_limit_and_timeout() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "mare.setup { model = \"faux/echo\" }\n\
         mare.plugin(\"cells\", { execution_mode = \"persistent-in-memory\", max_instructions = 50000 })\n",
    )
    .unwrap();
    let mut rpc = spawn_rpc_with_args(home.path(), &["--yolo"]);
    let mare_pid = rpc.pid();

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");

    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();

    let seed = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "seed",
            "code": "counter = 7; function inc() counter = counter + 1; return counter end; return counter"
        }),
        |_| json!(null),
    );
    assert_eq!(seed["result"]["status"], "completed", "{seed}");
    assert_eq!(seed["result"]["result"], 7, "{seed}");

    let initial_pids = retained_worker_pids(mare_pid);
    assert_eq!(initial_pids.len(), 1, "expected 1 retained worker pid: {initial_pids:?}");

    // Normal Lua script error must retain the persistent worker and state.
    let script_err = rpc.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "err", "code": "counter = 9; error('expected')" }),
        |_| json!(null),
    );
    assert_eq!(script_err["result"]["status"], "failed", "{script_err}");
    let after_err = rpc.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "after-err", "code": "return inc()" }),
        |_| json!(null),
    );
    assert_eq!(after_err["result"]["result"], 10, "{after_err}");
    assert_eq!(retained_worker_pids(mare_pid), initial_pids, "normal Lua error should keep the same worker");

    // Worker hits instruction limit long before host's 60s timeout and sends Done{error}:
    // host must close/reap the persistent worker rather than leaving it alive.
    let limit_abort = rpc.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "limit", "code": "while true do end" }),
        |_| json!(null),
    );
    assert_eq!(limit_abort["result"]["status"], "failed", "{limit_abort}");
    for pid in &initial_pids {
        assert!(!pid_alive(*pid), "aborted persistent worker pid {pid} survived limit abort");
    }

    let after_limit = rpc.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "after-limit", "code": "assert(counter == nil); return 'cleared'" }),
        |_| json!(null),
    );
    assert_eq!(after_limit["result"]["result"], "cleared", "{after_limit}");
    assert!(
        after_limit["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"].as_str().unwrap_or("").contains("environment was lost")),
        "expected state_lost notice after worker close: {after_limit}"
    );

    rpc.shutdown_and_wait();

    // Separate session without max_instructions override so timeout_ms=50 genuinely hits the 50ms timeout.
    let timeout_home = tempfile::tempdir().unwrap();
    let timeout_cfg = timeout_home.path().join("config");
    std::fs::create_dir_all(&timeout_cfg).unwrap();
    std::fs::write(
        timeout_cfg.join("init.lua"),
        "mare.setup { model = \"faux/echo\" }\n\
         mare.plugin(\"cells\", { execution_mode = \"persistent-in-memory\" })\n",
    )
    .unwrap();
    let mut t_rpc = spawn_rpc_with_args(timeout_home.path(), &["--yolo"]);
    let t_mare_pid = t_rpc.pid();
    let t_init =
        t_rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(t_init.get("error").is_none(), "{t_init}");
    let t_sid = t_init["result"]["session"]["session_id"].as_str().unwrap().to_owned();
    let t_seed = t_rpc.call_raw(
        "cells.start",
        json!({ "session_id": t_sid, "cell_id": "t-seed", "code": "counter = 7; return counter" }),
        |_| json!(null),
    );
    assert_eq!(t_seed["result"]["result"], 7, "{t_seed}");
    let timeout_pids = retained_worker_pids(t_mare_pid);
    assert_eq!(timeout_pids.len(), 1, "expected 1 worker pid before timeout: {timeout_pids:?}");
    let timed_out = t_rpc.call_raw(
        "cells.start",
        json!({ "session_id": t_sid, "cell_id": "timeout", "code": "sleep(500)", "timeout_ms": 50 }),
        |_| json!(null),
    );
    assert_eq!(timed_out["result"]["status"], "failed", "{timed_out}");
    assert!(
        timed_out["result"]["error"].as_str().unwrap_or("").contains("time limit"),
        "expected time limit error: {timed_out}"
    );
    for pid in &timeout_pids {
        assert!(!pid_alive(*pid), "timed-out persistent worker pid {pid} survived");
    }
    let after_timeout = t_rpc.call_raw(
        "cells.start",
        json!({ "session_id": t_sid, "cell_id": "after-timeout", "code": "assert(counter == nil); return 'cleared'" }),
        |_| json!(null),
    );
    assert_eq!(after_timeout["result"]["result"], "cleared", "{after_timeout}");
    assert!(
        after_timeout["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"].as_str().unwrap_or("").contains("environment was lost")),
        "expected state_lost notice after timeout: {after_timeout}"
    );
    t_rpc.call_raw("shutdown", json!({}), |_| json!(null));
    t_rpc.wait_exit();
}

#[test]
fn cells_contract_methods_paginate_isolate_callers_and_raise_coded_errors() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let marker = home.path().join("lua-cells-contract.txt");
    std::fs::write(
        plugins.join("cells_contract_probe.lua"),
        r#"return {
  name = "cells-contract-probe",
  inject = { "cells", "task", "tools" },
  apply = function(ctx)
    local wc_token = mare.task.token()
    ctx.tools:register("hold_cell", {
      description = "Hold a cell deterministically until cancelled",
      side_effect = "read",
      parameters = {
        type = "object",
        additionalProperties = false,
        properties = { mode = { type = "string" } },
      },
      execute = function(args, tctx)
        if type(args) == "table" and args.mode == "wc" then
          wc_token:cancel()
        end
        tctx.cancel:wait()
        return { content = "held-cancelled", is_error = true, cancelled = true }
      end,
    })
    local out_path = "#
            .to_owned()
            + &serde_json::to_string(&marker.display().to_string()).unwrap()
            + r#"
    ctx:spawn(function()
      local ok, res = pcall(function()
        assert(ctx:get("sandbox") == nil, "legacy sandbox service must be removed without dual path")
        local svc = ctx.cells
        assert(svc, "cells service must be published")

        local miss_ok, miss_err = pcall(function() return svc:wait("missing-cell") end)
        assert(not miss_ok and miss_err.code == "cells.unknown_cell", tostring(miss_err))
        local miss_c_ok, miss_c_err = pcall(function() return svc:cancel("missing-cell") end)
        assert(not miss_c_ok and miss_c_err.code == "cells.unknown_cell", tostring(miss_c_err))

        -- Caller isolation: both callers can run cells with the same cell_id, and neither can wait/cancel the other's.
        local a = svc:start("yield_control(); return tools.hold_cell({})", { cell_id = "shared-id", capabilities = { "hold_cell" }, yield_time_ms = 5000 })
        assert(a.cell_id == "shared-id" and a.status == "running", mare.json.encode_strict(a))
        mare.g._shared_started = true
        while not mare.g._other_done do mare.task.sleep(5) end
        assert(not mare.g._other_err, tostring(mare.g._other_err))

        local dup_ok, dup_err = pcall(function() return svc:start("return 1", { cell_id = "shared-id" }) end)
        assert(not dup_ok and dup_err.code == "cells.duplicate_cell", tostring(dup_err))

        local b = svc:start("yield_control(); return tools.hold_cell({})", { cell_id = "cell-b", capabilities = { "hold_cell" }, yield_time_ms = 5000 })
        assert(b.cell_id == "cell-b" and b.status == "running", mare.json.encode_strict(b))

        -- Full cursor traversal for list and catalog.
        local seen_cells = {}
        local cur
        for _ = 1, 10 do
          local page = svc:list({ cursor = cur, limit = 1 })
          assert(#page.data <= 1)
          for _, item in ipairs(page.data) do seen_cells[item.cell_id] = true end
          cur = page.next_cursor
          if not cur then break end
        end
        assert(not cur and seen_cells["shared-id"] and seen_cells["cell-b"], "list pagination missed entries")
        svc:cancel("cell-b")
        svc:wait("cell-b", { yield_time_ms = 2000 })

        local cat_count, cat_cur = 0, nil
        for _ = 1, 50 do
          local page = svc:list_tools({ cursor = cat_cur, limit = 1 })
          cat_count = cat_count + #page.data
          cat_cur = page.next_cursor
          if not cat_cur then break end
        end
        assert(not cat_cur and cat_count >= 2, "list_tools pagination failed")

        -- Invalid pagination cursors / limits and entry bounds are rejected synchronously.
        for _, bad in ipairs({ "0", "abc", "inf", "-1" }) do
          local lok, lerr = pcall(function() return svc:list({ cursor = bad }) end)
          assert(not lok and lerr.code == "invalid_arguments", bad)
        end
        local lim_ok, lim_err = pcall(function() return svc:list({ limit = 0 }) end)
        assert(not lim_ok and lim_err.code == "invalid_arguments")
        local tm_ok, tm_err = pcall(function() return svc:start("return 1", { timeout_ms = 600001 }) end)
        assert(not tm_ok and tm_err.code == "invalid_arguments", tostring(tm_err))
        local so_ok, so_err = pcall(function() return svc:start("return 1", { state_owner = ("x"):rep(256) }) end)
        assert(not so_ok and so_err.code == "invalid_arguments", tostring(so_err))

        -- Contract callbacks: wait_cancel on start & cancel on wait.
        local wc = svc:start("return tools.hold_cell({ mode = 'wc' })", {
          cell_id = "wc-cell",
          capabilities = { "hold_cell" },
          yield_time_ms = 5000,
          wait_cancel = function() return wc_token end,
        })
        assert(wc.status == "running" and wc.cancelled == nil and wc.is_error == nil, mare.json.encode_strict(wc))
        local wc_gone, wc_err = pcall(function() return svc:wait("wc-cell") end)
        assert(not wc_gone and wc_err.code == "cells.unknown_cell", "wait_cancel must clean up unhanded cell")

        local hold = svc:start("yield_control(); return tools.hold_cell({})", {
          cell_id = "cb-wait",
          capabilities = { "hold_cell" },
          yield_time_ms = 5000,
        })
        assert(hold.status == "running")
        local w2 = mare.task.token()
        mare.task.spawn(function() mare.task.sleep(30); w2:cancel() end)
        local wres = svc:wait("cb-wait", { yield_time_ms = 5000, cancel = function() return w2 end })
        assert(wres.status == "running" and wres.cancelled == nil and wres.is_error == nil, mare.json.encode_strict(wres))

        -- Finish & clean up active cells.
        local st = svc:get()
        assert(st.running >= 2)
        assert(next(svc:cancel("shared-id")) == nil)
        assert(svc:wait("shared-id", { yield_time_ms = 2000 }).status == "cancelled")
        assert(next(svc:cancel("cb-wait")) == nil)
        assert(next(svc:cancel("cb-wait")) == nil, "cells.cancel must be idempotent while unconsumed")
        assert(svc:wait("cb-wait", { yield_time_ms = 2000 }).status == "cancelled")

        -- Cell limit reached at 32 active cells.
        local ids = {}
        for i = 1, 32 do
          local id = "lim-" .. i
          local r = svc:start("yield_control(); return tools.hold_cell({})", {
            cell_id = id,
            capabilities = { "hold_cell" },
            yield_time_ms = 5000,
          })
          assert(r.status == "running")
          ids[i] = id
        end
        local ov_ok, ov_err = pcall(function() return svc:start("return 1", { cell_id = "lim-33" }) end)
        assert(not ov_ok and ov_err.code == "cells.cell_limit_reached", tostring(ov_err))
        for _, id in ipairs(ids) do
          svc:cancel(id)
          svc:wait(id, { yield_time_ms = 2000 })
        end

        local rst = svc:reset()
        assert(next(rst) == nil)
        return "OK:" .. a.cell_id .. ":" .. b.cell_id
      end)
      local f = assert(io.open(out_path, "w"))
      f:write(ok and res or ("ERR:" .. tostring(res)))
      f:close()
    end)
  end,
}"#,
    )
    .unwrap();
    std::fs::write(
        plugins.join("cells_second_caller.lua"),
        r#"return {
  name = "cells-second-caller",
  inject = { "cells", "task" },
  apply = function(ctx)
    ctx:spawn(function()
      local ok, err = pcall(function()
        while not mare.g._shared_started do mare.task.sleep(5) end
        local other = ctx.cells
        local oa = other:start("yield_control(); return tools.hold_cell({})", { cell_id = "shared-id", capabilities = { "hold_cell" }, yield_time_ms = 5000 })
        assert(oa.cell_id == "shared-id" and oa.status == "running")
        other:cancel("shared-id")
        other:wait("shared-id", { yield_time_ms = 2000 })
      end)
      if not ok then mare.g._other_err = tostring(err) end
      mare.g._other_done = true
    end)
  end,
}"#,
    )
    .unwrap();

    let mut rpc = spawn_rpc_with_probe(home.path());
    let _init = rpc.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    let t0 = std::time::Instant::now();
    let result = loop {
        if let Ok(text) = std::fs::read_to_string(&marker)
            && !text.is_empty()
        {
            break text;
        }
        if t0.elapsed() >= std::time::Duration::from_secs(30) {
            panic!("lua cells contract probe timed out; other_err={:?}", std::fs::read_to_string(&marker).ok(),);
        }
        std::thread::yield_now();
    };
    assert_eq!(result, "OK:shared-id:cell-b");
    rpc.shutdown_and_wait();
}

#[test]
fn rpc_cells_enforce_os_sandbox_and_protected_path_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join(".mare")).unwrap();
    let secret = home.path().join("outside-secret.txt");
    std::fs::write(&secret, "TOP_SECRET_MARKER_B11").unwrap();
    let protected_target = work.join(".mare/injected.txt");
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        format!(
            "mare.setup {{ model = \"faux/echo\" }}\n\
             mare.plugin(\"permissions\", {{ deny_read = {{ {} }} }})\n",
            serde_json::to_string(&secret).unwrap()
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session", "--model", "faux/echo", "--permission", "auto", "--approval-policy", "never"])
        .env("MARE_HOME", home.path())
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();

    // Zero dual paths: old sandbox.* RPC methods must return MethodNotFound (-32601).
    for old_method in ["sandbox.run", "sandbox.start", "sandbox.list"] {
        let old = rpc.call_raw(old_method, json!({}), |_| json!(null));
        assert_eq!(old["error"]["code"], -32601, "legacy {old_method} must be gone: {old}");
    }

    // Protected path (.mare) write via tools.write inside a cell must be denied.
    let write_protected = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "write-protected",
            "code": "return tools.write({ path = '.mare/injected.txt', content = 'owned' })",
            "capabilities": ["write"],
            "yield_time_ms": 5000
        }),
        |_| json!(null),
    );
    assert!(write_protected.get("error").is_none(), "{write_protected}");
    assert_eq!(write_protected["result"]["status"], "failed", "write to .mare must be denied: {write_protected}");
    assert!(!protected_target.exists(), "protected .mare file must not be created");

    // deny-read file read via tools.read inside a cell must be denied and never leak secret text.
    let read_code = format!("return tools.read({{ path = {} }})", serde_json::to_string(&secret).unwrap());
    let read_secret = rpc.call_raw(
        "cells.start",
        json!({
            "session_id": sid,
            "cell_id": "read-deny",
            "code": read_code,
            "capabilities": ["read"],
            "yield_time_ms": 5000
        }),
        |_| json!(null),
    );
    assert!(read_secret.get("error").is_none(), "{read_secret}");
    assert_eq!(read_secret["result"]["status"], "failed", "deny-read path must be denied: {read_secret}");
    assert!(!read_secret.to_string().contains("TOP_SECRET_MARKER_B11"), "secret leaked through cell: {read_secret}");

    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[cfg(target_os = "linux")]
#[test]
fn rpc_reload_reaps_active_cells_and_allows_non_blocking_cancel() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        "mare.setup { model = \"faux/echo\" }\nmare.plugin(\"cells\", { execution_mode = \"vm-script\", script_persistence = true })\n",
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--permission", "full-access", "--approval-policy", "never"])
        .env("MARE_HOME", home.path())
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();

    // Warm a retained worker and spawn an active child process inside a running cell.
    let warm = rpc.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "warm", "code": "retained_x = 42; return retained_x", "state_owner": "w1", "yield_time_ms": 5000 }),
        |_| json!(null),
    );
    assert_eq!(warm["result"]["result"], 42, "{warm}");

    let pid_file = home.path().join("reload-cell-child.pid");
    let code = format!(
        "return tools.bash({{ command = {} }})",
        serde_json::to_string(&format!("echo $$ > {}; exec sleep 60", pid_file.display())).unwrap()
    );
    let started = rpc.call_raw(
        "cells.start",
        json!({ "session_id": sid, "cell_id": "reload-proc", "code": code, "capabilities": ["bash"], "yield_time_ms": 400 }),
        |_| json!(null),
    );
    assert_eq!(started["result"]["status"], "running", "{started}");

    let t0 = std::time::Instant::now();
    let child_pid: u32 = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(15), "child pid never written");
        std::thread::yield_now();
    };
    let proc_dir = std::path::PathBuf::from(format!("/proc/{child_pid}"));
    assert!(proc_dir.exists(), "child process {child_pid} should exist before reload");

    // Run /reload over RPC so plugin reload stops sandbox + retained worker and reaps child_pid.
    std::fs::write(
        home.path().join("config/init.lua"),
        "mare.setup { model = \"faux/echo\" }\nmare.plugin(\"cells\", { execution_mode = \"vm-script\", script_persistence = true })\n-- bumped\n",
    )
    .unwrap();
    let reloaded = rpc.call_raw("commands.run", json!({ "line": "/reload" }), |_| json!(null));
    assert!(reloaded.get("error").is_none(), "{reloaded}");

    let deadline = std::time::Instant::now();
    while proc_dir.exists() {
        assert!(deadline.elapsed() < std::time::Duration::from_secs(15), "child pid {child_pid} survived reload");
        std::thread::yield_now();
    }
    let st_after = rpc.call_raw("status.get", json!({}), |_| json!(null));
    let sid_after = st_after["result"]["session"]["id"].as_str().unwrap_or(sid.as_str()).to_owned();
    let listed = rpc.call_raw("cells.list", json!({ "session_id": sid_after }), |_| json!(null));
    assert_eq!(listed["result"]["data"], json!([]), "reload must clear active cells: {listed}");

    // cells.cancel returns cleanly without hanging after reload has completed.
    let cancel_res =
        rpc.call_raw("cells.cancel", json!({ "session_id": sid_after, "cell_id": "reload-proc" }), |_| json!(null));
    assert_eq!(cancel_res["error"]["data"]["code"], "cells.unknown_cell", "{cancel_res}");

    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[tokio::test]
async fn cells_plugin_replaces_sandbox_plugin_and_config_path() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.plugin("cells", { execution_mode = "per-call", max_instructions = 50000 })"#,
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        let (has_cells, has_sandbox, has_cells_plugin, has_sandbox_plugin, max_instr, warnings): (
            bool,
            bool,
            bool,
            bool,
            i64,
            Vec<String>,
        ) = eval(
            &h,
            r#"
            local has_cells, has_sandbox = false, false
            for _, key in ipairs(mare.config_declared()) do
                if key == "plugins.cells.execution_mode" then has_cells = true end
                if key:sub(1, #"plugins.sandbox.") == "plugins.sandbox." then has_sandbox = true end
            end
            local has_cells_plugin, has_sandbox_plugin = false, false
            for _, p in ipairs(mare.plugins.list()) do
                if p.name == "cells" then has_cells_plugin = true end
                if p.name == "sandbox" then has_sandbox_plugin = true end
            end
            return has_cells, has_sandbox, has_cells_plugin, has_sandbox_plugin,
                   mare.config.plugins.cells.max_instructions, mare.config_warnings()
            "#,
        )
        .await;
        assert!(has_cells && has_cells_plugin, "built-in plugin must be declared under plugins.cells");
        assert!(!has_sandbox && !has_sandbox_plugin, "legacy plugins.sandbox declaration must be removed");
        assert_eq!(max_instr, 50000, "plugins.cells option must apply");
        assert!(warnings.is_empty(), "plugins.cells config must produce no warnings: {warnings:?}");
        h.shutdown().await;
    })
    .await
}

#[test]
fn protocol_roots_match_actual_protocol_methods_without_stale_entries() {
    let lua = mlua::Lua::new();
    let raw: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let contracts = mare_kernel::contracts::parse_contracts(&lua, raw).unwrap();
    let expected: std::collections::BTreeSet<&str> = mare_protocol::protocol_methods()
        .keys()
        .map(|m| m.split('.').next().unwrap())
        .filter(|root| !contracts.get(*root).is_some_and(|c| !c.methods.is_empty()))
        .collect();
    let actual: std::collections::BTreeSet<&str> = mare_kernel::contracts::PROTOCOL_ROOTS.iter().copied().collect();
    assert_eq!(actual, expected, "PROTOCOL_ROOTS must match non-contract protocol method roots exactly");
}

/// Calling `cells:start` from an agent-dispatched tool enables `notify()` via
/// `tools:output_channel()` / `tools:send_output()`: the initial result commits first without
/// the notification, and the notification reaches a subsequent model request.
#[test]
fn cells_start_notify_delivers_associated_output_after_initial_result() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins/cell_notify_probe");
    std::fs::create_dir_all(&plugins).unwrap();
    let report_path = home.path().join("notify-report.json");
    let report_str = report_path.to_string_lossy().replace('\\', "/");
    std::fs::write(
        plugins.join("init.lua"),
        format!(
            r#"return {{
                name = "cell-notify-probe",
                inject = {{ "tools", "cells" }},
                apply = function(ctx)
                    local gate = mare.task.token()
                    local last_cell_id = nil
                    ctx.tools:register("cn_gate", {{
                        description = "block until released",
                        side_effect = "read",
                        parameters = {{ type = "object", properties = {{}} }},
                        execute = function()
                            gate:wait()
                            return {{ content = "gate-released" }}
                        end,
                    }})
                    ctx.tools:register("cn_start", {{
                        description = "start a cell that emits partial output and notify()",
                        allow_nested = false,
                        side_effect = "external",
                        parameters = {{
                            type = "object",
                            properties = {{ code = {{ type = "string" }} }},
                            required = {{ "code" }},
                        }},
                        execute = function(args, tctx)
                            local res = ctx.cells:start(args.code, {{
                                capabilities = {{ "cn_gate" }},
                                yield_time_ms = 5000,
                                wait_cancel = function() return tctx.cancel end,
                            }})
                            last_cell_id = res.cell_id
                            return {{ content = mare.json.encode_strict(res), details = res }}
                        end,
                    }})
                    ctx.tools:register("cn_wait", {{
                        description = "release gate and wait for cell completion",
                        allow_nested = false,
                        side_effect = "external",
                        parameters = {{ type = "object", properties = {{}} }},
                        execute = function()
                            gate:cancel()
                            local res = ctx.cells:wait(last_cell_id, {{ yield_time_ms = 5000 }})
                            return {{ content = mare.json.encode_strict(res), details = res }}
                        end,
                    }})
                    ctx:on("request/build", function(request, ev)
                        local encoded = mare.json.encode_strict(request.messages or {{}})
                        if encoded:find("ASSOCIATED_NOTIFY", 1, true) then
                            mare.fs.write({report_str:?}, encoded)
                        end
                        return ev.next(request)
                    end)
                end,
            }}"#
        ),
    )
    .unwrap();

    let code = r#"text("PARTIAL_BEFORE"); notify("ASSOCIATED_NOTIFY"); tools.cn_gate({}); print("PARTIAL_AFTER"); return "cell-ok""#;
    let script_path = home.path().join("script.json");
    std::fs::write(
        &script_path,
        serde_json::to_string(&json!([
            { "tool_calls": [{ "name": "cn_start", "arguments": { "code": code } }] },
            { "tool_calls": [{ "name": "cn_wait", "arguments": {} }] },
            { "text": "all done" }
        ]))
        .unwrap(),
    )
    .unwrap();

    let output = mare_cmd()
        .args(["--yolo", "-p", "--json", "--no-session", "--model", "faux/echo", "run notify cell"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .current_dir(home.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "mare -p failed:\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let results: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("item/completed"))
        .filter(|e| e["data"]["item"]["type"].as_str() == Some("tool_call"))
        .map(|e| e["data"]["item"]["message"].clone())
        .collect();
    assert_eq!(results.len(), 2, "expected 2 tool results, got: {results:?}");

    let start = &results[0]["details"];
    let start_json = results[0].to_string();
    let wait = &results[1]["details"];
    let wait_json = results[1].to_string();
    assert_eq!(start["status"], "running", "initial cell result must yield running: {start}");
    assert!(start_json.contains("PARTIAL_BEFORE"), "initial result must include PARTIAL_BEFORE: {start}");
    assert!(!start_json.contains("ASSOCIATED_NOTIFY"), "initial result must not include notify payload: {start}");
    assert_eq!(wait["status"], "completed", "cell wait must complete cleanly: {wait}");
    assert_eq!(wait["result"], "cell-ok", "cell return value must match: {wait}");
    assert!(wait_json.contains("PARTIAL_AFTER"), "wait result must include PARTIAL_AFTER: {wait}");
    assert!(!wait_json.contains("PARTIAL_BEFORE"), "wait result must not repeat PARTIAL_BEFORE: {wait}");
    assert!(!wait_json.contains("ASSOCIATED_NOTIFY"), "wait result must not repeat ASSOCIATED_NOTIFY: {wait}");
    assert!(
        report_path.exists() && std::fs::read_to_string(&report_path).unwrap().contains("ASSOCIATED_NOTIFY"),
        "notify output did not reach subsequent model request"
    );
}

/// Direct contract test for `tools:output_channel()` and `tools:send_output()`:
/// third-party orchestrators supplying `on_output` to `tools:execute` get the same capability as
/// the built-in agent, with initial-commit gating, secret redaction, ownership/close checks, and
/// unavailability outside active top-level executions.
#[tokio::test]
async fn tools_output_channel_direct_third_party_lifecycle_and_projection() {
    local(async {
        let env = TestEnv::new().await;
        let plugin_dir = env.config_dir().join("plugins/out_chan_probe");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("init.lua"),
            r#"return {
                name = "out-chan-probe",
                inject = { "tools" },
                apply = function(ctx)
                    local opened_channel = nil
                    ctx.tools:register("oc_tool", {
                        description = "open an output channel during execution",
                        side_effect = "read",
                        parameters = { type = "object", properties = {} },
                        execute = function()
                            local ch, err = ctx.tools:output_channel()
                            if not ch then
                                return { content = "no-channel:" .. tostring(err) }
                            end
                            opened_channel = ch
                            local pending = ctx.tools:send_output(ch, { { type = "text", text = "early" } })
                            return { content = "opened:" .. tostring(pending.status) .. ":" .. tostring(pending.error) }
                        end,
                    })
                    mare._oc_run = function()
                        -- 1. Outside any execution: unavailable.
                        local ch0, err0 = ctx.tools:output_channel()
                        assert(ch0 == nil and err0 == "active_output_unavailable", "outside: " .. tostring(err0))

                        -- 2. Forged channel handle is rejected.
                        local forged = ctx.tools:send_output({ close = function() end }, { { type = "text", text = "x" } })
                        assert(forged.status == "rejected" and forged.error == "invalid_channel", "forged: " .. tostring(forged.error))

                        -- 3. Execution without on_output: unavailable.
                        local no_sink = ctx.tools:execute({ id = "oc-no-sink", name = "oc_tool", arguments = {} }, {})
                        assert(no_sink.content == "no-channel:active_output_unavailable", "no_sink: " .. tostring(no_sink.content))

                        -- 4. Execution with on_output (third-party orchestrator).
                        local committed = false
                        local delivered = {}
                        local res = ctx.tools:execute({ id = "oc-with-sink", name = "oc_tool", arguments = {} }, {
                            on_output = function(blocks)
                                if not committed then
                                    return { status = "rejected", error = "initial_result_pending" }
                                end
                                delivered[#delivered + 1] = blocks
                                return { status = "accepted" }
                            end,
                        })
                        assert(res.content == "opened:rejected:initial_result_pending", "res: " .. tostring(res.content))
                        assert(opened_channel ~= nil, "channel must be opened")

                        -- 5. Invalid blocks and cancelled token are rejected.
                        local bad = ctx.tools:send_output(opened_channel, {})
                        assert(bad.status == "rejected" and bad.error == "invalid_blocks", "bad: " .. tostring(bad.error))
                        local tok = mare.task.token()
                        tok:cancel()
                        local canc = ctx.tools:send_output(opened_channel, { { type = "text", text = "hi" } }, { cancel = tok })
                        assert(canc.status == "rejected" and canc.error == "cancelled", "canc: " .. tostring(canc.error))

                        -- 6. After commit, secret values registered with mare.projection are redacted.
                        committed = true
                        local secret = "TOP_SECRET_OUT_CHANNEL_VALUE_98765"
                        local lease = mare.projection.register(secret, "[REDACTED]")
                        local sent = ctx.tools:send_output(opened_channel, { { type = "text", text = "tok=" .. secret } })
                        if type(lease) == "function" then lease() elseif lease and lease.release then lease:release() end
                        assert(sent.status == "accepted", "sent: " .. tostring(sent.error))
                        local projected_text = delivered[1] and delivered[1][1] and delivered[1][1].text or ""
                        assert(#delivered == 1 and not projected_text:find(secret, 1, true) and projected_text:find("mare%-value:"), "projected: " .. tostring(projected_text))

                        -- 7. Close is idempotent and rejects subsequent sends.
                        opened_channel:close()
                        opened_channel:close()
                        local after_close = ctx.tools:send_output(opened_channel, { { type = "text", text = "late" } })
                        assert(after_close.status == "rejected" and after_close.error == "channel_closed", "closed: " .. tostring(after_close.error))
                        return "ok"
                    end
                end,
            }"#,
        )
        .unwrap();

        let h = env.host().faux(json!([])).user_plugins().no_session().build().await;
        let res: String = eval(&h, "return mare._oc_run()").await;
        h.shutdown().await;
        assert_eq!(res, "ok");
    })
    .await;
}
