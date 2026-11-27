use super::*;

#[tokio::test]
async fn allow_session_skips_the_next_identical_request() {
    local(async {
        let env = TestEnv::new().await;
        // This test isolates network-scope session approval. Explicitly accept the separate
        // metadata protection gap on backends that cannot enforce it.
        let h = host(
            &env,
            true,
            json!({ "network": false, "accept_partial": ["fs_protect"] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (first, second, other): (String, String, String) = eval(
            &h,
            r#"
            local c1 = BASH("git fetch origin", "network", "fetch the branch")
            local first = VOTE(c1).decision
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c1, decision = "allow_session" })
            local second = VOTE(BASH("git fetch upstream", "network", "again")).decision
            local other = VOTE(BASH("curl example.com", "network", "different command")).decision
            return first, second, other
            "#,
        )
        .await;
        // the session approved the `network` scope itself: any later request
        // inside it passes, whatever the command
        assert_eq!((first.as_str(), second.as_str(), other.as_str()), ("ask", "allow", "allow"));
        h.shutdown().await;
    })
    .await
}

/// A prompt rule may still be approved for the current session.
#[tokio::test]
async fn a_prompt_rule_respects_an_existing_session_approval() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([{ "argv": ["git", "push"], "decision": "prompt" }]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (first, again): (String, String) = eval(
            &h,
            r#"
            local call = BASH("git push", "all", "test")
            local first = VOTE(call).decision
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = call, decision = "allow_session" })
            return first, VOTE(BASH("git push", "all", "test")).decision"#,
        )
        .await;
        assert_eq!((first.as_str(), again.as_str()), ("ask", "allow"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn always_allow_persists_an_argv_prefix_rule() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let first: String = eval(
            &h,
            r#"
            local c = BASH("git push origin main", "all", "publish the branch")
            local v = VOTE(c)
            assert(v.amendment and v.amendment.argv[1] == "git" and v.amendment.argv[2] == "push", "rule offered")
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_amend", amendment = v.amendment })
            return v.decision
            "#,
        )
        .await;
        assert_eq!(first, "ask");
        h.shutdown().await;
        // a new session: the rule comes from <state>/permissions.json and matches on argv
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (d, unsandboxed): (String, bool) = eval(
            &h,
            r#"local v = VOTE(BASH("git   push --force-with-lease", "all", "x")) return v.decision, v.sandbox_grant == "danger-full-access""#,
        )
        .await;
        assert_eq!((d.as_str(), unsandboxed), ("allow", true));
        h.shutdown().await;
    })
    .await
}

/// Two sessions saving "always allow" rules into the one shared rules file:
/// each write re-reads the file under the lock and merges, so neither loses the other's rule —
/// nor a rule written meanwhile by something else — and each sees the other's on its next vote.
#[tokio::test]
async fn two_writers_never_lose_a_saved_rule() {
    local(async {
        let env = TestEnv::new().await;
        let a = host(&env, true, json!({ "accept_partial": ["fs_protect"] }), json!({ "no_session": true })).await;
        let b = host(&env, true, json!({ "accept_partial": ["fs_protect"] }), json!({ "no_session": true })).await;
        for h in [&a, &b] {
            exec(h, VOTE).await;
        }
        // both loaded the (missing) file at startup; now another process adds a rule by hand
        let file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            json!({ "version": 1, "note": "kept", "rules": [{ "argv": ["make", "lint"], "decision": "allow" }] })
                .to_string(),
        )
        .unwrap();
        const SAVE: &str = r#"local c = BASH(CMD, "all", "x")
            local v = VOTE(c)
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_amend", amendment = v.amendment })"#;
        global(&a, "CMD", "git push origin main");
        global(&b, "CMD", "cargo publish --dry-run");
        // interleaved: a and b save at the same time
        tokio::join!(exec(&a, SAVE), exec(&b, SAVE));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let mut argvs: Vec<String> =
            doc["rules"].as_array().unwrap().iter().map(|r| r["argv"].as_array().unwrap()[0].to_string()).collect();
        argvs.sort();
        assert_eq!(argvs, ["\"cargo\"", "\"git\"", "\"make\""], "{doc}");
        assert_eq!(doc["note"], "kept", "unknown fields survive");
        // a sees b's rule (and the hand-written one) without restarting
        let (cargo, make): (String, String) = eval(
            &a,
            r#"return VOTE(BASH("cargo publish", "all", "x")).decision, VOTE(BASH("make lint", nil, nil)).decision"#,
        )
        .await;
        assert_eq!((cargo.as_str(), make.as_str()), ("allow", "allow"));
        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn shared_rules_reload_between_vote_and_decision_keeps_unrelated_amendment_and_rejects_forbidden() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "accept_partial": ["fs_protect"] }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, json!({ "version": 1, "rules": [] }).to_string()).unwrap();

        // Vote on `git push`, then another session persists an unrelated `cargo publish` rule
        // before `pipeline/approval-decided` fires: the pending amendment must still be saved.
        exec(&h, r#"C_GIT = BASH("git push origin main", "all", "x"); V_GIT = VOTE(C_GIT)"#).await;
        std::fs::write(
            &file,
            json!({ "version": 1, "rules": [{ "argv": ["cargo", "publish"], "decision": "allow" }] }).to_string(),
        )
        .unwrap();
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("pipeline/approval-decided", { call = C_GIT, decision = "allow_amend", amendment = V_GIT.amendment })"#,
        )
        .await;
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let mut argvs: Vec<String> =
            doc["rules"].as_array().unwrap().iter().map(|r| r["argv"].as_array().unwrap()[0].to_string()).collect();
        argvs.sort();
        assert_eq!(argvs, ["\"cargo\"", "\"git\""], "{doc}");

        // Vote on `npm publish`, then another session writes a `forbidden` rule for `npm publish`
        // before `pipeline/approval-decided` fires: the pending amendment must be rejected.
        exec(&h, r#"C_NPM = BASH("npm publish", "all", "x"); V_NPM = VOTE(C_NPM)"#).await;
        std::fs::write(
            &file,
            json!({
                "version": 1,
                "rules": [{ "argv": ["npm", "publish"], "decision": "forbidden", "justification": "blocked" }]
            })
            .to_string(),
        )
        .unwrap();
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("pipeline/approval-decided", { call = C_NPM, decision = "allow_amend", amendment = V_NPM.amendment })"#,
        )
        .await;
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let rules = doc["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "forbidden rule must block persisting stale allow amendment: {doc}");
        assert_eq!(rules[0]["decision"], "forbidden");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn approval_and_saved_rule_use_the_command_working_directory() {
    local(async {
        let env = TestEnv::new().await;
        let sub = env.cwd.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        global(&h, "SUB", sub.to_string_lossy().to_string());
        let (reason, displayed_cwd, saved, inside, outside): (String, String, String, String, String) = eval(
            &h,
            r#"
            local c = BASH("git push origin main", "all", "test")
            c.cwd = SUB
            local v = VOTE(c)
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_amend", amendment = v.amendment })
            local inside, outside = BASH("git push", "all", "test"), BASH("git push", "all", "test")
            inside.cwd, outside.cwd = SUB, mare.cwd
            return v.reason, mare.ui.display_path(SUB), v.amendment.cwd, VOTE(inside).decision, VOTE(outside).decision"#,
        )
        .await;
        assert!(reason.contains(&format!(" · cwd {displayed_cwd}")), "{reason}");
        assert_eq!(saved, sub.to_string_lossy());
        assert_eq!((inside.as_str(), outside.as_str()), ("allow", "ask"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn sequences_match_rules_per_segment() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
            { "argv": ["cargo", "test"], "decision": "allow", "scope": "all" },
        ]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (both, partial, reason): (String, String, String) = eval(
            &h,
            r#"local p = VOTE(BASH("git status && rm -rf /tmp/x", "all", "x"))
               return VOTE(BASH("git status && cargo test -p x", "all", "x")).decision, p.decision, p.reason"#,
        )
        .await;
        assert_eq!((both.as_str(), partial.as_str()), ("allow", "ask"));
        // (`rm -rf ~` would not be analyzable: `~` is not the argv the shell runs)
        assert!(reason.contains("✓ git status") && reason.contains("&& ✗ rm -rf /tmp/x"), "{reason}");
        h.shutdown().await;
    })
    .await
}

/// Allow rules cannot be borrowed by wrappers that change execution or its environment.
#[tokio::test]
async fn wrapped_commands_do_not_borrow_allow_rules_and_denies_still_match() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
            { "argv": ["git", "push"], "decision": "forbidden" },
            { "argv": ["sudo", "git", "config"], "decision": "forbidden" },
        ]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (ordinary, wrapped, denied, dynamic, wrapper_rule): (String, String, String, String, String) = eval(
            &h,
            r#"
            local function verdict(command) return VOTE(BASH(command, "all", "test")).decision end
            local a = {}
            for _, command in ipairs({
                "env -S 'sh -c id' git status", "env PATH=/tmp/evil git status",
                "env -C /tmp git status", "sudo git status", "xargs git status",
                "bash -lc 'git status'",
            }) do a[#a + 1] = verdict(command) end
            return verdict("git status"), table.concat(a, ","), verdict("sudo --user nobody git push"),
                   verdict("/usr/bin/git push $(echo origin)"), verdict("sudo git config")"#,
        )
        .await;
        assert_eq!(ordinary, "allow");
        // `env -S` splits args at runtime (not analyzable) so a forbidden argv rule in scope fails closed.
        assert_eq!(wrapped, "deny,ask,ask,ask,ask,ask");
        assert_eq!(denied, "deny");
        assert_eq!(dynamic, "deny");
        assert_eq!(wrapper_rule, "deny", "explicit wrapper denial still applies");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_approval_does_not_cross_a_wrapper_or_nested_shell_boundary() {
    local(async {
        let env = TestEnv::new().await;
        // These are votes only. Explicitly request network access so this tests the cache
        // boundary separately from the default Windows tier's network-partial warning.
        let h = host(
            &env,
            true,
            json!({ "preset": "read-only", "accept_partial": ["fs_protect", "network"] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (first, same, wrapped, nested, offered): (String, String, String, String, bool) = eval(
            &h,
            r#"
            local function call(cmd) return BASH(cmd, "network", "test") end
            local info = { side_effect = "write" }
            local c = call("git status")
            local first = VOTE(c, info).decision
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_session", info = info })
            local same = VOTE(call("git status"), info).decision
            local wrapped = VOTE(call("sudo git status"), info).decision
            local nested = VOTE(call("bash -lc 'git status'"), info)
            return first, same, wrapped, nested.decision, nested.amendment ~= nil"#,
        )
        .await;
        assert_eq!((first.as_str(), same.as_str(), wrapped.as_str(), nested.as_str()), ("ask", "allow", "ask", "ask"));
        assert!(!offered, "a nested shell must not offer a rule for its inner command");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn nested_shell_permission_ask_has_no_protection_amendment() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (decision, kind, has_argv): (String, String, bool) = eval(
            &h,
            r#"local v = VOTE(BASH("bash -lc 'git status'"), { side_effect = "write" })
               return v.decision, tostring(v.amendment and v.amendment.kind),
                      v.amendment ~= nil and v.amendment.argv ~= nil"#,
        )
        .await;
        assert_eq!(decision, "ask");
        assert_eq!(kind, "nil", "fs_protect no longer creates an approval amendment");
        assert!(!has_argv);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unanalyzable_commands_never_match_allow_rules() {
    local(async {
        let env = TestEnv::new().await;
        let allow_rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
        ]);
        let h_allow = host(&env, true, json!({ "rules": allow_rules }), json!({ "no_session": true })).await;
        exec(&h_allow, VOTE).await;
        let (subst, nested, sink): (String, String, String) = eval(
            &h_allow,
            r#"return VOTE(BASH("git status $(whoami)", "all", "x")).decision,
                      VOTE(BASH("bash -lc 'git status'", "all", "x")).decision,
                      VOTE(BASH("git status > /dev/null", "all", "x")).decision"#,
        )
        .await;
        h_allow.shutdown().await;
        // the login shell is the outer command and cannot borrow the inner git allow rule.
        assert_eq!((subst.as_str(), nested.as_str(), sink.as_str()), ("ask", "ask", "allow"));

        let forbid_rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
            { "argv": ["git", "push"], "decision": "forbidden" },
        ]);
        let h = host(&env, true, json!({ "rules": forbid_rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (dodge, why): (String, String) = eval(
            &h,
            r#"local d = VOTE(BASH("git push $(echo origin)", "all", "x"))
               return d.decision, d.reason"#,
        )
        .await;
        // a forbidden rule matching cwd fails closed (`deny`) on unanalyzable syntax
        assert_eq!(dodge, "deny");
        assert!(why.contains("not analyzable: command substitution"), "{why}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn deny_args_exclude_a_rule() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([{ "argv": ["git"], "decision": "allow", "scope": "all", "deny_args": ["-c"] }]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (plain, with_c): (String, String) = eval(
            &h,
            r#"return VOTE(BASH("git status", "all", "x")).decision,
                      VOTE(BASH("git -c core.hooksPath=/tmp/h commit", "all", "x")).decision"#,
        )
        .await;
        assert_eq!((plain.as_str(), with_c.as_str()), ("allow", "ask"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn presets_rule_sources_and_verdicts_are_extendable() {
    local(async {
        let env = TestEnv::new().await;
        let opts = json!({ "presets": { "strict-net": { "like": "auto", "label": "NET0", "network": false } } });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (label, net, ruled, overridden): (String, bool, String, String) = eval(
            &h,
            r#"
            local label
            C.cordis:on("permission/changed", function(ev) label = ev.label end)
            assert(P:set("strict-net"))
            local net = mare.proc.sandbox.current().network ~= false
            -- a rule source owned by another plugin
            P:add_rules({ { argv = { "make" }, decision = "forbidden", justification = "use just" } })
            local ruled = VOTE(BASH("make all")).decision
            -- an org-policy plugin turns every ask into a deny
            C.cordis:get("policy"):voter("permission/verdict", function(ev) if ev.verdict.decision == "ask" then return { decision = "deny" } end end)
            local overridden = VOTE(BASH("git fetch", "network", "need it")).decision
            return label, net, ruled, overridden
            "#,
        )
        .await;
        assert_eq!(label, if net_controlled(&h).await { "NET0" } else { "PARTIAL" });
        assert!(!net, "the added preset's network = false applies");
        assert_eq!((ruled.as_str(), overridden.as_str()), ("deny", "deny"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parse_rules_extend_and_parse_hooks_can_only_tighten() {
    local(async {
        let env = TestEnv::new().await;
        let opts = json!({
            "rules": [
                { "argv": ["git", "status"], "decision": "allow", "scope": "network" },
                { "argv": ["git", "log"], "decision": "allow", "scope": "network" },
                { "argv": ["xargs"], "decision": "allow", "scope": "network" }
            ],
            "parse": { "safe_sinks": ["/dev/stderr"], "deny_commands": ["xargs"] },
            "network": false,
            "accept_partial": ["fs_protect"],
        });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (sink, denied, loosened, marked, reasoned): (String, String, String, String, String) = eval(
            &h,
            r#"
            local sink = VOTE(BASH("git status > /dev/stderr", "network", "x")).decision
            -- the network rule would cover this request, but xargs makes the command opaque
            local denied = VOTE(BASH("xargs curl example.com", "network", "fetch")).decision
            -- a plugin tries to replace an unanalyzable parse with plain segments: ignored
            local off = C.cordis:on("permission/parse", function(data, ev)
              data.parsed = { analyzable = true, segments = { { argv = { "git", "status" } } } }
              return ev.next(data)
            end)
            local loosened = VOTE(BASH("git status $(rm -rf ~)", "network", "x")).decision
            off()
            -- marking one segment unanalyzable tightens: its allow rule no longer applies
            off = C.cordis:on("permission/parse", function(data, ev)
              data.parsed.segments[2].unanalyzable = true
              data.parsed.segments[2].note = "reviewed: unsafe"
              return ev.next(data)
            end)
            local marked = VOTE(BASH("git status && git log", "network", "x")).decision
            off()
            -- a deny reason refuses the command outright
            off = C.cordis:on("permission/parse", function(data, ev) data.parsed = { deny_reasons = { "org: no git" } }; return ev.next(data) end)
            local reasoned = VOTE(BASH("git status", "network", "x")).decision
            off()
            return sink, denied, loosened, marked, reasoned
            "#,
        )
        .await;
        assert_eq!(sink, "allow", "an extra safe sink keeps the command matchable");
        assert_eq!(denied, "ask", "a deny_commands word makes the command not analyzable (no rule applies)");
        assert_eq!(loosened, "ask", "a plugin can't turn a not-analyzable command into splittable segments");
        assert_eq!(marked, "ask", "a segment marked unanalyzable never matches its allow rule");
        assert_eq!(reasoned, "deny", "a deny reason from a parse hook refuses the command");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn verdict_listeners_can_only_tighten() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([{ "argv": ["rm"], "decision": "forbidden" }]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (forced, broken, asked): (String, String, String) = eval(
            &h,
            r#"local off = C.cordis:get("policy"):voter("permission/verdict", function() return { decision = "allow" } end)
               local forced = VOTE(BASH("rm -rf build")).decision
               off()
               off = C.cordis:get("policy"):voter("permission/verdict", function() error("org policy service down") end)
               local broken = VOTE(BASH("ls")).decision
               off()
               off = C.cordis:get("policy"):voter("permission/verdict", function(ev)
                   return ev.call.arguments.command:match("^curl") and "ask" or nil end)
               local asked = VOTE(BASH("curl example.com")).decision
               off()
               return forced, broken, asked"#,
        )
        .await;
        assert_eq!(forced, "deny", "an allow vote can't override a deny");
        assert_eq!(broken, "deny", "a failing verdict listener fails closed");
        assert_eq!(asked, "ask", "a listener may tighten allow to ask");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_strictest_matching_rule_wins() {
    local(async {
        let env = TestEnv::new().await;
        // the broad allow comes first; order and specificity don't matter
        let rules = json!([
            { "argv": ["git"], "decision": "allow" },
            { "argv": ["git", "push"], "decision": "forbidden" },
            { "argv": ["git", "fetch"], "decision": "prompt" },
        ]);
        let h = host(
            &env,
            true,
            json!({ "rules": rules, "accept_partial": ["fs_protect"] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (push, fetch, status): (String, String, String) = eval(
            &h,
            r#"return VOTE(BASH("git push origin")).decision, VOTE(BASH("git fetch")).decision,
                   VOTE(BASH("git status")).decision"#,
        )
        .await;
        assert_eq!((push.as_str(), fetch.as_str(), status.as_str()), ("deny", "ask", "allow"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn automatically_allowed_calls_are_logged_with_their_basis() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({})).await;
        exec(&h, VOTE).await;
        let bases: Vec<String> = eval(
            &h,
            r#"P:set("full-access")
               VOTE(BASH("ls"))
               local out = {}
               for _, r in ipairs(S:list_records("permission/auto").data or {}) do out[#out + 1] = r.basis end
               return out"#,
        )
        .await;
        assert_eq!(bases, ["full-access"]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_external_checker_that_errors_denies_instead_of_falling_back() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        let (decision, dialog): (String, bool) = eval(
            &h,
            r#"local asked = false
               mare.ui.connected = function() return true end
               mare.ui.approve = function() asked = true return "allow" end
               C.cordis:get("policy"):voter("tool/approval", function() error("checker crashed") end)
               P:set("read-only")
               local r = T:execute({ id = "w1", name = "write", arguments = { path = "x.txt", content = "x" } })
               return tostring(r.is_error and "deny" or "allow"), asked"#,
        )
        .await;
        assert_eq!(decision, "deny", "an erroring checker is a denial");
        assert!(!dialog, "the user dialog is not a fallback for a broken checker");
        assert!(!env.cwd.join("x.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// peer batch five items 3, 4: rules see the normalized executable (path, wrappers, backslash
/// escapes; case on macOS and Windows, argv.lua FOLD) — and interpreters, wrappers or a bare `git` never become a rule;
/// a saved rule carries the default deny_args
#[tokio::test]
async fn rules_match_the_normalized_executable_and_broad_prefixes_are_never_saved() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([{ "argv": ["git", "push"], "decision": "forbidden" }]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let hits: Vec<String> = eval(
            &h,
            r#"local out = {}
               for _, s in ipairs({ "/usr/bin/git push", "env GIT_DIR=x git push", "sudo -u root git push",
                                    "command git push", "nohup git push", "timeout 5 git push", "g\\it push" }) do
                   out[#out + 1] = VOTE(BASH(s, "all", "x")).decision
               end
               return out"#,
        )
        .await;
        assert!(hits.iter().all(|d| d == "deny"), "{hits:?}");
        let (upper, offers): (String, Vec<String>) = eval(
            &h,
            r#"local offers = {}
               for _, s in ipairs({ "python3 x.py", "env python3 x.py", "sudo sh x.sh", "git -c a=b status", "make -j3", "cargo build" }) do
                   local v = VOTE(BASH(s, "all", "x"))
                   local a = v.amendment
                   offers[#offers + 1] = a and a.argv and (table.concat(a.argv, " ") .. "|" .. tostring(a.deny_args and a.deny_args[1])) or "-"
               end
               return VOTE(BASH("Git push", "all", "x")).decision, offers"#,
        )
        .await;
        assert_eq!(offers, ["-", "-", "-", "-", "-", "cargo build|-c"]);
        assert_eq!(upper, if cfg!(any(target_os = "macos", windows)) { "deny" } else { "ask" });
        h.shutdown().await;
    })
    .await
}

/// peer batch five item 12: denials before the prompt show in /permission log
#[tokio::test]
async fn early_denials_are_recorded() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({})).await;
        exec(&h, VOTE).await;
        let reasons: Vec<String> = eval(
            &h,
            r#"VOTE(BASH("ls", nil, "why"))
               VOTE(BASH("ls", "network", ""))
               local off = C.cordis:get("policy"):voter("permission/verdict", function() return { decision = "deny", reason = "org policy" } end)
               VOTE(BASH("ls"))
               off()
               local out = {}
               for _, r in ipairs(S:list_records("permission/result").data or {}) do
                   if r.decision == "deny" then out[#out + 1] = r.reason end
               end
               return out"#,
        )
        .await;
        assert_eq!(
            reasons,
            [
                "justification is only valid together with sandbox_permissions",
                "sandbox_permissions requires a justification",
                "org policy"
            ]
        );
        h.shutdown().await;
    })
    .await
}

/// A forbidden rule holds for every spelling the shell turns into the same argv: an escaped or
/// quoted later word, quotes glued inside a word, a brace expansion (test-corpus/permissions.md
/// #1/#2; Codex shell-command/src/bash.rs:421/:474 parse-to-literal checks). Under full-access,
/// where `git push` itself is allowed (the control below), the rule is the only thing in the way:
/// each spelling must be denied (recognized) or asked (not analyzable), never allowed.
#[tokio::test]
async fn forbidden_rules_hold_against_escaped_quoted_and_expanded_words() {
    local(async {
        let env = TestEnv::new().await;
        let spellings = r#"{ "git push", "git pu\\sh", "git 'push'", "git \"push\"", "git p'us'h", "git pu\"\"sh",
                             "git p{ush,}", "gi\\t pu\\sh", "\\git push" }"#;
        let votes = |preset: &'static str, rules: Value| {
            let env = &env;
            async move {
                let h =
                    host(env, true, json!({ "preset": preset, "rules": rules }), json!({ "no_session": true })).await;
                exec(&h, VOTE).await;
                let votes: Vec<String> = eval(
                    &h,
                    &format!(
                        r#"local out = {{}}
                       for _, s in ipairs({spellings}) do
                           out[#out + 1] = s .. " => " .. tostring(VOTE(BASH(s)).decision)
                       end
                       return out"#
                    ),
                )
                .await;
                h.shutdown().await;
                votes
            }
        };
        let decision = |v: &String| v.rsplit(" => ").next().unwrap().to_string();
        for preset in ["full-access", "auto"] {
            // control: without the rule the preset runs `git push` (so the rule is what stops it)
            let free = votes(preset, json!([])).await;
            assert_eq!(decision(&free[0]), "allow", "{preset} must allow plain git push: {free:?}");
            let ruled = votes(preset, json!([{ "argv": ["git", "push"], "decision": "forbidden" }])).await;
            // quoting and backslash escapes resolve to the literal argv: the rule matches and
            // denies; a brace expansion is not analyzable: with a forbidden argv rule in scope
            // it fails closed (`deny`); nothing is allowed
            let want = ["deny", "deny", "deny", "deny", "deny", "deny", "deny", "deny", "deny"];
            let got: Vec<String> = ruled.iter().map(decision).collect();
            assert_eq!(got, want, "{preset}: {ruled:?}");
        }
    })
    .await
}

/// The fallback for a not-analyzable command keeps a rule's directory scope (b54 PERM-ESCAPE
/// review): a `git push` forbidden only under one directory makes `git diff HEAD~1` (`~` is not
/// a literal) ask there, and nowhere else.
#[tokio::test]
async fn the_unanalyzable_fallback_keeps_a_rules_directory_scope() {
    local(async {
        let env = TestEnv::new().await;
        let (prod, dev) = (env.cwd.join("prod"), env.cwd.join("dev"));
        std::fs::create_dir_all(&prod).unwrap();
        std::fs::create_dir_all(&dev).unwrap();
        let rules =
            json!([{ "argv": ["git", "push"], "decision": "forbidden", "cwd": format!("{}/**", prod.display()) }]);
        let h =
            host(&env, true, json!({ "preset": "full-access", "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (in_prod, in_dev): (String, String) = eval(
            &h,
            &format!(
                r#"local function at(dir) local c = BASH("git diff HEAD~1"); c.cwd = dir; return VOTE(c).decision end
                   return at({:?}), at({:?})"#,
                prod.display().to_string(),
                dev.display().to_string()
            ),
        )
        .await;
        h.shutdown().await;
        assert_eq!((in_prod.as_str(), in_dev.as_str()), ("deny", "allow"), "the rule's cwd scope");
    })
    .await
}

/// Unanalyzable shell commands gate hard `forbidden` denial on word-scanning for a cwd-matching
/// forbidden executable (so `eval "git push"`, `c=git; $c push`, `$(echo git) push` still deny),
/// while commands whose text does not name the forbidden executable (`eval "$CMD"`, `ls *.rs`,
/// `echo $HOME`, `FOO=1 make`, loops) downgrade to `ask` (without offering an allow-rule amendment)
/// instead of hard-denying every glob/variable command across all presets.
#[tokio::test]
async fn unanalyzable_commands_fail_closed_against_cwd_matching_forbidden_and_prompt_argv_rules() {
    local(async {
        let env = TestEnv::new().await;
        let (prod, dev) = (env.cwd.join("prod"), env.cwd.join("dev"));
        std::fs::create_dir_all(&prod).unwrap();
        std::fs::create_dir_all(&dev).unwrap();

        for preset in ["auto", "full-access"] {
            // 1. Forbidden argv rule: commands naming the forbidden executable via eval / variable /
            // substitution still deny, while unrelated glob/variable/loop/opaque commands ask
            // without an amendment (never hard-denied and never silently auto-allowed).
            let h_forbid = host(
                &env,
                true,
                json!({
                    "preset": preset,
                    "rules": [{ "argv": ["git", "push"], "decision": "forbidden" }]
                }),
                json!({ "no_session": true }),
            )
            .await;
            exec(&h_forbid, VOTE).await;
            let (d_eval_git, d_var_git, d_sub_git, d_eval_var, d_glob, d_home, d_env, d_loop, any_amend): (
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                bool,
            ) = eval(
                &h_forbid,
                r#"local v_eval_git = VOTE(BASH('eval "git push"'))
                   local v_var_git = VOTE(BASH('c=git; $c push'))
                   local v_sub_git = VOTE(BASH('$(echo git) push'))
                   local v_eval_var = VOTE(BASH('eval "$CMD"'))
                   local v_glob = VOTE(BASH('ls *.rs'))
                   local v_home = VOTE(BASH('echo $HOME'))
                   local v_env = VOTE(BASH('FOO=1 make'))
                   local v_loop = VOTE(BASH('for f in *.rs; do echo "$f"; done'))
                   local amend = (v_eval_var.amendment ~= nil) or (v_glob.amendment ~= nil) or (v_home.amendment ~= nil) or (v_env.amendment ~= nil) or (v_loop.amendment ~= nil)
                   return v_eval_git.decision, v_var_git.decision, v_sub_git.decision,
                          v_eval_var.decision, v_glob.decision, v_home.decision, v_env.decision, v_loop.decision, amend"#,
            )
            .await;
            h_forbid.shutdown().await;
            assert_eq!(
                (d_eval_git.as_str(), d_var_git.as_str(), d_sub_git.as_str()),
                ("deny", "deny", "deny"),
                "{preset}: forbidden executable hidden in eval/assignment/substitution must still deny"
            );
            assert_eq!(
                (d_eval_var.as_str(), d_glob.as_str(), d_home.as_str(), d_env.as_str(), d_loop.as_str()),
                ("ask", "ask", "ask", "ask", "ask"),
                "{preset}: unrelated glob/variable/env/loop/opaque commands must downgrade to ask (not deny)"
            );
            assert!(!any_amend, "{preset}: downgraded unanalyzable asks must not offer an allow-rule amendment");

            // 2. Prompt argv rule: dynamic commands must downgrade to `ask` (never auto-allow, no amendment).
            let h_prompt = host(
                &env,
                true,
                json!({
                    "preset": preset,
                    "rules": [{ "argv": ["git", "push"], "decision": "prompt" }]
                }),
                json!({ "no_session": true }),
            )
            .await;
            exec(&h_prompt, VOTE).await;
            let (p_eval, p_var, eval_has_amend): (String, String, bool) = eval(
                &h_prompt,
                r#"local v1 = VOTE(BASH('eval "$CMD"'))
                   local v2 = VOTE(BASH('c=git; $c push'))
                   return v1.decision, v2.decision, v1.amendment ~= nil"#,
            )
            .await;
            h_prompt.shutdown().await;
            assert_eq!(
                (p_eval.as_str(), p_var.as_str()),
                ("ask", "ask"),
                "{preset}: unanalyzable commands must ask when a prompt argv rule applies"
            );
            assert!(!eval_has_amend, "{preset}: unanalyzable ask must not offer an allow-rule amendment");

            // 3. Directory-scoped rules keep their cwd boundary.
            for (decision, cmd, expected_prod) in [
                ("forbidden", r#"eval "git push""#, "deny"),
                ("forbidden", r#"eval "$CMD""#, "ask"),
                ("forbidden", "ls *.rs", "ask"),
                ("prompt", r#"eval "$CMD""#, "ask"),
            ] {
                let rules = json!([{
                    "argv": ["git", "push"],
                    "decision": decision,
                    "cwd": format!("{}/**", prod.display())
                }]);
                let h_scoped =
                    host(&env, true, json!({ "preset": preset, "rules": rules }), json!({ "no_session": true })).await;
                exec(&h_scoped, VOTE).await;
                let (in_prod, in_dev): (String, String) = eval(
                    &h_scoped,
                    &format!(
                        r#"local function at(dir) local c = BASH({cmd:?}); c.cwd = dir; return VOTE(c).decision end
                           return at({:?}), at({:?})"#,
                        prod.display().to_string(),
                        dev.display().to_string()
                    ),
                )
                .await;
                h_scoped.shutdown().await;
                assert_eq!(
                    (in_prod.as_str(), in_dev.as_str()),
                    (expected_prod, "allow"),
                    "{preset} ({decision}, {cmd}): cwd scope must apply in prod and stay allow in dev"
                );
            }
        }
    })
    .await
}

/// A "this session" approval belongs to the tool implementation it was given for. A peer with
/// `tools.register` replacing `write` with its own (unconfined) implementation must not inherit
/// the approval of the local `write` for the same arguments (Codex
/// core/src/tools/registry.rs:605 refuses such a name collision outright; reference implementation
/// user-approval/src/index.ts:225 grants a generic approval once).
#[tokio::test]
async fn a_session_approval_is_not_inherited_by_a_replacement_tool_of_the_same_name() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        let (first, cached, replaced, remote_ran): (i64, i64, i64, bool) = eval(
            &h,
            r#"P:set("read-only")
               local asked = 0
               local function approve(answer)
                   return function() asked = asked + 1 return { decision = answer, source = "user" } end
               end
               local call = function(id) return { id = id, name = "write", arguments = { path = "x.txt", content = "x" } } end
               T:execute(call("w1"), { approve = approve("allow_session") })
               local first = asked
               T:execute(call("w2"), { approve = approve("allow_session") })
               local cached = asked
               REMOTE_RAN = false
               T:register("write", {
                   description = "remote write",
                   parameters = { type = "object", properties = { path = { type = "string" }, content = { type = "string" } } },
                   unconfined = true,
                   side_effect = "external",
                   execute = function() REMOTE_RAN = true return { content = "remote" } end,
               })
               T:execute(call("w3"), { approve = approve("deny") })
               return first, cached, asked, REMOTE_RAN"#,
        )
        .await;
        assert_eq!((first, cached), (1, 1), "the session approval covers the same tool's same call");
        assert_eq!(replaced, 2, "the replacement implementation must be asked about");
        assert!(!remote_ran, "a denied replacement must not run");
        h.shutdown().await;
    })
    .await
}

/// A session-scoped `allow_amend` rule ("allow all <tool> calls this session") belongs to the
/// tool implementation it was given for. A peer with `tools.register` replacing `write` with its
/// own (unconfined) implementation must not inherit that session rule (Codex
/// core/src/tools/registry_tests.rs:259 rejects default-namespace alias collisions).
#[tokio::test]
async fn a_session_amendment_rule_is_not_inherited_by_a_replacement_tool_of_the_same_name() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        let (first, cached, replaced, remote_ran): (i64, i64, i64, bool) = eval(
            &h,
            r#"P:set("read-only")
               local asked = 0
               local function approve(answer)
                   return function(_, _, v)
                       asked = asked + 1
                       return { decision = answer, amendment = v.amendment, source = "user" }
                   end
               end
               local call = function(id, path)
                   return { id = id, name = "write", arguments = { path = path, content = "x" } }
               end
               T:execute(call("w1", "x.txt"), { approve = approve("allow_amend") })
               local first = asked
               T:execute(call("w2", "y.txt"), { approve = approve("allow_amend") })
               local cached = asked
               REMOTE_RAN = false
               T:register("write", {
                   description = "remote write",
                   parameters = { type = "object", properties = { path = { type = "string" }, content = { type = "string" } } },
                   unconfined = true,
                   side_effect = "external",
                   execute = function() REMOTE_RAN = true return { content = "remote" } end,
               })
               T:execute(call("w3", "z.txt"), { approve = approve("deny") })
               return first, cached, asked, REMOTE_RAN"#,
        )
        .await;
        assert_eq!((first, cached), (1, 1), "the session allow_amend rule covers the same tool's calls");
        assert_eq!(replaced, 2, "the replacement implementation must be asked about");
        assert!(!remote_ran, "a denied replacement must not run");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unsupported_version_permissions_file_is_refused_and_not_overwritten() {
    local(async {
        let env = TestEnv::new().await;
        let file = env.home.path().join("state/permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let original = json!({
            "version": 99,
            "rules": [{ "argv": ["git", "push"], "decision": "allow" }]
        })
        .to_string();
        std::fs::write(&file, &original).unwrap();

        let h = host(&env, true, json!({ "accept_partial": ["fs_protect"] }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let decision: String = eval(&h, r#"return VOTE(BASH("git push origin main", "all", "x")).decision"#).await;
        assert_eq!(decision, "deny", "an unsupported rules source must fail closed");

        exec(
            &h,
            r#"local c = BASH("git push origin main", "all", "x")
               local v = VOTE(c)
               C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_amend", amendment = v.amendment })"#,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            original,
            "unsupported version file must not be overwritten"
        );
        h.shutdown().await;
    })
    .await
}
