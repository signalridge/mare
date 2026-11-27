use super::policy_eval;
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn policy_reasons_retain_each_ask_proposal_in_either_order() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (first, second): (String, String) = eval(
            &h,
            r#"
            -- tool/before folds several asks into one verdict (the permissions plugin's ask meets
            -- other voters' asks there); each case's voters are disposed before the next
            local policy = C.cordis:get("policy")
            local function kinds(first, second)
              local offs = {}
              for _, kind in ipairs({ first, second }) do
                offs[#offs + 1] = policy:voter("tool/before", function() return { decision = "ask", reason = "ask",
                  amendment = { kind = kind } } end)
              end
              local v = policy:decide("tool/before", { {} })
              for _, off in ipairs(offs) do off() end
              return v.amendment.kind .. ":" .. v.reasons[1].amendment.kind .. ":" .. v.reasons[2].amendment.kind
            end
            return kinds("marker", "other"), kinds("other", "marker")"#,
        )
        .await;
        assert_eq!(first, "marker:marker:other");
        assert_eq!(second, "other:other:marker");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn policy_hook_timeout_denies_the_tool() {
    local(async {
        let env = TestEnv::new().await;
        let marker = env.cwd.join("ran");
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": create_file_cmd(&marker) } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        exec(&h, r#"C.cordis:get("policy"):voter("tool/before", function() mare.task.sleep(600000) end, { timeout_ms = 100 })"#).await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(!marker.exists(), "tool ran although its policy hook timed out");
        assert!(res.to_string().contains("timed out"), "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn policy_hook_error_or_invalid_vote_denies() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (on_error, reason, on_invalid): (String, String, String) = eval(
            &h,
            r#"local off = C.cordis:get("policy"):voter("tool/before", function() error("boom") end)
               local v1 = C.cordis:get("policy"):decide("tool/before", { { name = "x" } })
               off()
               C.cordis:get("policy"):voter("tool/before", function() return 42 end)
               return v1.decision, v1.reason, C.cordis:get("policy"):decide("tool/before", { { name = "x" } }).decision"#,
        )
        .await;
        assert_eq!((on_error.as_str(), on_invalid.as_str()), ("deny", "deny"));
        // the reason reaches the model and the transcript: the error's message, not its traceback
        assert!(reason.contains("boom"), "{reason}");
        assert!(!reason.contains("stack traceback") && !reason.contains('\n'), "{reason}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn approval_decisions_fold_into_the_three_way_vote() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (d1, g1, a1, d2, ab2): (String, String, String, String, bool) = eval(
            &h,
            r#"local off = C.cordis:get("policy"):voter("tool/approval", function() return { decision = "allow", grant = "allow_amend", amendment = { rule = "git push" } } end)
               local v1 = C.cordis:get("policy"):decide("tool/approval", { { name = "x" } })
               off()
               C.cordis:get("policy"):voter("tool/approval", function() return { decision = "deny", abort = true } end)
               local v2 = C.cordis:get("policy"):decide("tool/approval", { { name = "x" } })
               return v1.decision, v1.grant, v1.amendment.rule, v2.decision, v2.abort == true"#,
        )
        .await;
        assert_eq!((d1.as_str(), g1.as_str(), a1.as_str()), ("allow", "allow_amend", "git push"));
        assert_eq!((d2.as_str(), ab2), ("deny", true));
        h.shutdown().await;
    })
    .await
}

// Also the Windows regression test for a persistent shell that outlived shutdown: its open output
// pipes kept a blocking read on the runtime's blocking pool, and the test process never exited
// (see tools/shell_lifecycle.rs and trees.rs `Tree`).
#[tokio::test]
async fn approval_abort_ends_the_turn_and_allow_session_runs_the_tool() {
    local(async {
        let env = TestEnv::new().await;
        let (one, two) = (env.cwd.join("one"), env.cwd.join("two"));
        let script = json!([
            { "tool_calls": [
                { "name": "bash", "arguments": { "command": create_file_cmd(&one) } },
                { "name": "bash", "arguments": { "command": create_file_cmd(&two) } }
            ] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"RESULTS = {}
               C.cordis:get("policy"):voter("tool/before", function() return "ask" end)
               C.cordis:get("policy"):voter("tool/approval", function(call)
                   if call.arguments.command:find("one") then return { decision = "allow", grant = "allow_session" } end
                   return { decision = "deny", abort = true }
               end)
               C.cordis:on("approval/resolved", function(e) RESULTS[#RESULTS + 1] = e.result.decision end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert!(one.exists(), "allow_session must run the call: {res}");
        assert!(!two.exists(), "abort must not run the call");
        assert_eq!(res["error"]["code"], "aborted", "abort ends the turn: {res}");
        let results: Vec<String> = eval(&h, "return RESULTS").await;
        assert_eq!(results, ["allow_session", "abort"]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unknown_host_asks_net_approval_once_per_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        exec(
            &h,
            // net/approval is the policy vote (re-run as a veto even on a cached grant);
            // net/approval-host is who answers, and that answer is cached for the session.
            r#"ASKED, OUTCOMES = {}, {}
               C.cordis:get("policy"):voter("net/approval-host", function(req) ASKED[#ASKED + 1] = req.host .. ":" .. req.port; return { decision = "allow", grant = "allow_session" } end)
               C.cordis:on("approval/resolved", function(e) if e.category == "network" then OUTCOMES[#OUTCOMES + 1] = e.result.decision end end)
               -- a plain observer of net/approval is not a voter: the host is still asked (a-rnh4 #3)
               C.cordis:on("net/approval", function() end)"#,
        )
        .await;
        let proxy = mare_binding::netproxy::ProxyHandle::start_in_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            mare_binding::netproxy::approver(h.lua()),
        )
        .unwrap();
        for _ in 0..2 {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
            s.write_all(b"CONNECT approve-me.invalid:443 HTTP/1.1\r\nHost: approve-me.invalid:443\r\n\r\n").await.unwrap();
            let mut buf = [0u8; 256];
            let _ = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s.read(&mut buf)).await;
        }
        let (asked, outcomes): (Vec<String>, Vec<String>) = eval(&h, "return ASKED, OUTCOMES").await;
        assert_eq!(asked, ["approve-me.invalid:443"], "asked once, then cached for the session");
        assert_eq!(outcomes, ["allow_session"]);
        // a deny-listed / local host is never offered for approval
        assert!(proxy.denied().iter().all(|(h, _)| h != "localhost"));
        h.shutdown().await;
    })
    .await
}

/// Network approvals publish `approval/requested` before waiting and `approval/resolved` after
/// deciding, with `category = "network"` and the same `approval_id`.
#[tokio::test]
async fn net_approval_emits_requested_and_resolved_with_same_id() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        exec(
            &h,
            r#"ORDER, REQUESTED, RESOLVED = {}, {}, {}
               C.cordis:on("approval/requested", function(ev)
                   if ev.category == "network" then
                       ORDER[#ORDER + 1] = "requested"
                       REQUESTED[#REQUESTED + 1] = ev
                   end
               end)
               C.cordis:on("approval/resolved", function(ev)
                   if ev.category == "network" then
                       ORDER[#ORDER + 1] = "resolved"
                       RESOLVED[#RESOLVED + 1] = ev
                   end
               end)
               C.cordis:get("policy"):voter("net/approval-host", function()
                   ORDER[#ORDER + 1] = "voter"
                   return { decision = "allow", grant = "allow_session" }
               end)"#,
        )
        .await;
        let proxy = mare_binding::netproxy::ProxyHandle::start_in_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            mare_binding::netproxy::approver(h.lua()),
        )
        .unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(b"CONNECT net-pair.invalid:443 HTTP/1.1\r\nHost: net-pair.invalid:443\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 256];
        let _ = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s.read(&mut buf)).await;
        let raw: String = eval(&h, "return mare.json.encode({ order = ORDER, req = REQUESTED, res = RESOLVED })").await;
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        h.shutdown().await;
        assert_eq!(v["order"], json!(["requested", "voter", "resolved"]), "{v}");
        let req = &v["req"][0];
        let res = &v["res"][0];
        assert_eq!(req["approval_id"], res["approval_id"], "{v}");
        assert!(req["approval_id"].as_str().is_some_and(|s| !s.is_empty()), "{v}");
        assert_eq!(res["category"], "network", "{v}");
        assert_eq!(res["result"]["decision"], "allow_session", "{v}");
    })
    .await
}

#[tokio::test]
async fn net_approval_is_isolated_per_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let env = TestEnv::new().await;
        let h1 = env.host().no_session().build().await;
        let h2 = env.host().no_session().build().await;
        expose(&h1);
        expose(&h2);
        exec(
            &h1,
            r#"ASKED = {}
               C.cordis:get("policy"):voter("net/approval-host", function(req)
                   ASKED[#ASKED + 1] = req.host .. ":" .. req.port
                   return { decision = "allow", grant = "allow_session" }
               end)"#,
        )
        .await;
        exec(
            &h2,
            r#"ASKED = {}
               C.cordis:get("policy"):voter("net/approval-host", function(req)
                   ASKED[#ASKED + 1] = req.host .. ":" .. req.port
                   return { decision = "deny" }
               end)"#,
        )
        .await;

        let proxy1 = mare_binding::netproxy::ProxyHandle::start_in_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            mare_binding::netproxy::approver(h1.lua()),
        )
        .unwrap();
        let proxy2 = mare_binding::netproxy::ProxyHandle::start_in_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            mare_binding::netproxy::approver(h2.lua()),
        )
        .unwrap();

        let mut s1 = tokio::net::TcpStream::connect(("127.0.0.1", proxy1.port())).await.unwrap();
        s1.write_all(b"CONNECT s1-only.invalid:443 HTTP/1.1\r\nHost: s1-only.invalid:443\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 256];
        let _ = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s1.read(&mut buf)).await;

        let asked1: Vec<String> = eval(&h1, "return ASKED").await;
        let asked2: Vec<String> = eval(&h2, "return ASKED").await;
        assert_eq!(asked1, ["s1-only.invalid:443"]);
        assert!(asked2.is_empty(), "session 2 must not receive session 1's net approval: {asked2:?}");

        // Shutting down session 2 must not break session 1's network approver.
        h2.shutdown().await;
        let mut s1b = tokio::net::TcpStream::connect(("127.0.0.1", proxy1.port())).await.unwrap();
        s1b.write_all(b"CONNECT s1-after.invalid:443 HTTP/1.1\r\nHost: s1-after.invalid:443\r\n\r\n").await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s1b.read(&mut buf)).await;
        let asked1_after: Vec<String> = eval(&h1, "return ASKED").await;
        assert_eq!(asked1_after, ["s1-only.invalid:443", "s1-after.invalid:443"]);

        let _ = proxy2;
        h1.shutdown().await;
    })
    .await
}

/// Run a Lua snippet with a fresh host and return its results (policy rule tests).

#[tokio::test]
async fn policy_stops_at_the_first_deny() {
    local(async {
        let (d, later_ran): (String, bool) = policy_eval(
            r#"LATER = false
               C.cordis:get("policy"):voter("tool/before", function() return "deny" end)
               C.cordis:get("policy"):voter("tool/before", function() LATER = true return "allow" end)
               return C.cordis:get("policy"):decide("tool/before", { { name = "x" } }).decision, LATER"#,
        )
        .await;
        assert_eq!(d, "deny");
        assert!(!later_ran, "listeners after a deny don't run (and can't override it)");
    })
    .await
}

#[tokio::test]
async fn unknown_decisions_and_extra_fields_are_invalid_votes() {
    local(async {
        let (unknown, extra, legacy): (String, String, String) = policy_eval(
            r#"local off = C.cordis:get("policy"):voter("tool/before", function() return { decision = "maybe" } end)
               local a = C.cordis:get("policy"):decide("tool/before", { { name = "x" } }).decision
               off()
               off = C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow", note = "hi" } end)
               local b = C.cordis:get("policy"):decide("tool/before", { { name = "x" } }).decision
               off()
               -- approval vocabulary goes in `grant` / `abort`, never in `decision`
               C.cordis:get("policy"):voter("tool/before", function() return "allow_session" end)
               return a, b, C.cordis:get("policy"):decide("tool/before", { { name = "x" } }).decision"#,
        )
        .await;
        assert_eq!((unknown.as_str(), extra.as_str(), legacy.as_str()), ("deny", "deny", "deny"));
    })
    .await
}

/// Host with an `echo` tool and the services exposed; returns (env, host).

#[tokio::test]
async fn a_stateful_policy_slot_has_one_decider() {
    local(async {
        let (d, second_ran): (String, bool) = policy_eval(
            r#"SECOND = false
               C.cordis:get("policy"):voter("tool/before", function() return "allow" end, { slot = "fs-observation" })
               C.cordis:get("policy"):voter("tool/before", function() SECOND = true return "deny" end, { slot = "fs-observation" })
               return C.cordis:get("policy"):decide("tool/before", { { name = "x" } }).decision, SECOND"#,
        )
        .await;
        assert_eq!(d, "allow", "the first registered listener of the slot decides");
        assert!(!second_ran);
    })
    .await
}

#[tokio::test]
async fn nobody_to_answer_is_unavailable_and_recorded() {
    local(async {
        let env = TestEnv::new().await;
        let marker = env.cwd.join("ran");
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": create_file_cmd(&marker) } }] },
            { "text": "done" }
        ]);
        // headless: no UI and no approval listener can answer the `ask`
        let h = env.host().faux(script).build().await;
        expose(&h);
        exec(
            &h,
            r#"RESULTS = {}
               C.cordis:get("policy"):voter("tool/before", function() return "ask" end)
               C.cordis:on("approval/resolved", function(e) RESULTS[#RESULTS + 1] = e.result.decision end)"#,
        )
        .await;
        h.prompt("go", json!({})).await.unwrap();
        assert!(!marker.exists(), "unavailable counts as deny");
        let results: Vec<String> = eval(&h, "return RESULTS").await;
        assert_eq!(results, ["unavailable"]);
        let recorded: i64 = eval(&h, r#"return #S:list_records("approval/unavailable").data"#).await;
        assert_eq!(recorded, 1, "what was asked and the outcome are recorded in the session");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn dropping_proxy_cancels_an_unanswered_network_approval() {
    use tokio::io::AsyncWriteExt;
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        exec(&h, r#"ASKED, OUTCOMES = false, {}
          C.cordis:get("policy"):voter("net/approval", function() ASKED = true; mare.task.sleep(60000); return { decision = "allow", grant = "allow_session" } end, { timeout_ms = 0 })
          C.cordis:on("approval/resolved", function(e) if e.category == "network" then OUTCOMES[#OUTCOMES + 1] = e.result.decision end end)"#).await;
        let proxy = mare_binding::netproxy::ProxyHandle::start_in_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            mare_binding::netproxy::approver(h.lua()),
        )
        .unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(b"CONNECT cancel-me.invalid:443 HTTP/1.1\r\nHost: cancel-me.invalid:443\r\n\r\n").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !eval::<bool>(&h, "return ASKED").await { tokio::task::yield_now().await; }
        }).await.expect("approval never began");
        drop(proxy);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while eval::<i64>(&h, "return #OUTCOMES").await == 0 { tokio::task::yield_now().await; }
        }).await.expect("network approval was left pending");
        let outcomes: Vec<String> = eval(&h, "return OUTCOMES").await;
        assert_eq!(outcomes, ["deny"]);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn cancellation_leaves_the_approval_queue_without_waiting_for_others() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (finished, denied, ran): (bool, bool, i64) = eval(
            &h,
            r#"
            local entered, ran = false, 0
            local release, wait = mare.task.channel()
            C.cordis:get("policy"):voter("tool/before", function() return "ask" end)
            C.cordis:get("policy"):voter("tool/approval", function()
                entered = true
                wait:recv()
                return { decision = "deny" }
            end, { timeout_ms = 0 })
            T:register("approval_queue_probe", { description = "probe", parameters = { type = "object" },
                execute = function() ran = ran + 1; return "ok" end })
            local first = mare.task.spawn(function()
                return T:execute({ id = "one", name = "approval_queue_probe", arguments = {} }, {})
            end)
            while not entered do mare.task.sleep(5) end
            local token = mare.task.token()
            local second = mare.task.spawn(function()
                return T:execute({ id = "two", name = "approval_queue_probe", arguments = {} }, { cancel = token })
            end)
            mare.task.sleep(20)
            token:cancel()
            local finished, result = mare.task.timeout(500, function() return second:wait() end)
            release:send(true)
            first:wait()
            return finished, finished and result.is_error == true, ran
        "#,
        )
        .await;
        assert!(finished && denied, "queued cancellation must complete without waiting for the first approval");
        assert_eq!(ran, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn net_approval_needs_an_affirmative_vote_instead_of_an_observer() {
    local(async {
        let (empty, observer, approved, denied, ordinary, custom_empty, custom_observer, custom_allow): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = policy_eval(
            r#"local empty = C.cordis:get("policy"):decide("net/approval", { {} }, { require_explicit_approval = true }).decision
               local off = C.cordis:get("policy"):voter("net/approval", function() return nil end)
               local observer = C.cordis:get("policy"):decide("net/approval", { {} }, { require_explicit_approval = true }).decision
               local allow = C.cordis:get("policy"):voter("net/approval", function() return "allow" end)
               local approved = C.cordis:get("policy"):decide("net/approval", { {} }, { require_explicit_approval = true }).decision
               C.cordis:get("policy"):voter("net/approval", function() return "deny" end)
               local denied = C.cordis:get("policy"):decide("net/approval", { {} }, { require_explicit_approval = true }).decision
               C.cordis:get("policy"):voter("tool/before", function() return nil end)
               -- the producer's option on a point whose fold is not affirmative: tool/approval-host,
               -- which the product decides with require_explicit_approval (approvals/child.rs)
               local custom_empty = C.cordis:get("policy"):decide("tool/approval-host", { {} }, { require_explicit_approval = true }).decision
               C.cordis:get("policy"):voter("tool/approval-host", function() return nil end)
               local custom_observer = C.cordis:get("policy"):decide("tool/approval-host", { {} }, { require_explicit_approval = true }).decision
               C.cordis:get("policy"):voter("tool/approval-host", function() return "allow" end)
               local custom_allow = C.cordis:get("policy"):decide("tool/approval-host", { {} }, { require_explicit_approval = true }).decision
               return empty, observer, approved, denied, C.cordis:get("policy"):decide("tool/before", { {} }).decision, custom_empty, custom_observer, custom_allow"#,
        )
        .await;
        assert_eq!(
            (
                empty.as_str(),
                observer.as_str(),
                approved.as_str(),
                denied.as_str(),
                ordinary.as_str(),
                custom_empty.as_str(),
                custom_observer.as_str(),
                custom_allow.as_str(),
            ),
            ("deny", "deny", "allow", "deny", "allow", "deny", "deny", "allow")
        );
    })
    .await
}

#[tokio::test]
async fn revoked_shadow_tool_is_blocked_before_entering_approval() {
    local(async {
        let (pre_content, pre_refused, before_content, before_refused, asked, ran): (
            String,
            bool,
            String,
            bool,
            i64,
            i64,
        ) = policy_eval(
            r#"
            RAN, ASKED = 0, 0
            local base = T:register("shadow_pre_revoke", { description = "base", parameters = { type = "object" }, execute = function() RAN = RAN + 1 end })
            local copy = T:get("shadow_pre_revoke")
            copy.description = "wrapped"
            T:shadow("shadow_pre_revoke", copy)
            local bound = T:bind_declarations(T:definitions({}, { "shadow_pre_revoke" }))
            C.cordis:get("policy"):voter("tool/approval", function() ASKED = ASKED + 1 return { decision = "allow" } end)
            local approve_cb = function() ASKED = ASKED + 1 return { decision = "allow" } end
            -- Case 1: revoked during tool/before after returning "ask"
            local off_before = C.cordis:get("policy"):voter("tool/before", function(call)
                if call.name == "shadow_pre_revoke" then
                    base()
                    return "ask"
                end
            end)
            local r_before = T:execute({ id = "b", name = "shadow_pre_revoke", arguments = {} }, {
                declarations = bound,
                approve = approve_cb,
            })
            off_before()
            -- Case 2: revoked and replaced before T:execute is even called
            T:register("shadow_pre_revoke", { description = "replacement", parameters = { type = "object" }, execute = function() RAN = RAN + 10 end })
            local r_pre = T:execute({ id = "p", name = "shadow_pre_revoke", arguments = {} }, {
                declarations = bound,
                approve = approve_cb,
            })
            return r_pre.content, r_pre.refused == true, r_before.content, r_before.refused == true, ASKED, RAN
        "#,
        )
        .await;
        assert!(pre_content.contains("blocked"), "{pre_content}");
        assert!(before_content.contains("blocked"), "{before_content}");
        assert!(pre_refused && before_refused);
        assert_eq!(asked, 0, "no approval request may be emitted for a revoked shadow tool");
        assert_eq!(ran, 0);
    })
    .await
}

#[tokio::test]
async fn policy_block_vote_is_rejected_and_deny_omits_block_alias() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (
            str_dec,
            str_reason,
            str_block_nil,
            tbl_dec,
            tbl_reason,
            tbl_block_nil,
            deny_block_nil,
            bare_deny_block_nil,
        ): (String, String, bool, String, String, bool, bool, bool) = eval(
            &h,
            r#"
            local policy = C.cordis:get("policy")
            local off1 = policy:voter("tool/before", function() return "block" end)
            local v1 = policy:decide("tool/before", { { name = "x" } })
            off1()

            local off2 = policy:voter("tool/before", function() return { decision = "block", reason = "legacy" } end)
            local v2 = policy:decide("tool/before", { { name = "x" } })
            off2()

            local off3 = policy:voter("tool/before", function() return { decision = "deny", reason = "nope" } end)
            local v3 = policy:decide("tool/before", { { name = "x" } })
            off3()

            local off4 = policy:voter("tool/before", function() return "deny" end)
            local v4 = policy:decide("tool/before", { { name = "x" } })
            off4()

            return v1.decision, tostring(v1.reason), v1.block == nil,
                   v2.decision, tostring(v2.reason), v2.block == nil,
                   v3.block == nil, v4.block == nil
            "#,
        )
        .await;
        assert_eq!(str_dec, "deny");
        assert!(str_reason.contains("unknown decision `block`"), "{str_reason}");
        assert!(str_block_nil, "deny verdict must not expose legacy `block` alias");
        assert_eq!(tbl_dec, "deny");
        assert!(tbl_reason.contains("unknown decision `block`"), "{tbl_reason}");
        assert!(tbl_block_nil, "invalid-vote deny must not expose legacy `block` alias");
        assert!(deny_block_nil, "explicit deny with reason must not expose legacy `block` alias");
        assert!(bare_deny_block_nil, "bare deny must not expose legacy `block` alias");
        h.shutdown().await;
    })
    .await
}
