//! `plugins.approvals.policy` over RPC `options.set` / `options.get`: a write is applied by the
//! approvals plugin and waited for — a failed save fails the call and changes nothing, a later refusal in
//! the same batch puts an earlier plugin's write back, a leaf write keeps its siblings, a new
//! session starts from the policy now configured, and a leaf reads the same live state as its
//! option.
//!
//! Reference tests: Kimi `packages/agent-core-v2/test/app/config/config.test.ts:3244` (a failed
//! write leaves memory and file unchanged) and `:3048` (a multi-domain write is all or nothing);
//! reference implementation (a nested update keeps its
//! siblings) and `editor-failures.spec.ts` (an injected I/O failure leaves the patch unchanged —
//! the failure here is injected the same way, by a double over the store); Codex
//! `codex-rs/app-server/src/config_manager_service_tests.rs:1717` (an invalid `approval_policy`
//! write is refused, the file unchanged). None of them tests a new session's default after a
//! runtime write; that case is written here.
use crate::common::*;
use serde_json::{Value, json};
use std::process::Stdio;

/// A store double (as in session/metadata_and_transitions.rs:905): writing the record that saves
/// approval policy `never` fails, every other write goes through.
const FAIL_NEVER_SAVE: &str = r#"
local open = mare.store.open
mare.store.open = function(path, opts)
  local f = open(path, opts)
  if not f then return f end
  return setmetatable({}, { __index = function(_, k)
    if k == "append" or k == "append_batch" then
      return function(_, entry, o)
        local ok, s = pcall(mare.json.encode, entry)
        if ok and s:find("approval/policy", 1, true) and s:find('"never"', 1, true) then
          error("injected policy save failure", 0)
        end
        return f[k](f, entry, o)
      end
    end
    local v = f[k]
    if type(v) == "function" then return function(_, ...) return v(f, ...) end end
    return v
  end })
end
"#;

/// A plugin whose option is applied live and refused on apply when set to "no"; "leak" is refused
/// with a key-shaped value in the error text.
const ZETA: &str = r#"
return { name = "zeta",
  config_schema = { mode = { type = "string", apply = "live", desc = "refused when no" } },
  apply = function(ctx)
    ctx:on("option/apply", function(ev)
      if ev.name == "plugins.zeta.mode" and ev.value == "no" then error("zeta refuses no") end
      if ev.name == "plugins.zeta.mode" and ev.value == "leak" then
        error("zeta refuses: upstream said api_key=sk-proj-0123456789abcdefABCDEF0123456789")
      end
    end)
  end,
}
"#;

/// A plugin with an optional key and no refusal: written before `zeta` in a batch.
const ALPHA: &str = r#"
return { name = "alpha",
  config_schema = { extra = { type = "string", apply = "live", desc = "optional, no default" } },
  apply = function() end,
}
"#;

/// servers with a credential and a field declared sensitive whose name no keyword rule
/// catches; a top-level option declared sensitive, with a default.
const VAULT: &str = r#"
return { name = "vault",
  config_schema = {
    srv = { type = "map", apply = "live", desc = "servers", values = { type = "object", fields = {
      tier = { type = "string", desc = "t" }, env = { type = "map", values = { type = "string" }, desc = "e" },
      pin = { type = "string", sensitive = true, desc = "p" } } } },
    classified = { type = "string", default = "classified-value", sensitive = true, desc = "declared sensitive" },
  },
  apply = function() end,
}
"#;

fn spawn(home: &std::path::Path, init: &str, session: bool) -> RpcClient {
    let plugins = home.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(home.join("config/init.lua"), init).unwrap();
    std::fs::write(plugins.join("zeta.lua"), ZETA).unwrap();
    std::fs::write(plugins.join("alpha.lua"), ALPHA).unwrap();
    std::fs::write(plugins.join("vault.lua"), VAULT).unwrap();
    let mut args = vec!["--rpc", "--model", "faux/echo"];
    if !session {
        args.push("--no-session");
    }
    let mut cmd = mare_cmd();
    cmd.args(args).env("MARE_HOME", home).current_dir(home).stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");
    rpc
}

fn set(rpc: &mut RpcClient, values: Value) -> Value {
    let sid = rpc.session_id();
    rpc.call_raw("options.set", json!({ "session_id": sid, "values": values }), |_| json!(null))
}

fn get(rpc: &mut RpcClient, key: &str) -> Value {
    let sid = rpc.session_id();
    let r = rpc.call_raw("options.get", json!({ "session_id": sid, "keys": [key] }), |_| json!(null));
    r["result"][key].clone()
}

fn finish(mut rpc: RpcClient) {
    rpc.shutdown_and_wait();
}

/// #9: the real approvals plugin's failed save fails `options.set` and the policy and the saved
/// records stay; a write it can save is the positive control. #3: in one batch, `alpha`'s new key
/// is removed and the approvals policy (set with `/approval`, not in the config) returns when a
/// later plugin refuses.
#[test]
fn a_failed_policy_save_or_a_later_refusal_leaves_everything_as_it_was() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn(home.path(), &format!("mare.setup {{ model = 'faux/echo' }}\n{FAIL_NEVER_SAVE}"), true);

    let ok = set(&mut rpc, json!({ "plugins.approvals.policy": "untrusted" }));
    assert!(ok.get("error").is_none(), "positive control: {ok}");
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "untrusted");

    // b35a should-fix: the saved policy records are the same before and after the failed call.
    // The session files only: a failed write's entries go to state/quarantine as evidence, which
    // nothing reads back (session load and resume never replay it)
    let records = |home: &std::path::Path| -> Vec<String> {
        walk(&home.join("state")).lines().filter(|l| l.contains("approval/policy")).map(str::to_string).collect()
    };
    let before = records(home.path());
    assert!(!before.is_empty(), "the positive control saved a record");
    let refused = set(&mut rpc, json!({ "plugins.approvals.policy": "never" }));
    let msg = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("injected policy save failure"), "the failed save is the call's failure: {refused}");
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "untrusted", "the policy stays");
    assert_eq!(records(home.path()), before, "no policy record changed");

    // the live policy now differs from the config: `/approval` changes the session's only
    rpc.call_raw("commands.run", json!({ "line": "/approval ask" }), |_| json!(null));
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "ask");
    let batch = set(
        &mut rpc,
        json!({ "plugins.alpha.extra": "x", "plugins.approvals.policy": "untrusted", "plugins.zeta.mode": "no" }),
    );
    assert!(batch["error"]["message"].as_str().unwrap_or_default().contains("zeta refuses no"), "{batch}");
    assert_eq!(get(&mut rpc, "plugins.alpha.extra"), Value::Null, "the key alpha's write introduced is gone");
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "ask", "the live policy returns, not the config's");
    finish(rpc);

    let saved: String = walk(&home.path().join("state"));
    let never = saved.lines().filter(|l| l.contains("approval/policy") && l.contains(r#""never""#)).collect::<Vec<_>>();
    assert!(never.is_empty(), "no saved `never` policy record: {never:?}");
    // the refused record is kept as quarantine evidence of the failed write, not as session state
    let quarantined = walk_all(&home.path().join("state/quarantine"));
    assert!(
        quarantined.lines().any(|l| l.contains("approval/policy") && l.contains(r#""never""#)),
        "the failed write is quarantined: {quarantined}"
    );
}

fn walk_all(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out += &walk_all(&p);
        } else if let Ok(s) = std::fs::read_to_string(&p) {
            out += &s;
        }
    }
    out
}

/// The session state under `dir`, without the quarantine evidence of failed writes.
fn walk(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|name| name == "quarantine") {
                continue;
            }
            out += &walk(&p);
        } else if let Ok(s) = std::fs::read_to_string(&p) {
            out += &s;
        }
    }
    out
}

/// #4: a leaf write changes that leaf of the current policy and keeps `mode` and the sibling
/// categories; #8: after the service changes the policy, a leaf and its option agree.
#[test]
fn a_leaf_write_keeps_its_siblings_and_a_leaf_reads_the_live_policy() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn(home.path(), "mare.setup { model = 'faux/echo' }\n", false);
    let whole = json!({ "mode": "granular", "categories": { "tool": true, "network": true } });
    assert!(set(&mut rpc, json!({ "plugins.approvals.policy": whole })).get("error").is_none());
    let leaf = set(&mut rpc, json!({ "plugins.approvals.policy.categories.tool": false }));
    assert!(leaf.get("error").is_none(), "{leaf}");
    assert_eq!(
        get(&mut rpc, "plugins.approvals.policy"),
        json!({ "mode": "granular", "categories": { "tool": false, "network": true } })
    );
    assert_eq!(get(&mut rpc, "plugins.approvals.policy.categories.network"), true);

    rpc.call_raw("commands.run", json!({ "line": "/approval never" }), |_| json!(null));
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "never");
    assert_eq!(get(&mut rpc, "plugins.approvals.policy.categories.network"), Value::Null, "not the stale config leaf");
    finish(rpc);
}

/// #7: a new session starts from the policy configured now, not the one at startup.
#[test]
fn a_new_session_starts_from_the_policy_written_at_runtime() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn(home.path(), "mare.setup { model = 'faux/echo' }\n", true);
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "ask");
    assert!(set(&mut rpc, json!({ "plugins.approvals.policy": "never" })).get("error").is_none());
    let new = rpc.call_raw("session.create", json!({}), |_| json!(null));
    assert!(new.get("error").is_none(), "{new}");
    assert_eq!(get(&mut rpc, "plugins.approvals.policy"), "never");
    finish(rpc);
}

/// the primary connection subscribed to every event sees no `option/apply` or
/// `option/current` frame and no credential: only an `option/changed` for the one leaf written.
/// Reference: reference implementation (an edit keeps
/// sibling secrets); Codex `codex-rs/app-server/tests/suite/v2/config_rpc.rs:791` (redaction on
/// what is read out).
#[test]
fn nothing_carries_a_credential_out_of_the_process() {
    let home = tempfile::tempdir().unwrap();
    let init = "mare.setup { model = 'faux/echo' }\n\
                mare.plugin('vault', { srv = { gh = { tier = 'a', env = { TOKEN = 's3cret' } } } })\n";
    let mut rpc = spawn(home.path(), init, false);
    let sid = rpc.session_id();
    let sub = rpc.call_raw("events.subscribe", json!({ "scope": "all", "include": ["*"] }), |_| json!(null));
    assert!(sub.get("error").is_none(), "{sub}");
    let id = rpc.request("options.set", json!({ "session_id": sid, "values": { "plugins.vault.srv.gh.tier": "b" } }));
    let mut frames: Vec<Value> = vec![];
    let r = loop {
        let m = rpc.read();
        if m.get("method").is_some() {
            frames.push(m);
            continue;
        }
        if m["id"] == id {
            break m;
        }
    };
    assert!(r.get("error").is_none(), "{r}");
    // events travel through the tap's queue and may trail the response: a follow-up call is the
    // sync point
    let id = rpc.request("options.get", json!({ "session_id": sid, "keys": ["plugins.vault.srv.gh.tier"] }));
    loop {
        let m = rpc.read();
        if m.get("method").is_some() {
            frames.push(m);
            continue;
        }
        if m["id"] == id {
            break;
        }
    }
    let out = serde_json::to_string(&frames).unwrap();
    assert!(!out.contains("s3cret"), "{out}");
    assert!(!out.contains("option/apply") && !out.contains("option/current"), "{out}");
    let changed: Vec<&Value> = frames.iter().filter(|f| f["params"]["type"] == "option/changed").collect();
    assert_eq!(changed.len(), 1, "{out}");
    assert_eq!(changed[0]["params"]["data"]["name"], "plugins.vault.srv.gh.tier", "{out}");
    finish(rpc);
}

/// the full `config.describe` lists a plugin option declared sensitive with its
/// default redacted and flagged. Reference: reference implementation
/// `packages/api/settings-controller/tests/settings-controller.host.spec.ts:24`.
#[test]
fn config_describe_redacts_a_declared_sensitive_default() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn(home.path(), "mare.setup { model = 'faux/echo' }\n", false);
    let all = rpc.call_raw("config.describe", json!({}), |_| json!(null));
    let entry = &all["result"]["options"]["plugins.vault.classified"];
    assert_eq!((entry["sensitive"].clone(), entry["redacted"].clone()), (json!(true), json!(true)), "{entry}");
    assert!(!all.to_string().contains("classified-value"), "{entry}");
    finish(rpc);
}

/// an owner's refusal that quotes a key-shaped value reaches the RPC caller redacted.
#[test]
fn an_owner_error_is_redacted_for_the_caller() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc = spawn(home.path(), "mare.setup { model = 'faux/echo' }\n", false);
    let r = set(&mut rpc, json!({ "plugins.zeta.mode": "leak" }));
    let msg = r["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(msg.contains("zeta refuses"), "{r}");
    assert!(!msg.contains("sk-proj-0123456789abcdef"), "{msg}");
    finish(rpc);
}

/// lazy#46 #1: a map key containing dots (`srv["api.example.com"]`) keeps the `sensitive` field
/// below it redacted for a real RPC reader (`options.get`) and on the event stream.
#[test]
fn a_dotted_map_key_keeps_its_sensitive_field_redacted_over_rpc() {
    let home = tempfile::tempdir().unwrap();
    let init = "mare.setup { model = 'faux/echo' }\n";
    let mut rpc = spawn(home.path(), init, false);
    let sid = rpc.session_id();
    rpc.call_raw("events.subscribe", json!({ "scope": "all", "include": ["*"] }), |_| json!(null));
    let id = rpc.request(
        "options.set",
        json!({ "session_id": sid, "values": { "plugins.vault.srv": { "api.example.com": { "pin": "5678" } } } }),
    );
    let mut frames: Vec<Value> = vec![];
    loop {
        let m = rpc.read();
        if m.get("method").is_some() {
            frames.push(m);
            continue;
        }
        if m["id"] == id {
            assert!(m.get("error").is_none(), "{m}");
            break;
        }
    }
    let got = rpc.call_raw("options.get", json!({ "session_id": sid, "keys": ["plugins.vault.srv"] }), |_| json!(null));
    let out = serde_json::to_string(&frames).unwrap() + &got.to_string();
    assert!(!out.contains("5678"), "{out}");
    assert!(got.to_string().contains("[REDACTED]"), "{got}");
    finish(rpc);
}

/// b52 CFG: `options.set` with the root key `plugins` is refused — it replaced the whole committed
/// plugins table and skipped every owner's `option/apply`; plugin options go through
/// `plugins.<plugin>.<option>` (the write queue). The existing value stays.
#[test]
fn a_root_plugins_write_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let mut rpc =
        spawn(home.path(), "mare.setup { model = 'faux/echo' }\nmare.plugin('alpha', { extra = 'kept' })\n", false);
    let r = set(&mut rpc, json!({ "plugins": { "alpha": { "extra": "replaced" } } }));
    assert!(r["error"]["message"].as_str().unwrap_or_default().contains("plugins.<plugin>.<option>"), "{r}");
    assert_eq!(get(&mut rpc, "plugins.alpha.extra"), "kept");
    finish(rpc);
}
