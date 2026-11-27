//! The built-in `permissions` plugin: presets → sandbox + approval, untrusted
//! projects, confirmations, session approvals, argv prefix rules, unconfined tools, sub-agents,
//! headless and the disabled plugin. One focused test per behavior.

use crate::common::*;
use mare_shared::ui::Mode;
use serde_json::{Value, json};

mod approval_policy;
mod child_terminal;
mod followups;
mod presets;
mod protected_paths;
mod request;
mod rules;
mod scopes;
mod sensitive;
mod shell;
mod subagents;

/// a host with the permissions plugin enabled; `trusted` = project trust
async fn host(env: &TestEnv, trusted: bool, opts: Value, args: Value) -> std::sync::Arc<mare_host::Host> {
    let h = env
        .host()
        .builtins(&["jobs", "tools-apply-patch"])
        .config(json!({ "plugins": { "permissions": merge_into(json!({ "enabled": true }), opts) } }))
        .args(args)
        .project_trusted(trusted)
        .build_exposed()
        .await;
    global(&h, "P", h.service_table("permissions").unwrap());
    h
}

/// Whether this platform's sandbox tier controls the network (`net_block` / `net_proxy`). The
/// Windows default tier can't: there a preset that restricts the network is badged PARTIAL and
/// read-only asks before every shell command (permissions/presets.lua `no_net_control`, vote.lua
/// `net_note`) — product rules, asserted as such where a test meets them.
async fn net_controlled(h: &mare_host::Host) -> bool {
    eval(
        h,
        r#"local c = mare.proc.sandbox.capabilities()
           if c.mechanism == "none" then return true end
           for _, x in ipairs(c.caps or {}) do
               if x == "net_block" or x == "net_proxy" then return true end
           end
           return false"#,
    )
    .await
}

fn merge_into(mut a: Value, b: Value) -> Value {
    merge(&mut a, b);
    a
}

/// `SHELL_WRITE`: a model shell call that writes `shell` to `shell.txt` in its cwd. Inside the
/// Windows default-tier sandbox Git Bash can't start (SANDBOX_MSYS_UNSUPPORTED) and the
/// `powershell` tool is the model's shell there; elsewhere it is `bash`.
const SHELL_WRITE: &str = r#"
    SHELL_WRITE = mare.os == "windows"
        and { name = "powershell", arguments = { command = "Set-Content -NoNewline -Path shell.txt -Value shell" } }
        or { name = "bash", arguments = { command = "printf shell > shell.txt" } }
"#;

/// the vote for a call through the real `tool/before` dispatch
const VOTE: &str = r#"
    function VOTE(call, info)
        local def = T and call and call.name and T:get(call.name) or nil
        local merged = { side_effect = def and def.side_effect or "external" }
        if def then
            merged.idempotent = def.idempotent
            merged.unconfined = def.unconfined
            merged.permission_request = def.permission_request == true
            merged.command_tool = def.command_tool == true
            merged.command = def.command
        end
        for k, v in pairs(info or {}) do
            merged[k] = v
        end
        return C.cordis:get("policy"):decide("tool/before", { call, merged })
    end
    -- a no-op program that starts inside every sandbox tier: Windows' msys `true.exe` can't
    -- start in the default-tier sandbox (SANDBOX_MSYS_UNSUPPORTED), cmd.exe can
    function TRUE(spec)
        if mare.os == "windows" then spec.cmd, spec.args = "cmd.exe", { "/d", "/c", "exit 0" }
        else spec.cmd = "true" end
        return spec
    end
    function BASH(cmd, scope, why)
        return { id = "c" .. math.random(1e9), name = "bash",
                 arguments = { command = cmd, sandbox_permissions = scope, justification = why } }
    end
"#;

#[tokio::test]
async fn user_plugin_registers_an_approval_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/approval_snap_probe.lua"),
            r#"return {
                name = "approval-snap-probe",
                apply = function()
                    mare._reg_snap = function()
                        local binding = { owner = "user-svc", turn = "t1", request = "r1", snapshot = "snap1" }
                        local action = { call = { name = "bash", arguments = { command = "echo hi" } } }
                        local lease = mare.approvals.register_snapshot(binding, action)
                        assert(type(lease) == "userdata", "register_snapshot must return SnapshotLease userdata")
                        local dup_ok = pcall(mare.approvals.register_snapshot, binding, action)
                        assert(not dup_ok, "duplicate snapshot binding must still be rejected while lease is active")
                        lease:release()
                        local lease2 = mare.approvals.register_snapshot(binding, action)
                        lease2:release()
                        return true
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let ok: bool = eval(&h, "return mare._reg_snap()").await;
        assert!(ok, "user plugin must be allowed to register an approval snapshot without chunk-name gate");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn human_approval_still_needs_matching_evidence_once() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let binding = mare_binding::approval::Binding::from_json(&json!({
            "owner": "user-svc",
            "turn": "t1",
            "request": "r1",
            "snapshot": "snap1",
        }))
        .unwrap();
        let token = mare_binding::approval::issue(h.lua(), binding, json!({ "cmd": "echo ok" }), "allow");
        global(&h, "TOK", token);
        let (wrong_args, first_ok, second_ok): (bool, bool, bool) = eval(
            &h,
            r#"
            local b = { owner = "user-svc", turn = "t1", request = "r1", snapshot = "snap1" }
            local wrong = mare.approvals.consume_human(TOK, b, { cmd = "rm -rf /" }, nil, "allow")
            local ok1 = mare.approvals.consume_human(TOK, b, { cmd = "echo ok" }, nil, "allow")
            local ok2 = mare.approvals.consume_human(TOK, b, { cmd = "echo ok" }, nil, "allow")
            return wrong, ok1, ok2
            "#,
        )
        .await;
        assert!(!wrong_args, "mismatched arguments must never consume human approval");
        assert!(first_ok, "matching evidence must consume once");
        assert!(!second_ok, "human approval evidence must be single-use");
        h.shutdown().await;
    })
    .await;
}
