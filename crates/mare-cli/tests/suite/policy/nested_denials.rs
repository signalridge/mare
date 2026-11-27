use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn nested_contexts_sharing_denial_array_record_once() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("secret"), "hidden").unwrap();
        let h = env.host().no_session().build().await;
        let count: i64 = eval(
            &h,
            r#"
            local denials = {}
            local outer = {denials = denials, sandbox = {level = "read-only", network = true,
                deny_read = {"secret"}, accept_partial = {"fs_deny_read"}}}
            local inner = {denials = denials}
            mare.exec.run(outer, function()
                mare.exec.run(inner, function() pcall(mare.fs.read, "secret") end)
            end)
            return #denials
        "#,
        )
        .await;
        assert_eq!(count, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn nested_denial_evidence_reaches_both_tool_contexts() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("secret"), "hidden").unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        let (direct_grand, direct_inner, direct_outer, pipeline_inner, pipeline_outer, independent): (i64, i64, i64, i64, i64, bool) = eval(&h, r#"
            local policy = {level = "read-only", network = true, deny_read = {"secret"}, accept_partial = {"fs_deny_read"}}
            local outer, inner, grand = {sandbox = policy}, {}, {}
            mare.exec.run(outer, function()
                mare.exec.run(inner, function()
                    mare.exec.run(grand, function() pcall(mare.fs.read, "secret") end)
                end)
            end)
            grand.denials[1].path = "altered-grand"
            local grand_copy = (inner.denials or {})[1] and inner.denials[1].path ~= "altered-grand"
            inner.denials[1].path = "altered"
            local direct_copy = (outer.denials or {})[1] and outer.denials[1].path ~= "altered"
            T:register("nested_evidence", {description = "x", parameters = {type = "object"},
                execute = function()
                    INNER_CTX = {}
                    mare.exec.run(INNER_CTX, function() pcall(mare.fs.read, "secret") end)
                    return "done"
                end })
            local result = T:execute({id = "evidence", name = "nested_evidence", arguments = {}}, {sandbox = policy})
            INNER_CTX.denials[1].path = "altered"
            local pipeline_copy = (result.sandbox_denials or {})[1] and result.sandbox_denials[1].path ~= "altered"
            return #(grand.denials or {}), #(inner.denials or {}), #(outer.denials or {}), #(INNER_CTX.denials or {}), #(result.sandbox_denials or {}), grand_copy and direct_copy and pipeline_copy or false
        "#).await;
        assert_eq!((direct_grand, direct_inner, direct_outer, pipeline_inner, pipeline_outer), (1, 1, 1, 1, 1));
        assert!(independent, "inner evidence mutations must not rewrite outer evidence");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn nested_narrowing_retains_session_deny_acceptance() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("secret-a"), "one").unwrap();
        std::fs::write(env.cwd.join("secret-b"), "two").unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        let (ordinary, strict, same, accepted): (String, String, String, String) = eval(&h, r#"
            local p = {level = "workspace-write", network = true, deny_read = {"secret-a"}, accept_partial = {"fs_deny_read"}}
            mare.proc.sandbox.set_current(p)
            T:register("narrow_inner", {description = "x", parameters = {type = "object"},
                execute = function() return "inner ran" end})
            T:register("narrow_outer", {description = "x", parameters = {type = "object", properties = {kind = {type = "string"}}},
                execute = function(args)
                    local spec = {level = "read-only", network = true}
                    if args.kind == "strict" or args.kind == "accepted" then spec.deny_read = {"secret-b"} end
                    if args.kind == "same" then spec.deny_read = {"secret-a"} end
                    if args.kind == "accepted" then spec.accept_partial = {"fs_deny_read"} end
                    local result = T:execute({id = "inner", name = "narrow_inner", arguments = {}}, {sandbox = spec})
                    return tostring(result.content)
                end})
            local function run(kind)
                return tostring(T:execute({id = kind, name = "narrow_outer", arguments = {kind = kind}}, {}).content)
            end
            local a, b, c, d = run("ordinary"), run("strict"), run("same"), run("accepted")
            mare.proc.sandbox.set_current(nil)
            return a, b, c, d
        "#).await;
        assert!(ordinary.contains("inner ran"), "inherited deny acceptance lost: {ordinary}");
        assert!(strict.contains("fs_deny_read") && !strict.contains("inner ran"), "new strict deny was accepted: {strict}");
        assert!(same.contains("fs_deny_read") && !same.contains("inner ran"), "explicit strict deny was accepted: {same}");
        assert!(accepted.contains("inner ran"), "explicitly accepted new deny did not run: {accepted}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn partial_deny_read_is_visible_for_each_tool_call() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("secret"), "private").unwrap();
        std::fs::write(env.cwd.join("public"), "visible").unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        let (single, normal, session, structured, builtin): (String, String, String, String, String) = eval(&h, r#"
            T:register("partial_probe", { description = "x", parameters = { type = "object" },
                execute = function() return mare.fs.read("public") end })
            T:register("partial_structured", { description = "x", parameters = { type = "object" },
                execute = function() return {content = {{type = "text", text = "visible"}}} end })
            local call = {id = "partial", name = "partial_probe", arguments = {}}
            local p = {level = "workspace-write", network = true, deny_read = {"secret"}, accept_partial = {"fs_deny_read"}}
            local a = T:execute(call, {sandbox = p})
            local b = T:execute({id = "normal", name = "partial_probe", arguments = {}}, {})
            mare.proc.sandbox.set_current(p)
            local c = T:execute({id = "session", name = "partial_probe", arguments = {}}, {})
            local d = T:execute({id = "structured", name = "partial_structured", arguments = {}}, {})
            local e = T:execute({id = "builtin", name = "read", arguments = {path = "public"}}, {})
            mare.proc.sandbox.set_current(nil)
            local blocks = {}
            for _, block in ipairs(d.content) do blocks[#blocks + 1] = block.text or "" end
            local read_text = type(e.content) == "string" and e.content or e.content[1].text
            return tostring(a.content), tostring(b.content), tostring(c.content), table.concat(blocks, "\n"), read_text
        "#).await;
        assert!(single.contains("PARTIAL") && single.contains("fs_deny_read") && single.contains("visible"), "{single}");
        assert_eq!(normal, "visible");
        assert!(session.contains("PARTIAL") && session.contains("fs_deny_read") && session.contains("visible"));
        assert!(structured.contains("PARTIAL") && structured.contains("fs_deny_read") && structured.contains("visible"));
        assert!(builtin.contains("PARTIAL") && builtin.contains("fs_deny_read") && builtin.contains("visible"));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn nested_strict_deny_does_not_inherit_outer_acceptance() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("outer-secret"), "one").unwrap();
        std::fs::write(env.cwd.join("inner-secret"), "two").unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        let result: String = eval(&h, r#"
            T:register("strict_inner", { description = "x", parameters = {type = "object"},
                execute = function() return "incorrectly ran" end })
            T:register("strict_outer", { description = "x", parameters = {type = "object"},
                execute = function()
                    local r = T:execute({id = "inner", name = "strict_inner", arguments = {}},
                        {sandbox = {level = "read-only", network = true, deny_read = {"inner-secret"}}})
                    return tostring(r.content)
                end })
            local r = T:execute({id = "outer", name = "strict_outer", arguments = {}},
                {sandbox = {level = "read-only", network = true, deny_read = {"outer-secret"}, accept_partial = {"fs_deny_read"}}})
            return tostring(r.content)
        "#).await;
        assert!(result.contains("fs_deny_read") && !result.contains("incorrectly ran"), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn nested_tool_deny_read_narrows_the_outer_tool() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("secret"), "hidden").unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        let result: String = eval(&h, r#"
            T:register("inner_deny_probe", { description = "x", parameters = { type = "object" },
                execute = function() return tostring(pcall(mare.fs.read, "secret")) end })
            T:register("outer_deny_probe", { description = "x", parameters = { type = "object" },
                execute = function()
                    local inner = T:execute({id = "inner", name = "inner_deny_probe", arguments = {}},
                        { sandbox = {level = "read-only", network = true, deny_read = {"secret"}, accept_partial = {"fs_deny_read"}} })
                    return tostring(inner.content)
                end })
            return T:execute({id = "outer", name = "outer_deny_probe", arguments = {}},
                { sandbox = {level = "read-only", network = true} }).content
        "#).await;
        assert!(result.contains("false") && result.contains("PARTIAL") && result.contains("fs_deny_read"), "{result}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn deny_read_applies_to_tool_entries_and_preserves_trusted_calls() {
    local(async {
        let env = TestEnv::new().await;
        let private = env.cwd.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("secret"), "secret").unwrap();
        std::fs::write(env.cwd.join("sibling"), "visible").unwrap();
        let script = json!([{ "tool_calls": [{ "name": "deny_entries", "arguments": {} }] }, { "text": "done" }]);
        let h = env.host().faux(script).no_session().build().await;
        expose(&h);
        global(&h, "SECRET", private.join("secret").to_string_lossy().to_string());
        exec(
            &h,
            r#"
            INDEX = mare.fs.index(".")
            mare.proc.sandbox.set_current({ level = "workspace-write", deny_read = { "private" }, accept_partial = { "fs_deny_read" } })
            T:register("deny_entries", {
                description = "x", parameters = { type = "object", properties = {} },
                execute = function()
                    local out = {}
                    local function test(name, f) out[#out + 1] = name .. "=" .. tostring(pcall(f)) end
                    test("read", function() return mare.fs.read("private/secret") end)
                    test("io", function() return io.open(SECRET, "w+") end)
                    test("stat", function() return mare.fs.stat("private/secret") end)
                    test("walk", function() return mare.fs.walk(".") end)
                    test("index", function() return INDEX:files() end)
                    test("refresh", function() return INDEX:refresh() end)
                    test("snapshot", function() return mare.snapshot.content("unused") end)
                    test("loader", function() return mare.loader.prune() end)
                    test("rename", function() return mare.fs.rename(".", "moved") end)
                    local names = {}
                    for _, e in ipairs(mare.fs.list(".")) do names[e.name] = true end
                    out[#out + 1] = "list=" .. tostring(not names.private and names.sibling)
                    local strict_entries, strict_err = mare.fs.list(".", { strict = true })
                    out[#out + 1] = "list_strict=" .. tostring(strict_entries == nil and type(strict_err) == "string")
                    out[#out + 1] = "sibling=" .. tostring(mare.fs.read("sibling") == "visible")
                    return table.concat(out, ",")
                end,
            })
        "#,
        )
        .await;
        let dump = h.prompt("go", json!({})).await.unwrap().to_string();
        for name in ["read", "io", "stat", "walk", "index", "refresh", "snapshot", "loader", "rename"] {
            assert!(dump.contains(&format!("{name}=false")), "{dump}");
        }
        assert!(dump.contains("list=true") && dump.contains("list_strict=true") && dump.contains("sibling=true"), "{dump}");
        assert_eq!(std::fs::read_to_string(private.join("secret")).unwrap(), "secret");
        let trusted: String = eval(&h, "return mare.fs.read('private/secret')").await;
        assert_eq!(trusted, "secret");
        h.shutdown().await;
    })
    .await
}
