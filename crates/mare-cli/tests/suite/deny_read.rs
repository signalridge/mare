//! Denied reads survive escalation (Codex `core/src/tools/sandboxing.rs:247-252`,
//! `:271-283`): when the session restricts reads, an approved `all` escalation and a rule that
//! allows one stay sandboxed with those reads denied — for the main agent and for a sub-agent
//! (approved by the parent, or allowed by a shared rule). Without a read restriction they leave
//! the sandbox as before. A backend that can't enforce `fs_read` refuses the call instead.
//! Seatbelt, bubblewrap (masked root, `mare_binding::read_mask`) and Landlock enforce it once
//! probed (DENYREAD2), so on Linux the kept call runs; each refuses a denied read its own way.
#![cfg(unix)]

use crate::common::*;
use serde_json::{Value, json};

/// A secret outside every root the sandbox lets a process read (not under the workspace nor a
/// temp area), and a file outside every writable root; both removed on drop.
struct Outside {
    secret: std::path::PathBuf,
    out: std::path::PathBuf,
}

const TOKEN: &str = "deny-read-token-7f3a";

impl Outside {
    fn new(tag: &str) -> Self {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let base = format!(".mare-denyread-{}-{tag}", std::process::id());
        let o = Self { secret: home.join(format!("{base}-secret")), out: home.join(format!("{base}-out")) };
        std::fs::write(&o.secret, TOKEN).unwrap();
        let _ = std::fs::remove_file(&o.out);
        o
    }
    /// read the secret, then write outside the workspace: both need the sandbox gone
    fn command(&self) -> String {
        format!("cat '{}'; touch '{}'", self.secret.display(), self.out.display())
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.secret);
        let _ = std::fs::remove_file(&self.out);
    }
}

/// `{argv = {"cat"}}` and `{argv = {"touch"}}` allowed with `scope = "all"` (no sandbox)
fn bypass_rules(env: &TestEnv) {
    std::fs::create_dir_all(env.home.path().join("state")).unwrap();
    std::fs::write(
        env.home.path().join("state/permissions.json"),
        json!({ "version": 1, "rules": [
            { "argv": ["cat"], "decision": "allow", "scope": "all" },
            { "argv": ["touch"], "decision": "allow", "scope": "all" },
        ] })
        .to_string(),
    )
    .unwrap();
}

async fn main_host(env: &TestEnv, script: Value) -> std::sync::Arc<mare_host::Host> {
    let mut b = env
        .host()
        .faux(script)
        .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
        .args(json!({ "no_session": true }));
    b.opts.project_trusted = true;
    let h = b.build().await;
    expose(&h);
    global(&h, "CWD", env.cwd.to_string_lossy().to_string());
    global(&h, "P", h.service_table("permissions").unwrap());
    h
}

/// The session restricts reads (reads: the workspace, temp areas and system roots).
const RESTRICT: &str =
    r#"mare.proc.sandbox.set_current({ level = "workspace-write", restrict_read = true, network = false })"#;

/// Run `cmd` through the main agent's tool pipeline as an `all` escalation that the user
/// approves (`ASKS` counts the dialogs): the tool result text.
async fn escalate(h: &mare_host::Host, cmd: &str, restrict: bool) -> (String, i64) {
    global(h, "CMD", cmd.to_string());
    if restrict {
        exec(h, RESTRICT).await;
    }
    eval(
        h,
        r#"local asks = 0
           local r = T:execute({ id = "e" .. math.random(1e9), name = "bash", arguments = {
               command = CMD, sandbox_permissions = "all", justification = "needs the file" } }, {
             approve = function() asks = asks + 1 return { decision = "allow", source = "user" } end,
           })
           local t = type(r.content) == "string" and r.content or (r.content[1] or {}).text
           return tostring(t), asks"#,
    )
    .await
}

/// How `mech` refuses a denied read inside the sandbox: Seatbelt `EPERM`, Landlock `EACCES`,
/// bubblewrap hides the path (`ENOENT`, mare_binding::read_mask).
fn read_refused(mech: &str, content: &str) -> bool {
    match mech {
        "bwrap" => content.contains("No such file or directory"),
        "landlock" => content.contains("Permission denied"),
        _ => content.contains("Operation not permitted"),
    }
}

/// What a backend that runs the kept policy shows, or the refusal of one that can't.
fn assert_kept(o: &Outside, content: &str, mech: &str, fs_read: bool) {
    assert!(!content.contains(TOKEN), "{mech}: the denied read leaked: {content}");
    assert!(!o.out.exists(), "{mech}: the escalation left the sandbox: {content}");
    if fs_read {
        // it ran, inside the sandbox: the read was refused, not the call
        assert!(read_refused(mech, content) && !content.contains("SANDBOX_"), "{mech}: {content}");
    } else {
        assert!(content.contains("SANDBOX_INSUFFICIENT") && content.contains("stays sandboxed"), "{content}");
    }
}

async fn backend(h: &mare_host::Host) -> (String, bool) {
    eval(h, "local c = mare.proc.sandbox.capabilities() return c.mechanism, c.restrict_read").await
}

#[tokio::test]
async fn an_approved_escalation_keeps_the_denied_reads() {
    for restrict in [true, false] {
        local(async {
            let env = TestEnv::new().await;
            let o = Outside::new("approved");
            let h = main_host(&env, json!([])).await;
            let (mech, fs_read) = backend(&h).await;
            assert_ne!(mech, "none", "sandbox backend required on unix");
            let (content, asks) = escalate(&h, &o.command(), restrict).await;
            assert_eq!(asks, 1, "{content}");
            if restrict {
                assert_kept(&o, &content, &mech, fs_read);
            } else {
                assert!(content.contains(TOKEN), "no denied reads: unsandboxed as approved: {content}");
                assert!(o.out.exists(), "{content}");
            }
            h.shutdown().await;
        })
        .await
    }
}

#[tokio::test]
async fn a_rule_allowed_bypass_keeps_the_denied_reads() {
    for restrict in [true, false] {
        local(async {
            let env = TestEnv::new().await;
            bypass_rules(&env);
            let o = Outside::new("rule");
            let h = main_host(&env, json!([])).await;
            let (mech, fs_read) = backend(&h).await;
            assert_ne!(mech, "none", "sandbox backend required on unix");
            let (content, asks) = escalate(&h, &o.command(), restrict).await;
            assert_eq!(asks, 0, "the rule decided: {content}");
            if restrict {
                assert_kept(&o, &content, &mech, fs_read);
            } else {
                assert!(content.contains(TOKEN) && o.out.exists(), "{content}");
            }
            h.shutdown().await;
        })
        .await
    }
}

/// An increment (`network`) keeps what it asked for and still not the reads.
#[tokio::test]
async fn an_approved_increment_keeps_the_denied_reads() {
    local(async {
        let env = TestEnv::new().await;
        let o = Outside::new("inc");
        let h = main_host(&env, json!([])).await;
        let (mech, fs_read) = backend(&h).await;
        assert_ne!(mech, "none", "sandbox backend required on unix");
        assert!(fs_read, "sandbox backend {mech} with fs_read capability required on unix");
        global(&h, "CMD", format!("cat '{}'", o.secret.display()));
        exec(&h, RESTRICT).await;
        let content: String = eval(
            &h,
            r#"local r = T:execute({ id = "n1", name = "bash", arguments = {
                   command = CMD, sandbox_permissions = "network", justification = "fetch" } }, {
                 approve = function() return { decision = "allow", source = "user" } end,
               })
               return tostring(type(r.content) == "string" and r.content or r.content[1].text)"#,
        )
        .await;
        assert!(!content.contains(TOKEN) && read_refused(&mech, &content), "{content}");
        h.shutdown().await;
    })
    .await
}

/// A readable directory outside the workspace and the temp areas (the session reads it, the
/// escalation may write it); removed on drop.
struct Shared(std::path::PathBuf);

impl Shared {
    fn new(tag: &str) -> Self {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let d = home.join(format!(".mare-denyread-{}-{tag}-shared", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("note"), "shared-note").unwrap();
        Self(dunce::canonicalize(&d).unwrap())
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `cmd` as an approved escalation `perm` under a session restricting reads to the
/// workspace, temp areas and `shared`: the tool result text.
async fn kept_call(h: &mare_host::Host, shared: &Shared, perm: &str, cmd: &str) -> String {
    global(h, "SHARED", shared.0.to_string_lossy().to_string());
    global(h, "PERM", perm.to_string());
    global(h, "CMD", cmd.to_string());
    exec(
        h,
        r#"mare.proc.sandbox.set_current({ level = "workspace-write", restrict_read = true, network = false,
             readable = { SHARED } })"#,
    )
    .await;
    eval(
        h,
        r#"local r = T:execute({ id = "k" .. math.random(1e9), name = "bash", arguments = {
               command = CMD, sandbox_permissions = PERM, justification = "needs it" } }, {
             approve = function() return { decision = "allow", source = "user" } end,
           })
           return tostring(type(r.content) == "string" and r.content or (r.content[1] or {}).text)"#,
    )
    .await
}

/// DENYREAD2: a kept escalation still does what it was approved for — an approved `write:` on a
/// readable directory writes there — while the denied read stays denied, and the readable and
/// writable roots stay readable. Real kernel enforcement on every backend with `fs_read`
/// (bubblewrap and Landlock in the Linux runs).
#[tokio::test]
async fn a_kept_escalation_still_does_what_was_approved() {
    local(async {
        let env = TestEnv::new().await;
        let o = Outside::new("approved-write");
        let shared = Shared::new("write");
        std::fs::write(env.cwd.join("ws-note"), "ws-note").unwrap();
        let h = main_host(&env, json!([])).await;
        let (mech, fs_read) = backend(&h).await;
        assert_ne!(mech, "none", "sandbox backend required on unix");
        assert!(fs_read, "sandbox backend {mech} with fs_read capability required on unix");
        let d = shared.0.display();
        let cmd = format!(
            "cat '{}'; cat '{d}/note'; cat ws-note; echo approved > '{d}/out' && cat '{d}/out'",
            o.secret.display()
        );
        let content = kept_call(&h, &shared, &format!("write:{d}"), &cmd).await;
        assert!(!content.contains(TOKEN) && read_refused(&mech, &content), "{mech}: {content}");
        assert!(content.contains("shared-note") && content.contains("ws-note"), "{mech}: roots readable: {content}");
        assert!(content.contains("approved"), "{mech}: the approved write: {content}");
        assert_eq!(std::fs::read_to_string(shared.0.join("out")).unwrap().trim(), "approved", "{mech}");
        h.shutdown().await;
    })
    .await
}

/// A root the escalation may write but the session doesn't read: Landlock and Seatbelt grant the
/// write alone; bubblewrap can't (a bind mount is readable) and refuses rather than exposing it.
#[tokio::test]
async fn a_write_outside_the_kept_reads_is_write_only_or_refused() {
    local(async {
        let env = TestEnv::new().await;
        let o = Outside::new("write-only");
        let shared = Shared::new("readable");
        let target = Shared::new("unread");
        std::fs::write(target.0.join("secret"), TOKEN).unwrap();
        let h = main_host(&env, json!([])).await;
        let (mech, fs_read) = backend(&h).await;
        assert_ne!(mech, "none", "sandbox backend required on unix");
        assert!(fs_read, "sandbox backend {mech} with fs_read capability required on unix");
        let t = target.0.display();
        let cmd = format!("cat '{t}/secret'; echo w > '{t}/out'; cat '{}'", o.secret.display());
        let content = kept_call(&h, &shared, &format!("write:{t}"), &cmd).await;
        assert!(!content.contains(TOKEN), "{mech}: {content}");
        if mech == "bwrap" {
            assert!(content.contains("SANDBOX_INSUFFICIENT") && content.contains("bind mount"), "{content}");
            assert!(!target.0.join("out").exists());
        } else {
            assert_eq!(std::fs::read_to_string(target.0.join("out")).unwrap().trim(), "w", "{mech}: {content}");
        }
        h.shutdown().await;
    })
    .await
}

/// A backend that can't enforce `fs_read` refuses the kept call with the reason, never runs it
/// with the reads dropped (simulated: `MARE_SANDBOX_CAPS` narrows the probed set).
#[tokio::test]
async fn a_backend_without_fs_read_refuses_the_kept_escalation() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,net_block,net_proxy,unix_socket");
        let o = Outside::new("refuse");
        let h = main_host(&env, json!([])).await;
        let (mech, fs_read) = backend(&h).await;
        assert_ne!(mech, "none", "sandbox backend required on unix");
        assert!(!fs_read);
        let (content, _) = escalate(&h, &o.command(), true).await;
        assert_kept(&o, &content, &mech, false);
        let line: String = eval(&h, "return mare.proc.sandbox.capabilities().escalation_reads").await;
        assert!(line.contains("refused") && line.contains("fs_read"), "{line}");
        h.shutdown().await;
    })
    .await
}

/// A sub-agent: its parent approves an `all` escalation, or a shared rule allows it; under a
/// parent policy that restricts reads the child's call stays sandboxed with them.
#[tokio::test]
async fn a_child_escalation_keeps_the_ceiling_denied_reads() {
    for (rule, restrict) in [(false, true), (true, true), (false, false), (true, false)] {
        local(async {
            let env = TestEnv::new().await;
            if rule {
                bypass_rules(&env);
            }
            let o = Outside::new("child");
            let script = json!([
                { "tool_calls": [{ "name": "bash", "arguments": { "command": o.command(),
                    "sandbox_permissions": "all", "justification": "x" } }] },
                { "text": "done" }
            ]);
            let h = main_host(&env, script).await;
            let (mech, fs_read) = backend(&h).await;
            assert_ne!(mech, "none", "sandbox backend required on unix");
            if restrict {
                exec(&h, RESTRICT).await;
            }
            let (asked, messages): (i64, String) = eval(
                &h,
                // tool/approval is a policy vote, consulted again by the post-answer checks:
                // count the calls it was asked about, not its invocations
                r#"local asked, seen = 0, {}
                   C.cordis:get("policy"):voter("tool/approval", function(call)
                       if not seen[call.id] then seen[call.id] = true; asked = asked + 1 end
                       return { decision = "allow" }
                   end)
                   local a = mare.subagent.start({ prompt = "go", cwd = CWD, no_session = true })
                   local ok, res = pcall(a.wait, a)
                   return asked, mare.json.encode(ok and res.messages or res)"#,
            )
            .await;
            assert_eq!(asked, if rule { 0 } else { 1 }, "{messages}");
            if restrict {
                assert_kept(&o, &messages, &mech, fs_read);
            } else {
                assert!(messages.contains(TOKEN) && o.out.exists(), "{messages}");
            }
            h.shutdown().await;
        })
        .await
    }
}
