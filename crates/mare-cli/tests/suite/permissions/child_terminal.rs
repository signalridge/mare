//! One terminal state per sub-agent approval: the parent decides between the answer and the
//! child's cancellation before any `approval/resolved` listener sees either, then publishes
//! that one state completely (local observers, rule persistence, public event).
//!
//! Reference: reference implementation (caller cancellation overtaking an
//! approval yields one cancelled outcome and no dispatch) and Kimi Code packages/kap-server/test/
//! sessionEventBroadcaster.test.ts:1523 (an approval ends with exactly one result event); Codex
//! core/tests/suite/approvals.rs:2633 (a sub-agent's approved rule reaches the parent's store).
use super::*;

/// Observers record every origin result: a pausing listener (ahead of or behind the built-in
/// permissions listener) and a late recorder. The public `approval/resolved` listener is the last thing the
/// parent publishes for an approval, so it bounds the wait for "all local results delivered".
const OBSERVE: &str = r#"
    SEEN = {}
    local function seen(who, ev)
        SEEN[#SEEN + 1] = { who = who, id = ev.child_approval_id, decision = ev.decision, source = ev.source }
    end
    ENTERED_TX, ENTERED_RX = mare.task.channel()
    RELEASE_TX, RELEASE_RX = mare.task.channel()
    PUBLIC_TX, PUBLIC_RX = mare.task.channel()
    C.cordis:on("pipeline/approval-decided", function(ev)
        if ev.origin then
            seen("pauser", ev)
            if PAUSE_RESULT and ev.source ~= "cancelled" then
                PAUSE_RESULT = false
                ENTERED_TX:send(true)
                RELEASE_RX:recv()
            end
        end
    end, { prepend = true })
    C.cordis:on("pipeline/approval-decided", function(ev)
        if ev.origin then seen("recorder", ev) end
    end)
    PUBLIC = {}
    C.cordis:on("approval/resolved", function(ev)
        local r = ev.result or {}
        PUBLIC[#PUBLIC + 1] = { id = r.child_approval_id, decision = r.decision, source = r.source }
        PUBLIC_TX:send(true)
    end)
    function SPAWN(file)
        local script = {
            { tool_calls = {{ name = "bash", arguments = { command = "touch " .. file, sandbox_permissions = "write:" .. mare.cwd, justification = "write file" } }} },
            { text = "done" }
        }
        return mare.subagent.start({
            prompt = "run",
            no_session = true,
            config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(script) } } }
        })
    end
    function CANCEL_AND_WAIT(child)
        child:cancel()
        local ok, err = pcall(function() return child:wait() end)
        assert(not ok and tostring(err):find("cancelled", 1, true), "expected cancelled child, got: " .. tostring(err))
    end
    -- hang guard only: the parent publishes promptly once released
    function AWAIT_PUBLIC()
        assert(mare.task.timeout(15000, function() PUBLIC_RX:recv() end), "no public approval/resolved")
    end
"#;

fn persisted_rules(env: &TestEnv) -> Vec<serde_json::Value> {
    let path = env.home.path().join("state/permissions.json");
    if !path.exists() {
        return vec![];
    }
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    json.get("rules").and_then(|r| r.as_array()).cloned().unwrap_or_default()
}

/// Every local observation (who, id, decision, source) and every public result (id, decision,
/// source).
async fn seen(h: &mare_host::Host) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let raw: String = eval(h, "return mare.json.encode({ SEEN, PUBLIC })").await;
    let both: Vec<Vec<serde_json::Value>> = serde_json::from_str(&raw).unwrap();
    let mut it = both.into_iter();
    (it.next().unwrap(), it.next().unwrap_or_default())
}

fn one_terminal((seen, public): &(Vec<serde_json::Value>, Vec<serde_json::Value>)) -> (String, String) {
    assert!(seen.iter().all(|s| s["id"].is_u64()), "every local result names its approval: {seen:?}");
    let ids: std::collections::BTreeSet<_> = seen.iter().map(|s| s["id"].to_string()).collect();
    assert_eq!(ids.len(), 1, "one approval expected: {seen:?}");
    // the public event names the same approval and carries the same terminal state
    assert_eq!(public.len(), 1, "one public result: {public:?}");
    assert_eq!(public[0]["id"], seen[0]["id"], "public and local results name one approval: {public:?} {seen:?}");
    assert_eq!(
        (&public[0]["decision"], &public[0]["source"]),
        (&seen[0]["decision"], &seen[0]["source"]),
        "public and local terminal states agree: {public:?} {seen:?}"
    );
    let states: std::collections::BTreeSet<_> = seen
        .iter()
        .map(|s| (s["decision"].as_str().unwrap().to_owned(), s["source"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(states.len(), 1, "one approval id must publish exactly one terminal state: {seen:?}");
    for who in ["pauser", "recorder"] {
        assert_eq!(seen.iter().filter(|s| s["who"] == who).count(), 1, "{who} sees it exactly once: {seen:?}");
    }
    states.into_iter().next().unwrap()
}

/// Cancellation that lands while the parent is still asking (before any result is published)
/// wins: observers see only `cancelled`, and the remembered rule is never handed out.
#[tokio::test]
async fn cancellation_before_publication_wins_with_one_cancelled_result_and_no_rule() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}}))
            .build()
            .await;
        expose(&h);
        exec(&h, OBSERVE).await;
        exec(
            &h,
            r#"
            local asked_tx, asked_rx = mare.task.channel()
            local answer_tx, answer_rx = mare.task.channel()
            C.cordis:get("policy"):voter("tool/approval-host", function(call, info, verdict)
                asked_tx:send(true)
                answer_rx:recv()
                return { decision = "allow", grant = "allow_amend", amendment = verdict and verdict.amendment or nil,
                    source = "user", human_approval = mare.crypto.random_hex(32) }
            end)
            local child = SPAWN("cancel-first.txt")
            asked_rx:recv()
            CANCEL_AND_WAIT(child)
            answer_tx:send(true)
            AWAIT_PUBLIC()
        "#,
        )
        .await;
        let seen = seen(&h).await;
        assert_eq!(one_terminal(&seen), ("deny".into(), "cancelled".into()));
        assert!(persisted_rules(&env).is_empty(), "a cancelled approval must not persist its rule");
        assert!(!env.cwd.join("cancel-first.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// Once the answer is being published, a later cancellation does not re-judge it: every local
/// observer — including one still waiting on I/O when the child is cancelled — sees the same
/// single result, and a remembered rule the user approved is saved.
async fn result_published_first_is_the_only_terminal(
    grant: &str,
    file: &str,
) -> (String, String, Vec<serde_json::Value>, serde_json::Value) {
    let env = TestEnv::new().await;
    let h = env
        .host()
        .no_session()
        .config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}}))
        .build()
        .await;
    expose(&h);
    exec(&h, OBSERVE).await;
    exec(&h, &format!(r#"
        PAUSE_RESULT = true
        SHOWN = mare.json.null
        C.cordis:get("policy"):voter("tool/approval-host", function(call, info, verdict)
            local answer = {{ decision = "allow", grant = "{grant}", source = "user", human_approval = mare.crypto.random_hex(32) }}
            if answer.grant == "allow_amend" then
                answer.amendment = verdict and verdict.amendment or nil
                SHOWN = answer.amendment
            end
            return answer
        end)
        local child = SPAWN("{file}")
        ENTERED_RX:recv()
        CANCEL_AND_WAIT(child)
        RELEASE_TX:send(true)
        AWAIT_PUBLIC()
    "#)).await;
    let seen = seen(&h).await;
    let (decision, source) = one_terminal(&seen);
    let shown: serde_json::Value =
        serde_json::from_str(&eval::<String>(&h, "return mare.json.encode(SHOWN)").await).unwrap();
    let rules = persisted_rules(&env);
    h.shutdown().await;
    (decision, source, rules, shown)
}

/// The rule as the dialog showed it, minus the child's display text (what "remember" stores).
fn normalized(rule: &serde_json::Value) -> serde_json::Value {
    let mut rule = rule.clone();
    if let Some(o) = rule.as_object_mut() {
        o.remove("description");
    }
    rule
}

#[tokio::test]
async fn result_published_before_cancellation_stays_the_only_terminal_and_saves_the_rule() {
    local(async {
        let (decision, _source, rules, shown) =
            result_published_first_is_the_only_terminal("allow_amend", "amend-first.txt").await;
        assert_eq!(decision, "allow_amend");
        assert!(shown.is_object(), "the dialog showed a rule: {shown}");
        // the saved rule is exactly the approved one (argv prefix, cwd, scope...), not a wider one
        assert_eq!(
            rules.iter().map(normalized).collect::<Vec<_>>(),
            vec![normalized(&shown)],
            "the approved rule is saved as shown even though the child was cancelled later"
        );

        let (decision, _source, rules, _) =
            result_published_first_is_the_only_terminal("allow", "allow-first.txt").await;
        assert_eq!(decision, "allow");
        assert!(rules.is_empty(), "a plain allow remembers nothing: {rules:?}");
    })
    .await
}
