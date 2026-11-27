//! Approval scopes: what an approval widens and what it covers again.

use super::*;

#[tokio::test]
async fn approving_a_write_scope_widens_only_file_writes() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        global(&h, "DIR", env.cwd.join("out").to_string_lossy().to_string());
        std::fs::create_dir(env.cwd.join("out")).unwrap();
        let (scoped, plain): (String, String) = eval(
            &h,
            r#"local function shape(v)
                   assert(v.decision == "ask", v.decision)
                   local p = mare.proc.sandbox.policy(v.sandbox_grant)
                   return p.level .. " net=" .. tostring(p.network) .. " roots=" .. #p.roots
               end
               return shape(VOTE(BASH("touch " .. DIR .. "/f", "write:" .. DIR, "write the report"))),
                      shape(VOTE({ id = "w1", name = "write", arguments = { path = "a.txt", content = "x" } },
                                 { side_effect = "write" }))"#,
        )
        .await;
        assert!(scoped.starts_with("workspace-write net=false"), "approving write:<path> opened more: {scoped}");
        assert!(plain.starts_with("workspace-write net=false"), "an approved write opened the network: {plain}");
        // a file the command is about to create: checked against (and granted as) its parent
        // directory; a missing parent is still refused
        let (new_file, missing): (String, String) = eval(
            &h,
            r#"local v = VOTE(BASH("echo x > " .. DIR .. "/new.txt", "write:" .. DIR .. "/new.txt", "create it"))
               local roots = {}
               for _, r in ipairs(mare.proc.sandbox.policy(v.sandbox_grant).roots) do roots[#roots + 1] = tostring(r.path or r) end
               local m = VOTE(BASH("echo x > " .. DIR .. "/nope/new.txt", "write:" .. DIR .. "/nope/new.txt", "create"))
               return v.decision .. " " .. table.concat(roots, ","), m.decision .. " " .. tostring(m.reason)"#,
        )
        .await;
        assert!(new_file.starts_with("ask ") && new_file.contains("out"), "{new_file}");
        assert!(!new_file.contains("new.txt"), "the grant is the parent directory: {new_file}");
        assert!(missing.contains("nor its parent directory exists"), "{missing}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn approved_scopes_are_reused_and_only_the_rest_is_asked() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("out/sub")).unwrap();
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        global(&h, "DIR", env.cwd.join("out").to_string_lossy().to_string());
        let (first, inside, net, narrowed_host, other_host, reused): (String, String, String, String, String, Vec<bool>) = eval(
            &h,
            r#"local c = BASH("touch " .. DIR .. "/f", "write:" .. DIR, "write the report")
               local v = VOTE(c)
               C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_session" })
               -- inside the session-approved scope: no question
               local iv = VOTE(BASH("touch " .. DIR .. "/sub/g", "write:" .. DIR .. "/sub", "more"))
               -- a different dimension is still asked, and only that part is shown
               local n = VOTE(BASH("curl a.example", "network:a.example", "fetch"))
               -- the approver grants the whole network: stored is only what was requested
               local c2 = BASH("curl a.example", "network:a.example", "fetch")
               VOTE(c2)
               C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c2, decision = "allow_session", permissions = { network = true } })
               local again = VOTE(BASH("curl a.example", "network:a.example", "again"))
               -- `request` is what a question would still ask for: nothing once the scope is approved
               return v.decision, iv.decision, n.reason, again.decision,
                   VOTE(BASH("curl c.example", "network:c.example", "other")).decision,
                   { iv.request == nil, again.request == nil }"#,
        )
        .await;
        // without network control read-only still asks before every shell command (vote.lua
        // net_note); the approved scope is reused all the same: nothing is left to request
        let reused_decision = if net_controlled(&h).await { "allow" } else { "ask" };
        assert_eq!(first, "ask");
        assert_eq!(inside, reused_decision, "a request inside an approved scope passes");
        assert!(net.contains("requests network:a.example") && !net.contains("write:"), "{net}");
        assert_eq!(narrowed_host, reused_decision, "network-partial still asks even for a reused host scope");
        assert_eq!(reused, [true, true], "the approved scopes are reused, not asked again");
        assert_eq!(other_host, "ask", "an approval can't widen past the request");
        h.shutdown().await;
    })
    .await
}

/// peer batch five item 3: a "this session" approval covers the exact command in the same
/// directory with the same scope (Codex runtimes/shell.rs) — neither a wider nor a narrower one;
/// a non-shell approval never approves other calls of that tool
#[tokio::test]
async fn session_approvals_cover_only_the_exact_command() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let r: Vec<String> = eval(
            &h,
            r#"local c = BASH("git push origin main", "all", "publish")
               VOTE(c)
               C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_session" })
               local w = { id = "w1", name = "web_fetch", arguments = { url = "https://a.example" } }
               local first = VOTE(w, { unconfined = true }).decision
               C.cordis:get("bus"):emit("pipeline/approval-decided", { call = w, decision = "allow_session" })
               return {
                   VOTE(BASH("git push origin main", "all", "again")).decision,
                   VOTE(BASH("git push --force origin main", "all", "wider")).decision,
                   VOTE(BASH("git push", "all", "narrower")).decision,
                   first,
                   VOTE({ name = "web_fetch", arguments = { url = "https://a.example" } }, { unconfined = true }).decision,
                   VOTE({ name = "web_fetch", arguments = { url = "https://b.example" } }, { unconfined = true }).decision,
               }"#,
        )
        .await;
        assert_eq!(r, ["allow", "ask", "ask", "ask", "allow", "ask"]);
        h.shutdown().await;
    })
    .await
}

/// An allow rule with `scope = nil` (or disagreed compound scopes / narrower scope) must
/// never satisfy a `sandbox_permissions` escalation request (`needs == true`).
#[tokio::test]
async fn allow_rule_without_covering_scope_never_auto_allows_escalation() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(
            &env,
            false,
            json!({
                "preset": "auto",
                "rules": [
                    { "argv": ["echo"], "decision": "allow" },
                    { "argv": ["curl"], "decision": "allow", "scope": "network" }
                ]
            }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (nil_scope_esc, disagreed_compound, insufficient_scope, covered_scope): (String, String, String, String) =
            eval(
                &h,
                r#"
            return VOTE(BASH("echo hi", "all", "need all")).decision,
                   VOTE(BASH("echo hi && curl example.com", "all", "need all")).decision,
                   VOTE(BASH("curl example.com", "all", "need all")).decision,
                   VOTE(BASH("curl example.com", "network", "need net")).decision
            "#,
            )
            .await;
        assert_eq!(nil_scope_esc, "ask", "allow rule with scope=nil must ask when escalation requested");
        assert_eq!(disagreed_compound, "ask", "disagreed compound scopes must ask when escalation requested");
        assert_eq!(insufficient_scope, "ask", "narrower rule.scope must ask for wider escalation");
        assert_eq!(covered_scope, "allow", "covering rule.scope satisfies matching escalation");
        h.shutdown().await;
    })
    .await
}
