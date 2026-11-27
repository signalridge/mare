//! The `context` plugin's inputs: skills per the Agent Skills spec with a real
//! YAML parser and lenient validation (pi skills.ts:310-330), prompt templates sharing that
//! parser, AGENTS.md budget / BOM / blank / duplicate / read-error handling (Codex
//! agents_md_tests.rs, reference implementation), and `@file` references (file-ref).

use crate::common::*;
use serde_json::{Value, json};

fn write(path: &std::path::Path, text: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fixtures(dir: &str) -> String {
    format!("{}/tests/fixtures/{dir}", env!("CARGO_MANIFEST_DIR"))
}

async fn context_host(env: &TestEnv, cfg: Value) -> std::sync::Arc<mare_host::Host> {
    let h = env.host().builtins(&["file-ref"]).faux(json!([{ "text": "ok" }])).config(cfg).no_session().build().await;
    expose(&h);
    global(&h, "X", h.service_table("context").unwrap());
    global(&h, "CMD", h.service_table("commands").unwrap());
    h
}

fn to_json(h: &mare_host::Host, v: mlua::Value) -> Value {
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

/// The persisted skills index is a registered artifact and must satisfy the closure check.
#[tokio::test]
async fn skills_cache_is_a_registered_artifact() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skills");
        write(&root.join("one/SKILL.md"), "---\nname: one\ndescription: cache me\n---\nbody\n");
        let h = context_host(&env, json!({ "plugins": { "context": { "skill_dirs": [root] } } })).await;
        let _: mlua::Value = eval(&h, "return X:skills()").await;
        h.shutdown().await;
        assert_registered(env.home.path());
    })
    .await;
}

/// A catalog with more than 10,000 JSON nodes must still persist and be reusable by a new host.
#[tokio::test]
async fn skills_cache_persists_and_reuses_large_catalog() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skills");
        let metadata: String = (0..20).map(|i| format!("  key{i}: value{i}\n")).collect();
        for i in 0..500 {
            write(
                &root.join(format!("skill-{i}/SKILL.md")),
                format!("---\nname: skill-{i}\ndescription: skill {i}\nmetadata:\n{metadata}---\nbody\n"),
            );
        }
        let cfg = json!({ "plugins": { "context": { "skill_dirs": [root] } } });
        let h = context_host(&env, cfg.clone()).await;
        let count: i64 = eval(&h, "return #X:skills()").await;
        assert_eq!(count, 500);
        h.shutdown().await;

        let cache_dir = env.home.path().join("state/skills-cache");
        assert!(
            std::fs::read_dir(&cache_dir).is_ok_and(|mut files| files.next().is_some()),
            "large catalog was not persisted"
        );
        let h = context_host(&env, cfg).await;
        let reads: i64 = eval(
            &h,
            r#"local read, count = mare.fs.read, 0
               mare.fs.read = function(path, ...)
                 if type(path) == 'string' and path:sub(-9) == '/SKILL.md' then count = count + 1 end
                 return read(path, ...)
               end
               assert(#X:skills() == 500)
               return count"#,
        )
        .await;
        assert_eq!(reads, 0, "a fresh host should reuse the persisted large catalog");
        h.shutdown().await;
    })
    .await;
}

/// Different workspaces and cache versions must not accumulate snapshots without bound.
#[tokio::test]
async fn skills_cache_bounds_snapshots_across_scopes() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skills");
        write(&root.join("one/SKILL.md"), "---\nname: one\ndescription: current\n---\nbody\n");
        let cache_dir = env.home.path().join("state/skills-cache");
        for i in 0..70 {
            write(&cache_dir.join(format!("{i:064x}-{i:064x}.json")), "{}");
        }
        let h = context_host(&env, json!({ "plugins": { "context": { "skill_dirs": [root] } } })).await;
        let _: mlua::Value = eval(&h, "return X:skills()").await;
        h.shutdown().await;
        let files: Vec<_> = std::fs::read_dir(&cache_dir).unwrap().map(|entry| entry.unwrap().path()).collect();
        assert!(files.len() <= 64, "global skill cache grew to {} files", files.len());
        assert!(files.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains("current"))));
    })
    .await;
}

/// A fresh process can reuse the previous process's skill metadata without reading the body again.
#[test]
fn skills_disk_cache_is_reused_by_a_second_process() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let root = home.path().join("skills");
    let later_root = home.path().join("later-skills");
    let log = home.path().join("skill-reads.log");
    let prompt_log = home.path().join("request-system.txt");
    write(
        &root.join("first/SKILL.md"),
        "---\nname: first\ndescription: cached first skill\n---\nPRIVATE_SKILL_BODY_SENTINEL\n",
    );
    write(
        &later_root.join("duplicate/SKILL.md"),
        "---\nname: first\ndescription: later duplicate skill\n---\nlater body\n",
    );
    write(
        &home.path().join("config/plugins/capture-system.lua"),
        format!(
            r#"return {{ name = "capture-system", inject = {{ "agent" }}, apply = function(ctx)
  ctx:on("request/build", function(req, next)
    local file = assert(io.open({prompt_log}, "w"))
    file:write(req.system or "")
    file:close()
    return next()
  end)
end }}"#,
            prompt_log = json!(prompt_log.to_string_lossy()),
        ),
    );
    let init = |first: &std::path::Path, second: &std::path::Path| {
        format!(
            r#"local original_read = mare.fs.read
mare.fs.read = function(path, ...)
  if type(path) == "string" and path:sub(-9) == "/SKILL.md" then
    local file = assert(io.open({log}, "a"))
    file:write("read\n")
    file:close()
  end
  return original_read(path, ...)
end
mare.setup({{ model = "faux/echo" }})
mare.plugin("context", {{ skill_dirs = {{ {first}, {second} }} }})
mare.plugin("onboarding", {{ plugins = false }})
"#,
            log = json!(log.to_string_lossy()),
            first = json!(first.to_string_lossy()),
            second = json!(second.to_string_lossy()),
        )
    };
    write(&home.path().join("config/init.lua"), init(&root, &later_root));
    let run = || {
        if prompt_log.exists() {
            std::fs::remove_file(&prompt_log).unwrap();
        }
        let output = mare_cmd()
            .env("MARE_HOME", home.path())
            .current_dir(work.path())
            .args(["--model", "faux/echo", "--no-session", "-p", "hello"])
            .output()
            .unwrap();
        assert!(output.status.success(), "mare failed: {}", String::from_utf8_lossy(&output.stderr));
        std::fs::read_to_string(&prompt_log).expect("the process must build a model request")
    };
    let first_prompt = run();
    assert!(first_prompt.contains("cached first skill") && !first_prompt.contains("later duplicate skill"));
    let first_reads = std::fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert!(first_reads > 0, "the first process must read the skill");
    let cache = home.path().join("state/skills-cache");
    let files: Vec<_> = std::fs::read_dir(&cache)
        .expect("the first process must persist the skill index")
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(!files.is_empty(), "no persisted skill index in {}", cache.display());
    for path in files {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("PRIVATE_SKILL_BODY_SENTINEL"), "skill bodies must never be cached");
    }
    let cached_prompt = run();
    let second_reads = std::fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert_eq!(second_reads, first_reads, "a fresh process reread an unchanged SKILL.md");
    assert!(cached_prompt.contains("cached first skill") && !cached_prompt.contains("later duplicate skill"));

    write(&home.path().join("config/init.lua"), init(&later_root, &root));
    let reordered_prompt = run();
    assert!(reordered_prompt.contains("later duplicate skill") && !reordered_prompt.contains("cached first skill"));
}

/// reference implementation: additions, edits, and
/// deletions invalidate the observed catalog. Also exercise disk-cache corruption across hosts.
#[tokio::test]
async fn skills_cache_tracks_changes_on_the_first_request_and_recovers_corruption() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skill-root");
        let first = root.join("first/SKILL.md");
        let second = root.join("second/SKILL.md");
        write(&first, "---\nname: first\ndescription: FIRST_OLD\n---\nfirst body\n");
        let cfg = json!({ "plugins": { "context": { "skill_dirs": [root.display().to_string()] } } });
        let h = context_host(&env, cfg.clone()).await;
        let before: String = eval(&h, "return X:system_prompt(C.cordis:get('llm'):current())").await;
        assert!(before.contains("FIRST_OLD"), "{before}");
        let original: String =
            eval(&h, "local s = X:skills(); s[1].description = 'MUTATED'; return X:skills()[1].description").await;
        assert_eq!(original, "FIRST_OLD", "a caller must not mutate the shared cached snapshot");
        let (cached_reads, doctor_reads): (i64, i64) = eval(
            &h,
            r#"local original_read, count = mare.fs.read, 0
               mare.fs.read = function(path, ...)
                 if type(path) == 'string' and path:sub(-9) == '/SKILL.md' then count = count + 1 end
                 return original_read(path, ...)
               end
               X:skills()
               local cached = count
               C.cordis:get('bus'):emit('kernel/doctor', {})
               mare.fs.read = original_read
               return cached, count"#,
        )
        .await;
        assert_eq!(cached_reads, 0, "unchanged skills should come from the validated cache");
        assert!(doctor_reads > 0, "doctor must inspect the actual skill files");
        exec(&h, "C.cordis:on('request/build', function(req, next) CAPTURED_SYSTEM = req.system; return next() end)")
            .await;

        write(&first, "---\nname: first\ndescription: FIRST_CHANGED\n---\nfirst body\n");
        write(&second, "---\nname: second\ndescription: SECOND_ADDED\n---\nsecond body\n");
        let response = h.prompt("first request", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let after: String = eval(&h, "return CAPTURED_SYSTEM").await;
        assert!(after.contains("FIRST_CHANGED") && after.contains("SECOND_ADDED"), "{after}");
        assert!(!after.contains("FIRST_OLD"), "{after}");

        std::fs::remove_dir_all(first.parent().unwrap()).unwrap();
        let response = h.prompt("second request", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let after_removal: String = eval(&h, "return CAPTURED_SYSTEM").await;
        assert!(after_removal.contains("SECOND_ADDED") && !after_removal.contains("FIRST_CHANGED"), "{after_removal}");
        exec(
            &h,
            r#"ORIGINAL_SKILL_WRITE = mare.fs.write
               mare.fs.write = function(path, ...)
                 if path:find('/skills%-cache/') then return nil, 'injected cache write failure' end
                 return ORIGINAL_SKILL_WRITE(path, ...)
               end"#,
        )
        .await;
        write(&root.join("third/SKILL.md"), "---\nname: third\ndescription: WRITE_FAILURE_SAFE\n---\nbody\n");
        let response = h.prompt("cache write fails", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "cache failure must not block a request: {response}");
        let after_failure: String = eval(&h, "return CAPTURED_SYSTEM").await;
        assert!(after_failure.contains("WRITE_FAILURE_SAFE"), "{after_failure}");
        exec(&h, "mare.fs.write = ORIGINAL_SKILL_WRITE; X:skill_problems()").await;
        for version in 1..=6 {
            write(&second, format!("---\nname: second\ndescription: VERSION_{}\n---\nbody\n", "x".repeat(version)));
            let _: mlua::Value = eval(&h, "return X:skills()").await;
        }
        write(&second, "---\nname: second\ndescription: SECOND_ADDED\n---\nsecond body\n");
        let _: mlua::Value = eval(&h, "return X:skills()").await;
        h.shutdown().await;

        let cache_dir = env.home.path().join("state/skills-cache");
        let cache_files: Vec<_> = std::fs::read_dir(&cache_dir).unwrap().map(|e| e.unwrap().path()).collect();
        assert!(!cache_files.is_empty());
        assert!(cache_files.len() <= 4, "old skill snapshots must be bounded: {cache_files:?}");
        let mut tampered = false;
        for file in &cache_files {
            let parsed = std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
            if let Some(mut doc) = parsed
                && let Some(skills) = doc["skills"].as_array_mut()
                && let Some(skill) = skills.iter_mut().find(|s| s["name"] == "second")
            {
                skill["description"] = json!("TAMPERED");
                std::fs::write(file, serde_json::to_vec(&doc).unwrap()).unwrap();
                tampered = true;
            } else {
                std::fs::write(file, b"{corrupt").unwrap();
            }
        }
        assert!(tampered, "expected a cache for the remaining skill");
        let h = context_host(&env, cfg).await;
        let restored: String = eval(&h, "return X:system_prompt()").await;
        assert!(
            restored.contains("SECOND_ADDED") && !restored.contains("FIRST_CHANGED") && !restored.contains("TAMPERED"),
            "{restored}"
        );
        let repaired = std::fs::read_dir(&cache_dir).unwrap().filter_map(Result::ok).any(|entry| {
            std::fs::read_to_string(entry.path()).is_ok_and(|text| {
                !text.contains("TAMPERED")
                    && serde_json::from_str::<Value>(&text).is_ok_and(|doc| {
                        doc["skills"].as_array().is_some_and(|skills| {
                            skills
                                .iter()
                                .any(|skill| skill["name"] == "second" && skill["description"] == "SECOND_ADDED")
                        })
                    })
            })
        });
        assert!(repaired, "corrupt cache must fall back to scanning and be replaced");
        h.shutdown().await;
    })
    .await;
}

/// reference implementation: incomplete scans remain uncached and retry.
#[cfg(unix)]
#[tokio::test]
async fn skills_unreadable_file_is_not_cached_and_recovers() {
    use std::os::unix::fs::PermissionsExt;
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skills");
        let skill = root.join("one/SKILL.md");
        write(&skill, "---\nname: one\ndescription: readable again\n---\nbody\n");
        std::fs::set_permissions(&skill, std::fs::Permissions::from_mode(0o000)).unwrap();
        let h = context_host(&env, json!({ "plugins": { "context": { "skill_dirs": [root] } } })).await;
        let unavailable: i64 = eval(&h, "return #X:skills()").await;
        let cache_dir = env.home.path().join("state/skills-cache");
        let cached_incomplete = std::fs::read_dir(&cache_dir).is_ok_and(|mut files| files.next().is_some());
        std::fs::set_permissions(&skill, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(unavailable, 0);
        assert!(!cached_incomplete, "an unreadable skill must not produce a partial cache");
        let description: String = eval(&h, "return X:skills()[1].description").await;
        assert_eq!(description, "readable again");
        assert!(std::fs::read_dir(&cache_dir).is_ok_and(|mut files| files.next().is_some()));
        h.shutdown().await;
    })
    .await;
}

/// A transient incomplete observation must not freeze the prompt's skill section across requests.
#[tokio::test]
async fn skills_incomplete_observation_retries_on_next_request() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skills");
        let skill = root.join("one/SKILL.md");
        write(&skill, "---\nname: one\ndescription: RETRIED_SKILL\n---\nbody\n");
        let h = context_host(&env, json!({ "plugins": { "context": { "skill_dirs": [root] } } })).await;
        global(&h, "SKILL_ROOT", root.to_string_lossy().to_string());
        global(&h, "SKILL_FILE", skill.to_string_lossy().to_string());
        exec(
            &h,
            r#"BLOCK_SKILL = true
               local list, read = mare.fs.list, mare.fs.read
               local norm = function(p) return type(p) == "string" and p:gsub("\\", "/") or p end
               local target_root, target_file = norm(SKILL_ROOT), norm(SKILL_FILE)
               mare.fs.list = function(path, opts)
                 if BLOCK_SKILL and norm(path) == target_root and opts and opts.strict then
                   return nil, 'injected incomplete observation', 'permission_denied'
                 end
                 return list(path, opts)
               end
               mare.fs.read = function(path, ...)
                 if BLOCK_SKILL and norm(path) == target_file then return nil, 'injected unreadable file' end
                 return read(path, ...)
               end
               C.cordis:on('request/build', function(req, next) CAPTURED_SYSTEM = req.system; return next() end)"#,
        )
        .await;
        let first = h.prompt("first", json!({})).await.unwrap();
        assert!(first["error"].is_null(), "{first}");
        let before: String = eval(&h, "return CAPTURED_SYSTEM").await;
        assert!(!before.contains("RETRIED_SKILL"));
        exec(&h, "BLOCK_SKILL = false").await;
        let second = h.prompt("second", json!({})).await.unwrap();
        assert!(second["error"].is_null(), "{second}");
        let after: String = eval(&h, "return CAPTURED_SYSTEM").await;
        assert!(after.contains("RETRIED_SKILL"), "the next prompt must retry an incomplete scan: {after}");
        h.shutdown().await;
    })
    .await;
}

/// Trust is part of the on-disk cache scope even across separate mare processes.
#[test]
fn skills_cache_does_not_cross_process_trust_boundary() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join(".git/objects")).unwrap();
    std::fs::create_dir_all(work.path().join(".git/refs")).unwrap();
    std::fs::write(work.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    write(
        &work.path().join(".mare/skills/private/SKILL.md"),
        "---\nname: private\ndescription: PROJECT_CACHE_SECRET\n---\nbody\n",
    );
    let prompt_log = home.path().join("system.txt");
    write(
        &home.path().join("config/plugins/capture-system.lua"),
        format!(
            r#"return {{ name = "capture-system", inject = {{ "agent" }}, apply = function(ctx)
  ctx:on("request/build", function(req, next)
    local file = assert(io.open({path}, "w")); file:write(req.system or ""); file:close()
    return next()
  end)
end }}"#,
            path = json!(prompt_log.to_string_lossy()),
        ),
    );
    write(
        &home.path().join("config/init.lua"),
        "mare.setup({ model = 'faux/echo' })\nmare.plugin('context', { skill_dirs = { '.mare/skills' } })\nmare.plugin('onboarding', { plugins = false })\n",
    );
    let run = |trust: &str| {
        let output = mare_cmd()
            .env("MARE_HOME", home.path())
            .current_dir(work.path())
            .args([trust, "--model", "faux/echo", "--no-session", "-p", "hello"])
            .output()
            .unwrap();
        assert!(output.status.success(), "mare failed: {}", String::from_utf8_lossy(&output.stderr));
        std::fs::read_to_string(&prompt_log).unwrap()
    };
    assert!(run("--trust").contains("PROJECT_CACHE_SECRET"));
    assert!(home.path().join("state/skills-cache").is_dir());
    assert!(!run("--no-trust").contains("PROJECT_CACHE_SECRET"));
}

/// A trusted project's cached skills disappear from the first request after trust is revoked.
#[tokio::test]
async fn skills_cache_does_not_expose_project_skills_after_trust_is_revoked() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        std::fs::write(env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        write(
            &env.cwd.join(".mare/skills/private/SKILL.md"),
            "---\nname: private\ndescription: TRUSTED_PROJECT_SKILL\n---\nprivate body\n",
        );
        let mut builder = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .no_session()
            .config(json!({ "plugins": { "context": { "skill_dirs": [".mare/skills"] } } }));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        global(&h, "X", h.service_table("context").unwrap());
        let before: String = eval(&h, "return X:system_prompt(C.cordis:get('llm'):current())").await;
        assert!(before.contains("TRUSTED_PROJECT_SKILL"), "{before}");
        exec(&h, "C.cordis:on('request/build', function(req, next) CAPTURED_SYSTEM = req.system; return next() end)")
            .await;
        h.answer_trust(mare_host::TrustChoice::DontTrust).await.unwrap();
        let response = h.prompt("after revocation", json!({})).await.unwrap();
        assert!(response["error"].is_null(), "{response}");
        let after: String = eval(&h, "return CAPTURED_SYSTEM").await;
        assert!(!after.contains("TRUSTED_PROJECT_SKILL"), "untrusted skill leaked into the request: {after}");
        h.shutdown().await;
    })
    .await;
}

/// A disposed background scan cannot replace a newer scan after a full plugin reload.
#[tokio::test]
async fn skills_background_refresh_cannot_publish_after_reload() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.home.path().join("skill-root");
        let skill = root.join("first/SKILL.md");
        write(&skill, "---\nname: first\ndescription: ORIGINAL\n---\nbody\n");
        let cfg = json!({ "plugins": { "context": { "skill_dirs": [root.display().to_string()] } } });
        let h = context_host(&env, cfg).await;
        let _: mlua::Value = eval(&h, "return X:skills()").await;
        write(&skill, "---\nname: first\ndescription: INTERMEDIATE_VALUE\n---\nbody\n");
        global(&h, "SKILL_FILE", skill.to_string_lossy().to_string());
        exec(
            &h,
            r#"local original_read = mare.fs.read
               local entered_tx, entered_rx = mare.task.channel()
               local gate_tx, gate_rx = mare.task.channel()
               local armed = true
               local norm = function(p) return type(p) == "string" and p:gsub("\\", "/") or p end
               local target_file = norm(SKILL_FILE)
               mare.fs.read = function(path, ...)
                 local data, err, code = original_read(path, ...)
                 if armed and norm(path) == target_file then
                   armed = false
                   entered_tx:send(true)
                   gate_rx:recv()
                 end
                 return data, err, code
               end
               ENTERED = entered_rx
               RELEASE = gate_tx"#,
        )
        .await;
        exec(&h, "C.cordis:get('bus'):emit('ui/ready')").await;
        let entered: bool = eval(&h, "return mare.task.timeout(5000, function() return ENTERED:recv() end)").await;
        assert!(entered, "background scan did not reach the read barrier");
        write(&skill, "---\nname: first\ndescription: FINAL_AFTER_RELOAD\n---\nbody\n");
        tokio::time::timeout(std::time::Duration::from_secs(10), h.reload_all())
            .await
            .expect("reload blocked on the disposed background scan")
            .unwrap();
        expose(&h);
        global(&h, "X", h.service_table("context").unwrap());
        exec(&h, "RELEASE:send(true)").await;
        let after: String = eval(&h, "return X:skills()[1].description").await;
        assert_eq!(after, "FINAL_AFTER_RELOAD");
        let prompt: String = eval(&h, "return X:system_prompt()").await;
        assert!(prompt.contains("FINAL_AFTER_RELOAD") && !prompt.contains("INTERMEDIATE_VALUE"), "{prompt}");
        h.shutdown().await;
    })
    .await;
}

/// A blocked HEAD response must not hold the ui/ready event or the host's disposal.
#[tokio::test]
async fn llm_ready_prewarm_does_not_block_and_is_cancelled_on_shutdown() {
    local(async {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;
        use tokio::sync::oneshot;

        let env = TestEnv::new().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0, "request closed before headers");
                request.extend_from_slice(&buf[..n]);
            }
            let _ = accepted_tx.send(String::from_utf8_lossy(&request).into_owned());
            let closed = socket.read(&mut buf).await.unwrap_or(0) == 0;
            let _ = closed_tx.send(closed);
        });
        let h = env
            .host()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "plugins": { "provider-openai": { "base_url": base } }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        let emitting_host = h.clone();
        let mut emitting = tokio::task::spawn_local(async move {
            exec(&emitting_host, "C.cordis:get('bus'):emit('ui/ready')").await;
        });
        let request = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), accepted_rx)
            .await
            .expect("prewarm request did not reach loopback")
            .unwrap();
        assert!(request.starts_with("HEAD "), "{request}");
        let ready_returned = tokio::time::timeout(std::time::Duration::from_secs(2), &mut emitting).await.is_ok();
        if !ready_returned {
            server.abort();
            let _ = emitting.await;
            h.shutdown().await;
            panic!("ui/ready waited for the prewarm response");
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), h.shutdown())
            .await
            .expect("host shutdown waited for the prewarm response");
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), closed_rx)
            .await
            .expect("prewarm connection stayed open after shutdown")
            .unwrap();
        assert!(closed, "shutdown must cancel the in-flight prewarm");
        server.await.unwrap();
    })
    .await;
}

/// An empty configured URL falls back to the provider URL and closes the response immediately.
#[tokio::test]
async fn llm_ready_prewarm_empty_override_uses_provider_and_closes_response() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .config(json!({ "providers": { "warm-test": { "base_url": "" } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"WARM_CHECKED, WARM_CLOSED, WARM_URL = false, false, nil
               local original_request = mare.http.request
               mare.http.request = function(opts)
                 if opts.method == 'HEAD' then
                   WARM_URL = opts.url
                   return { close = function() WARM_CLOSED = true end }
                 end
                 return original_request(opts)
               end
               local llm = C.cordis:get('llm')
               local registered, why = llm:register_provider({
                 provider = 'warm-test', no_key = true, replace = true,
                 models = { { model_id = 'only', name = 'Only' } },
                 warm_url = function() WARM_CHECKED = true; return 'http://example.test/private' end,
               })
               assert(registered, tostring(why))
               llm:set_model('warm-test/only', { save = false })
               C.cordis:get('bus'):emit('ui/ready')"#,
        )
        .await;
        wait_until(10, "the provider's warm decision", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return WARM_CHECKED").await }
        })
        .await;
        let (url, closed): (Option<String>, bool) = eval(&h, "return WARM_URL, WARM_CLOSED").await;
        assert_eq!(url.as_deref(), Some("http://example.test"));
        assert!(closed, "the warm response must be closed immediately");
        let child_warmed: bool = eval(
            &h,
            r#"WARM_CHECKED = false
               local previous_mode = mare.mode
               mare.mode = 'child'
               C.cordis:get('bus'):emit('ui/ready')
               local tx, rx = mare.task.channel()
               mare.task.spawn(function() tx:send(true) end)
               rx:recv()
               mare.mode = previous_mode
               return WARM_CHECKED"#,
        )
        .await;
        assert!(!child_warmed, "child hosts must not prewarm providers");
        h.shutdown().await;
    })
    .await;
}

/// Prewarming uses the provider route that the subsequent model request uses.
#[tokio::test]
async fn llm_ready_prewarm_uses_provider_proxy_and_connect_timeout() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("NO_PROXY", "");
        env.set_var("no_proxy", "");
        let origin = MockServer::start().await;
        let proxy = MockServer::start().await;
        let credentialed_origin = origin.base.replacen("http://", "http://prewarm-user:USER_SECRET@", 1);
        origin.route("", vec![Resp::status(200, "")]);
        proxy.route("", vec![Resp::status(200, "")]);
        let h = env
            .host()
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "providers": { "openai": {
                    "base_url": format!("{credentialed_origin}/private-path?credential=QUERY_SECRET"),
                    "proxy": proxy.base,
                    "connect_timeout_ms": 2345,
                    "headers": { "x-private-key": "HEADER_SECRET" },
                    "query_params": { "api_key": "PARAM_SECRET" }
                } },
                "plugins": { "provider-openai": { "base_url": origin.base } }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"local original_request = mare.http.request
               mare.http.request = function(opts)
                 if opts.method == "HEAD" then PREWARM_OPTIONS = opts end
                 return original_request(opts)
               end"#,
        )
        .await;
        exec(&h, "C.cordis:get('bus'):emit('ui/ready')").await;
        wait_until(WAIT_SECS, "the prewarm request", || async {
            !origin.requests().is_empty() || !proxy.requests().is_empty()
        })
        .await;
        let (configured_proxy, connect_timeout, redirect): (Option<String>, Option<i64>, Option<String>) =
            eval(&h, "return PREWARM_OPTIONS.proxy, PREWARM_OPTIONS.connect_timeout_ms, PREWARM_OPTIONS.redirect")
                .await;
        assert_eq!(configured_proxy.as_deref(), Some(proxy.base.as_str()));
        assert_eq!(connect_timeout, Some(2345));
        assert_eq!(redirect.as_deref(), Some("same-origin"));
        assert_eq!(origin.requests().len(), 0, "prewarm bypassed the configured proxy");
        let request = &proxy.requests()[0];
        assert_eq!(request.method, "HEAD");
        assert!(!request.path.contains("private-path") && !request.path.contains('?'), "{request:?}");
        assert!(request.header("x-private-key").is_none(), "{request:?}");
        assert!(request.header("authorization").is_none(), "{request:?}");
        let observed = format!("{request:?}");
        for secret in ["USER_SECRET", "QUERY_SECRET", "HEADER_SECRET", "PARAM_SECRET"] {
            assert!(!observed.contains(secret), "prewarm leaked a configured secret: {observed}");
        }
        h.shutdown().await;
    })
    .await;
}

/// A configured URL does not override a provider's decision to skip warming.
#[tokio::test]
async fn llm_ready_prewarm_honors_a_provider_that_declines_warming() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("", vec![Resp::status(200, "")]);
        let h = env
            .host()
            .faux(json!([{ "text": "ok" }]))
            .config(json!({ "providers": { "cold-test": { "base_url": server.base } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"WARM_CHECKED = false
               local llm = C.cordis:get('llm')
               local registered, why = llm:register_provider({
                 provider = 'cold-test', no_key = true, replace = true,
                 models = { { model_id = 'only', name = 'Only' } },
                 warm_url = function() WARM_CHECKED = true; return nil end,
               })
               assert(registered, tostring(why))
               llm:set_model('cold-test/only', { save = false })
               C.cordis:get('bus'):emit('ui/ready')"#,
        )
        .await;
        wait_until(10, "the provider's warm decision", || {
            let h = h.clone();
            let server = &server;
            async move { eval::<bool>(&h, "return WARM_CHECKED").await || !server.requests().is_empty() }
        })
        .await;
        let checked: bool = eval(&h, "return WARM_CHECKED").await;
        assert!(checked, "the provider's warm_url decision was bypassed");
        assert!(server.requests().is_empty(), "a declined warmup must not send a request");
        h.shutdown().await;
    })
    .await;
}

/// Synthetic samples (tests/fixtures/skills): `>` and `|` blocks, escaped quotes, a metadata map,
/// comments, unknown keys, a missing name, an invalid name, an empty description, invalid YAML
/// and a same-named skill in a later directory.
#[tokio::test]
async fn skills_follow_the_agent_skills_spec_leniently() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "context": { "skill_dirs": [fixtures("skills"), fixtures("skills-extra")] } } });
        let h = context_host(&env, cfg).await;
        let skills: mlua::Value = eval(&h, "return X:skills()").await;
        let skills = to_json(&h, skills);
        let by_name = |n: &str| skills.as_array().unwrap().iter().find(|s| s["name"] == n).cloned();
        let mut names: Vec<&str> = skills.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        names.sort();
        assert_eq!(
            names,
            ["Bad_Name--", "colon-in-value", "folded", "full-spec", "literal", "no-name", "quoted"],
            "only the empty description is skipped; the collision keeps the first"
        );
        assert_eq!(
            by_name("folded").unwrap()["description"],
            "FOLDED-START Review pull requests for correctness and style.\nFOLDED-PARA second paragraph."
        );
        assert!(by_name("folded").unwrap()["path"].as_str().unwrap().contains("/skills/folded/"), "first one wins");
        assert_eq!(by_name("literal").unwrap()["description"], "LITERAL-LINE-1\nLITERAL-LINE-2");
        assert_eq!(
            by_name("quoted").unwrap()["description"],
            "Use when the user says \"ship it\": run the release\tchecklist (café)"
        );
        let full = by_name("full-spec").unwrap();
        assert_eq!(full["license"], "Apache-2.0");
        assert_eq!(full["compatibility"], "Requires git and network access");
        assert_eq!(full["allowed_tools"], "Bash(git:*) Read");
        assert_eq!(full["metadata"], json!({ "author": "example-org", "version": "1.0" }));
        assert_eq!(full["argument_hint"], "[pr-number] [--draft]");
        assert_eq!((&full["frontmatter"]["tier"], &full["frontmatter"]["surface"]), (&json!(2), &json!("cli")));
        assert_eq!(
            by_name("colon-in-value").unwrap()["description"],
            "Use when: the value has an unquoted colon (not valid YAML)"
        );

        let probs: mlua::Value = eval(&h, "return X:skill_problems()").await;
        let probs = to_json(&h, probs);
        let about = |dir: &str| -> Vec<(String, bool)> {
            probs
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["path"].as_str().unwrap().ends_with(&format!("/{dir}/SKILL.md")))
                .map(|p| (p["message"].as_str().unwrap().to_string(), p["skipped"] == true))
                .collect()
        };
        assert!(about("full-spec").is_empty() && about("no-name").is_empty() && about("literal").is_empty(), "{probs}");
        let bad = about("Bad_Name");
        assert!(bad.iter().any(|(m, _)| m.contains("invalid characters")) && bad.iter().all(|(_, s)| !s), "{bad:?}");
        assert!(bad.iter().any(|(m, _)| m.contains("start or end with a hyphen")), "{bad:?}");
        assert_eq!(about("empty-description"), [("description is required; skill skipped".to_string(), true)]);
        assert!(about("colon-in-value").iter().any(|(m, s)| m.contains("not valid YAML") && !s));
        let dup: Vec<_> =
            probs.as_array().unwrap().iter().filter(|p| p["message"].as_str().unwrap().contains("collides")).collect();
        assert_eq!(dup.len(), 1, "{probs}");
        assert!(dup[0]["path"].as_str().unwrap().contains("skills-extra"), "the later one is reported: {probs}");

        // the system prompt lists them; the `/` menu shows argument-hint dim after the name
        let prompt: String = eval(&h, "return (X:system_prompt())").await;
        assert!(prompt.contains("FOLDED-PARA") && !prompt.contains("empty-description"), "{prompt}");
        let menu: Value = mare_shared::lua::lua_to_json(
            h.lua(),
            eval(
                &h,
                r#"return mare.ui.completion.collect({ token = "/skill:full", start = 0, line_index = 0, force = true })"#,
            )
            .await,
        )
        .unwrap();
        let item = menu.as_array().and_then(|a| a.first().cloned()).expect("a menu item");
        assert_eq!((&item["label"], &item["hint"]), (&json!("/skill:full-spec"), &json!("[pr-number] [--draft]")));
        // /skill:<name> sends the body without the frontmatter
        exec(&h, r#"CMD:run("/skill:full-spec 42")"#).await;
        wait_until(WAIT_SECS, "the skill prompt", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"return mare.json.encode(A:list_messages().data):find("FULL-SPEC-BODY", 1, true) ~= nil"#,
                )
                .await
            }
        })
        .await;
        let sent: String = eval(&h, "return A:list_messages().data[1].content[1].text").await;
        assert!(!sent.contains("argument-hint") && sent.ends_with("42"), "{sent}");

        // the skill commands follow the commands service: a new one (/reload commands) gets them
        // again, though context (its optional dependent) does not restart
        wait_until(WAIT_SECS, "the skill prompt's turn to end", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return not A:get().busy").await }
        })
        .await;
        h.kernel.reload(Some("commands")).await.unwrap();
        global(&h, "CMD", h.service_table("commands").unwrap());
        // context's follower registers them on the new service (reading the files may yield)
        wait_until(WAIT_SECS, "/skill:<name> after /reload commands", || {
            let h = h.clone();
            async move {
                eval::<bool>(&h, r#"return CMD:get("skill:full-spec") ~= nil and CMD:get("skills") ~= nil"#).await
            }
        })
        .await;

        // `mare doctor` lists the problems and fails because skills were dropped
        let check: mlua::Value = eval(
            &h,
            r#"local checks = {} C.cordis:get("bus"):emit("kernel/doctor", checks)
               for _, c in ipairs(checks) do if c.name == "skills" then return c end end"#,
        )
        .await;
        let check = to_json(&h, check);
        assert_eq!(check["ok"], false, "{check}");
        assert!(check["detail"].as_str().unwrap().starts_with("7 loaded, 2 skipped"), "{check}");
        h.shutdown().await;
    })
    .await
}

/// The user's real skills (473 SKILL.md in ~/.agents/skills, ~/.claude/skills, ~/.harnesses/skills
/// and plugin dirs): `MARE_TEST_SKILL_CORPUS=dir[:dir…]` (`;` on Windows). Every SKILL.md below
/// those dirs goes through the context plugin; none may be dropped or read line by line for a YAML
/// error, and
/// block-scalar descriptions must come out as text, not `>` / `|`.
#[tokio::test]
async fn skill_corpus_parses_without_drops() {
    let Ok(roots) = std::env::var("MARE_TEST_SKILL_CORPUS") else {
        return; // opt-in: needs a real corpus
    };
    local(async {
        // every SKILL.md below the roots, following symlinks (each real directory once)
        let (mut files, mut seen) = (vec![], std::collections::HashSet::new());
        let mut stack: Vec<std::path::PathBuf> =
            std::env::split_paths(&roots).filter(|r| !r.as_os_str().is_empty()).collect();
        while let Some(dir) = stack.pop() {
            if !dunce::canonicalize(&dir).is_ok_and(|real| seen.insert(real)) {
                continue;
            }
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if e.file_name() == "SKILL.md" {
                    files.push(p);
                }
            }
        }
        files.sort();
        // the plugin scans <skill_dir>/<name>/SKILL.md: every parent of a skill directory is one
        let mut dirs: Vec<String> =
            files.iter().filter_map(|f| f.parent()?.parent().map(|d| d.display().to_string())).collect();
        dirs.sort();
        dirs.dedup();
        let env = TestEnv::new().await;
        let h = context_host(&env, json!({ "plugins": { "context": { "skill_dirs": dirs } } })).await;
        let skills: mlua::Value = eval(&h, "return X:skills()").await;
        let skills = to_json(&h, skills);
        let probs: mlua::Value = eval(&h, "return X:skill_problems()").await;
        let probs = to_json(&h, probs);
        let probs = probs.as_array().cloned().unwrap_or_default();
        let count = |f: &dyn Fn(&str) -> bool| probs.iter().filter(|p| f(p["message"].as_str().unwrap_or(""))).count();
        let yaml_errors = count(&|m| m.contains("YAML"));
        let no_description = count(&|m| m.contains("description is required"));
        let collisions = count(&|m| m.contains("collides"));
        let name_warnings = count(&|m| m.starts_with("name "));
        let loaded = skills.as_array().unwrap();
        let mut multi = 0;
        for f in &files {
            let raw = std::fs::read_to_string(f).unwrap_or_default();
            // `description: >` / `|` (with an optional chomping indicator) inside the frontmatter
            let block = raw.lines().skip(1).take_while(|l| l.trim_end() != "---").any(|l| {
                let v = l.strip_prefix("description:").map(str::trim);
                matches!(v, Some(">" | "|" | ">-" | "|-" | ">+" | "|+"))
            });
            if block {
                multi += 1;
                let path = f.display().to_string();
                let sk = loaded.iter().find(|s| s["path"] == path.as_str());
                if let Some(sk) = sk {
                    let d = sk["description"].as_str().unwrap();
                    assert!(d.len() > 20 && !d.starts_with('>') && !d.starts_with('|'), "{path}: {d:?}");
                }
            }
        }
        eprintln!(
            "skill corpus: {} SKILL.md in {} dirs; {} loaded; {} YAML errors, {} without description, \
             {} name collisions, {} name warnings, {} block-scalar descriptions",
            files.len(),
            dirs.len(),
            loaded.len(),
            yaml_errors,
            no_description,
            collisions,
            name_warnings,
            multi
        );
        assert_eq!(yaml_errors, 0, "{probs:?}");
        assert_eq!(no_description, 0, "{probs:?}");
        assert_eq!(loaded.len() + collisions, files.len(), "every file loads or loses a name collision");
        h.shutdown().await;
    })
    .await
}

/// Prompt templates share the YAML frontmatter reader: multi-line descriptions and
/// `argument-hint` (dim after the name in the `/` menu), unknown keys ignored silently.
#[tokio::test]
async fn prompt_templates_read_yaml_frontmatter_and_show_the_argument_hint() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("prompts");
        write(
            &dir.join("explain.md"),
            "---\ndescription: >\n  Review a file\n  carefully\nargument-hint: \"<file> [focus]\"\ntier: 1\nsurface: [cli, tui]\n---\nREVIEW $ARGUMENTS now\n",
        );
        let cfg = json!({ "plugins": { "context": { "prompt_dirs": [dir.display().to_string()] } } });
        let h = context_host(&env, cfg).await;
        let (desc, args): (String, String) =
            eval(&h, r#"local c = CMD:get("explain") return c.description, c.args"#).await;
        assert_eq!((desc.as_str(), args.as_str()), ("Review a file carefully", "<file> [focus]"));
        let menu: Value = mare_shared::lua::lua_to_json(
            h.lua(),
            eval(
                &h,
                r#"return mare.ui.completion.collect({ token = "/expl", start = 0, line_index = 0, force = true })"#,
            )
            .await,
        )
        .unwrap();
        let item = menu.as_array().and_then(|a| a.first().cloned()).expect("a menu item");
        assert_eq!(item["hint"], "<file> [focus]");
        exec(&h, r#"CMD:run("/explain src/a.rs")"#).await;
        wait_until(WAIT_SECS, "the template prompt", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #A:list_messages().data > 0").await }
        })
        .await;
        let sent: String = eval(&h, "return A:list_messages().data[1].content[1].text").await;
        assert_eq!(sent.trim(), "REVIEW src/a.rs now");
        h.shutdown().await;
    })
    .await
}

/// Codex agents_md_tests.rs / reference implementation: the budget keeps the file nearest the cwd
/// and cuts the farthest without splitting a character; BOMs go; blank files are skipped (the
/// next name is used); one file reached twice (symlink or same content) counts once; an
/// unreadable file falls back to the next name; invalid UTF-8 is replaced; a 0 budget is off.
#[tokio::test]
async fn agents_md_budget_keeps_the_nearest_file_and_skips_blank_bom_duplicate_and_unreadable_files() {
    local(async {
        let env = TestEnv::new().await;
        let root = env.cwd.clone();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("AGENTS.md"), format!("ROOT-DOC {}", "中".repeat(100)));
        write(&root.join("a/AGENTS.md"), "\u{feff}A-DOC");
        write(&root.join("a/b/AGENTS.md"), "  \n\t\n");
        write(&root.join("a/b/CLAUDE.md"), "B-CLAUDE");
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a/AGENTS.md"), root.join("a/b/c/AGENTS.md")).unwrap();
        // needs Developer Mode or an elevated token, as on CI runners
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(root.join("a/AGENTS.md"), root.join("a/b/c/AGENTS.md")).unwrap();
        std::fs::create_dir_all(root.join("a/b/c/d/AGENTS.md")).unwrap(); // unreadable: a directory
        write(&root.join("a/b/c/d/CLAUDE.md"), "D-CLAUDE");
        write(&env.config_dir().join("AGENTS.md"), b"USER\xffDOC");
        let cwd = root.join("a/b/c/d");
        // user (10 bytes after U+FFFD) + D + B + A, and 10 bytes of the root file: "ROOT-DOC "
        // plus a third of a 中 — which must not be split
        let budget = 10 + 8 + 8 + 5 + 10;
        let mut b = env
            .host()
            .builtins(&["file-ref"])
            .no_session()
            .config(json!({ "plugins": { "context": { "project_doc_max_bytes": budget } } }));
        b.opts.cwd = cwd.clone();
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let raw: mlua::LuaString = eval(&h, "return (X:system_prompt())").await;
        let prompt = std::str::from_utf8(&raw.as_bytes()).expect("valid UTF-8 after the cut").to_string();
        let at = |s: &str| prompt.find(s).unwrap_or_else(|| panic!("{s} missing: {prompt}"));
        // reading order: the user file, then root → cwd; the symlinked A-DOC counts once, at the
        // nearer of its two paths (c/AGENTS.md)
        assert!(at("USER\u{fffd}DOC") < at("ROOT-DOC") && at("ROOT-DOC") < at("B-CLAUDE"), "reading order");
        assert!(at("B-CLAUDE") < at("A-DOC") && at("A-DOC") < at("D-CLAUDE"), "root → cwd");
        assert!(prompt.contains("ROOT-DOC \n[truncated"), "cut before the split character: {prompt}");
        assert_eq!(prompt.matches("A-DOC").count(), 1, "the symlinked copy counts once");
        assert!(!prompt.contains('\u{feff}'), "BOM stripped");
        let files: Vec<String> =
            eval(&h, "local o = {} for _, f in ipairs(X:files()) do o[#o + 1] = f.path end return o").await;
        assert!(
            files.iter().any(|f| f.ends_with("a/b/CLAUDE.md")) && !files.iter().any(|f| f.ends_with("a/b/AGENTS.md"))
        );
        h.shutdown().await;

        // the budget can't hold the root file at all: it's omitted and said
        let mut b = env
            .host()
            .builtins(&["file-ref"])
            .no_session()
            .config(json!({ "plugins": { "context": { "project_doc_max_bytes": 10 + 8 + 8 + 5 } } }));
        b.opts.cwd = cwd.clone();
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt: String = eval(&h, "return (X:system_prompt())").await;
        assert!(!prompt.contains("ROOT-DOC") && prompt.contains("D-CLAUDE"), "{prompt}");
        assert!(prompt.contains("[omitted: ") && prompt.contains(&format!("{}/AGENTS.md", root.display())), "{prompt}");
        h.shutdown().await;

        // 0 turns instruction files off
        let mut b = env
            .host()
            .builtins(&["file-ref"])
            .no_session()
            .config(json!({ "plugins": { "context": { "project_doc_max_bytes": 0 } } }));
        b.opts.cwd = cwd;
        let h = b.build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let prompt: String = eval(&h, "return (X:system_prompt())").await;
        assert!(!prompt.contains("project_instructions"), "{prompt}");
        h.shutdown().await;
    })
    .await
}

/// `@file` references: quoted paths
/// with spaces attach; a missing, binary, oversized or over-budget file is said, not dropped; the
/// completion quotes paths with spaces.
#[tokio::test]
async fn context_file_refs_attach_quoted_paths_and_say_why_others_were_not() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join("my notes.txt"), "\u{feff}NOTES-CONTENT");
        write(&env.cwd.join("bin.dat"), b"\x00\x01binary");
        write(&env.cwd.join("huge.txt"), "h".repeat(300));
        write(&env.cwd.join("second.txt"), "s".repeat(190));
        let cfg = json!({ "plugins": { "file-ref": { "max_bytes": 200, "max_total_bytes": 200 } } });
        let h = env.host().builtins(&["file-ref"]).faux(json!([{ "text": "ok" }])).config(cfg).no_session().build().await;
        expose(&h);
        let items: String = eval(
            &h,
            r#"return mare.json.encode(mare.ui.completion.collect({ token = "@my", start = 0, line_index = 0, force = true }))"#,
        )
        .await;
        assert!(items.contains(r#"@\"my notes.txt\" "#), "{items}");
        h.prompt(r#"see @"my notes.txt", @missing.txt @bin.dat @huge.txt @second.txt"#, json!({})).await.unwrap();
        let text: String = eval(&h, "return mare.json.encode(A:list_messages().data[1].content)").await;
        assert!(text.contains(r#"<file path=\"my notes.txt\">\nNOTES-CONTENT"#), "{text}");
        assert!(text.contains("[@missing.txt not attached: no such file"), "{text}");
        assert!(text.contains("[@bin.dat not attached: binary file"), "{text}");
        assert!(text.contains("[@huge.txt not attached: 300 bytes, over the 200-byte limit"), "{text}");
        assert!(
            text.contains("[@second.txt not attached: the attachments of this message would exceed 200 bytes"),
            "{text}"
        );
        h.shutdown().await;
    })
    .await
}

/// The `@` completion index is built on first use and then follows file changes without a
/// full rescan: a file created afterwards completes, a deleted one
/// no longer does.
#[tokio::test]
async fn at_completion_follows_new_and_deleted_files() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join("src/old_module.rs"), "");
        let cfg = json!({ "plugins": { "file-ref": { "refresh_ms": 0 } } });
        let h = env.host().builtins(&["file-ref"]).config(cfg).no_session().build().await;
        let complete = |q: &'static str| {
            let h = h.clone();
            async move {
                let code = format!(
                    r#"return mare.json.encode(mare.ui.completion.collect({{ token = {:?}, start = 0, line_index = 0, force = true }}))"#,
                    q
                );
                eval::<String>(&h, &code).await
            }
        };
        assert!(complete("@old_mod").await.contains("src/old_module.rs"));
        write(&env.cwd.join("src/new_module.rs"), "");
        std::fs::remove_file(env.cwd.join("src/old_module.rs")).unwrap();
        let items = complete("@_module").await;
        assert!(items.contains("src/new_module.rs"), "{items}");
        assert!(!items.contains("src/old_module.rs"), "{items}");
        h.shutdown().await;
    })
    .await
}

/// Even when the built-in `permissions` service is disabled, `file-ref` must still invoke and
/// obey `tool/before` policy hooks (deny / ask / error / extra sandbox).
#[tokio::test]
async fn context_file_refs_obey_tool_before_when_permissions_disabled() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join("denied.txt"), "DENIED-SECRET");
        write(&env.cwd.join("asked.txt"), "ASKED-SECRET");
        write(&env.cwd.join("errored.txt"), "ERRORED-SECRET");
        write(&env.cwd.join("sandboxed.txt"), "SANDBOXED-SECRET");
        write(&env.cwd.join("allowed.txt"), "ALLOWED-CONTENT");
        let h = env.host().builtins(&["file-ref"]).faux(json!([{ "text": "ok" }])).no_session().build().await;
        expose(&h);
        let has_perms: bool = eval(&h, r#"return C.cordis:get("permissions") ~= nil"#).await;
        assert!(!has_perms, "TestEnv::host() should disable permissions");
        exec(
            &h,
            r#"
            C.cordis:get("policy"):voter("tool/before", function(call)
                local p = call.arguments and call.arguments.path or ""
                if p:find("denied.txt", 1, true) then
                    return { decision = "deny", reason = "blocked by custom policy" }
                elseif p:find("asked.txt", 1, true) then
                    return { decision = "ask", reason = "approval required by custom policy" }
                elseif p:find("errored.txt", 1, true) then
                    error("policy hook boom")
                elseif p:find("sandboxed.txt", 1, true) or p:find("allowed.txt", 1, true) then
                    return {
                        decision = "allow",
                        sandbox = { level = "workspace-write", deny_read = { "sandboxed.txt" } },
                    }
                end
            end)
            "#,
        )
        .await;
        h.prompt(r#"check @denied.txt @asked.txt @errored.txt @sandboxed.txt @allowed.txt"#, json!({})).await.unwrap();
        let text: String = eval(&h, "return mare.json.encode(A:list_messages().data[1].content)").await;
        assert!(!text.contains("DENIED-SECRET"), "deny hook leaked content: {text}");
        assert!(!text.contains("ASKED-SECRET"), "ask hook leaked content: {text}");
        assert!(!text.contains("ERRORED-SECRET"), "erroring hook leaked content: {text}");
        assert!(!text.contains("SANDBOXED-SECRET"), "extra sandbox hook leaked content: {text}");
        assert!(text.contains("is not readable"), "expected sandbox read denial for @sandboxed.txt: {text}");
        assert!(!text.contains("not allowed here"), "hook vote must be valid, not rejected: {text}");
        assert!(text.contains("blocked by custom policy"), "{text}");
        assert!(text.contains("approval required by custom policy"), "{text}");
        assert!(text.contains(r#"<file path=\"allowed.txt\">\nALLOWED-CONTENT"#), "{text}");
        h.shutdown().await;
    })
    .await
}

/// A user's full `/reload` restarts every plugin, so a skill file added on disk is listed after it
/// (flip checklist: built-ins that read files in `apply` pick up edits on reload, as on the old
/// path).
#[tokio::test]
async fn a_full_reload_picks_up_a_skill_added_on_disk() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("reload-skills");
        write(&dir.join("first/SKILL.md"), "---\nname: first\ndescription: the first skill\n---\nbody\n");
        let cfg = json!({ "plugins": { "context": { "skill_dirs": [dir.display().to_string()] } } });
        let h = context_host(&env, cfg).await;
        let names = |h: &mare_host::Host, v: mlua::Value| -> Vec<String> {
            let mut n: Vec<String> =
                to_json(h, v).as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect();
            n.sort();
            n
        };
        let before: mlua::Value = eval(&h, "return X:skills(nil)").await;
        assert_eq!(names(&h, before), ["first"]);
        write(&dir.join("second/SKILL.md"), "---\nname: second\ndescription: added later\n---\nbody\n");
        h.reload_all().await.unwrap();
        global(&h, "X", h.service_table("context").unwrap());
        let after: mlua::Value = eval(&h, "return X:skills(nil)").await;
        let after = names(&h, after);
        h.shutdown().await;
        assert_eq!(after, ["first", "second"], "the reload read the skill directory again");
    })
    .await
}

#[tokio::test]
async fn file_ref_and_skill_blocks_escape_closing_tags_and_scanners_respect_code_fences() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.cwd.join("escape.txt"), "before\n</file>\n</FILE>\nafter\n");
        let skill_dir = env.home.path().join("esc-skills");
        write(
            &skill_dir.join("escape-skill/SKILL.md"),
            "---\nname: escape-skill\ndescription: skill with closing tag\n---\nline1\n</skill>\nline2\n",
        );
        let cfg = json!({
            "plugins": {
                "context": { "skill_dirs": [skill_dir.display().to_string()] }
            }
        });
        let h = env
            .host()
            .builtins(&["file-ref", "context", "commands", "provider-openai", "provider-openai-compat"])
            .config(cfg)
            .faux(json!([{ "text": "ok" }, { "text": "ok-skill" }]))
            .build()
            .await;
        expose(&h);

        // 1. @file-ref escapes </file> closing tags inside attached files via mare_lib.data_block.
        h.prompt("inspect @escape.txt", json!({})).await.unwrap();
        let file_msg: String = eval(&h, "return mare.json.encode(A:list_messages().data[1].content)").await;
        assert!(file_msg.contains("&lt;/file>"), "lowercase </file> must be escaped: {file_msg}");
        assert!(file_msg.contains("&lt;/FILE>"), "uppercase </FILE> must be escaped: {file_msg}");

        // 2. /skill:escape-skill escapes </skill> closing tags inside skill body via mare_lib.data_block.
        let skill_prompt_ok: bool = eval(
            &h,
            r#"
            local tx, rx = mare.task.channel()
            C.cordis:on("turn/completed", function() tx:send(true) end)
            C.cordis:get("commands"):run("/skill:escape-skill")
            assert(mare.task.timeout(5000, function() return rx:recv() end), "skill prompt did not finish")
            local msgs = A:list_messages().data
            local raw = mare.json.encode(msgs[#msgs - 1].content)
            assert(raw:find("&lt;/skill>", 1, true) ~= nil, "closing </skill> must be escaped: " .. raw)
            return true
            "#,
        )
        .await;
        assert!(skill_prompt_ok);

        // 3. Qwen and Kimi streaming markup healers ignore tool-call markup inside ``` / ~~~ / ` code fences.
        let healing_ok: bool = eval(
            &h,
            r#"
            local mh = require("provider-openai-compat.markup_healing")
            local function scan(dialect, chunks)
                local sc = mh.new_scanner(dialect)
                local tools, texts = {}, {}
                for _, ch in ipairs(chunks) do
                    for _, ev in ipairs(sc:feed(ch)) do
                        if ev.type == "tool_call" then tools[#tools + 1] = ev else texts[#texts + 1] = ev.text end
                    end
                end
                for _, ev in ipairs(sc:flush()) do
                    if ev.type == "tool_call" then tools[#tools + 1] = ev else texts[#texts + 1] = ev.text end
                end
                return tools, table.concat(texts)
            end

            -- Inside backtick fence: Qwen <tool_call> must stay text, not synthesize a tool_call.
            local qt1, qtxt1 = scan("qwen", {
                "Example:\n```xml\n",
                '<tool_call>{"name":"bash","arguments":{"command":"id"}}</tool_call>\n',
                "```\nDone.",
            })
            assert(#qt1 == 0, "qwen scanner must not heal inside ``` code fence")
            assert(qtxt1:find("<tool_call>", 1, true) ~= nil, "fenced qwen markup must remain literal text")

            -- Inside tilde fence: Qwen <tool_call> must also stay text.
            local qt2 = scan("qwen", {
                "~~~\n<tool_call>{\"name\":\"bash\",\"arguments\":{}}</tool_call>\n~~~\n",
            })
            assert(#qt2 == 0, "qwen scanner must not heal inside ~~~ code fence")

            -- Outside fence after closing fence: Qwen heals normally.
            local qt3 = scan("qwen", {
                "```\n<tool_call>{\"name\":\"ignored\",\"arguments\":{}}</tool_call>\n```\n",
                '<tool_call>{"name":"read","arguments":{"path":"a.txt"}}</tool_call>',
            })
            assert(#qt3 == 1 and qt3[1].name == "read", "qwen scanner must still heal outside code fence")

            -- Inside backtick fence: Kimi section tokens must stay text.
            local kt1, ktxt1 = scan("kimi", {
                "```txt\n<|tool_calls_section_begin|><|tool_call_begin|>functions.bash:0<|tool_call_argument_begin|>{}<|tool_call_end|><|tool_calls_section_end|>\n```\n",
            })
            assert(#kt1 == 0, "kimi scanner must not heal inside ``` code fence")
            assert(ktxt1:find("<|tool_calls_section_begin|>", 1, true) ~= nil)

            -- Outside fence: Kimi heals normally.
            local kt2 = scan("kimi", {
                "<|tool_calls_section_begin|><|tool_call_begin|>functions.read:0<|tool_call_argument_begin|>{\"path\":\"x\"}<|tool_call_end|><|tool_calls_section_end|>",
            })
            assert(#kt2 == 1 and kt2[1].name == "read", "kimi scanner must heal outside code fence")

            -- 4. Custom Responses provider with transport="websocket" is recognized as a WebSocket target.
            local ws_called = false
            local orig_ws = mare.http.websocket
            mare.http.websocket = function()
                ws_called = true
                error("ws-probe-reached")
            end
            local ev = {
                provider = "custom-responses",
                endpoint = { provider = "custom-responses", transport = "websocket" },
                transport = "websocket",
                request = { url = "https://custom.example/v1/responses", headers = {}, body = {} },
            }
            pcall(function()
                C.cordis:get("bus"):bail("provider/transport", ev)
            end)
            mare.http.websocket = orig_ws
            assert(ws_called == true, "custom responses provider with transport=websocket must invoke websocket transport")
            return true
            "#,
        )
        .await;
        assert!(healing_ok);

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cfg_root_loads_or_skips_system_skills_and_prompts_from_resource_root() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub/dir");
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        write(&repo.join(".mare/SYSTEM.md"), "REPO-ROOT-SYSTEM");
        write(&repo.join(".mare/skills/s1/SKILL.md"), "---\nname: s1\ndescription: root skill\n---\nbody\n");
        write(&repo.join(".agents/skills/s-agent/SKILL.md"), "---\nname: s-agent\ndescription: agent root skill\n---\nbody\n");
        write(&sub.join(".agents/skills/s-subdir/SKILL.md"), "---\nname: s-subdir\ndescription: subdir skill\n---\nbody\n");
        write(&repo.join(".mare/prompts/p1.md"), "Root prompt $1");
        write(&repo.join("team-prompts/team.md"), "Team prompt $1");
        mare_config::trust::set_dir(&repo, true).unwrap();

        for (trust, expect) in [("trusted", true), ("untrusted", false)] {
            let mut b = env
                .host()
                .faux(json!([]))
                .no_session()
                .config(json!({ "plugins": { "context": { "prompt_dirs": [".mare/prompts", "team-prompts"] } } }))
                .args(json!({ "no_session": true, "trust": trust }));
            b.opts.cwd = sub.clone();
            b.opts.project_trusted = trust == "trusted";
            let h = b.build().await;
            expose(&h);
            global(&h, "X", h.service_table("context").unwrap());
            global(&h, "CMD", h.service_table("commands").unwrap());

            let (prompt, has_skill, has_agent_skill, has_subdir_skill, has_prompt, has_team_prompt): (
                String,
                bool,
                bool,
                bool,
                bool,
                bool,
            ) = eval(
                &h,
                r#"local skill, agent_skill, subdir_skill = false, false, false
                   for _, s in ipairs(X:skills()) do
                       skill = skill or s.name == "s1"
                       agent_skill = agent_skill or s.name == "s-agent"
                       subdir_skill = subdir_skill or s.name == "s-subdir"
                   end
                   return (X:system_prompt()), skill, agent_skill, subdir_skill, CMD:get("p1") ~= nil, CMD:get("team") ~= nil"#,
            )
            .await;
            assert_eq!(prompt == "REPO-ROOT-SYSTEM", expect, "{trust}: SYSTEM.md from resource root: {prompt}");
            assert_eq!(has_skill, expect, "{trust}: skill s1 from resource root");
            assert_eq!(has_agent_skill, expect, "{trust}: .agents/skills from resource root");
            assert!(!has_subdir_skill, "{trust}: subdir .agents/skills must not override resource root");
            assert_eq!(has_prompt, expect, "{trust}: prompt /p1 from resource root");
            assert_eq!(has_team_prompt, expect, "{trust}: relative prompt /team from resource root");
            h.shutdown().await;
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn cfg_root_lua_skips_rejected_mare_symlink_even_with_sibling_mcp() {
    local(async {
        let env = TestEnv::new().await;
        let repo = dunce::canonicalize(&env.cwd).unwrap();
        let sub = repo.join("sub");
        let evil = repo.join("evil");
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        std::fs::create_dir_all(repo.join(".git/refs")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        write(&sub.join(".mcp.json"), r#"{ "mcpServers": { "good_top": { "command": "true" } } }"#);
        write(&evil.join("SYSTEM.md"), "EVIL-SYSTEM");
        write(&evil.join("skills/bad-skill/SKILL.md"), "---\nname: bad-skill\ndescription: evil\n---\nbody\n");
        write(&evil.join("prompts/bad-prompt.md"), "evil prompt");
        write(&evil.join("mcp.json"), r#"{ "mcpServers": { "evil_dot": { "command": "true" } } }"#);
        std::os::unix::fs::symlink(&evil, sub.join(".mare")).unwrap();
        mare_config::trust::set_dir(&sub, true).unwrap();

        let mut b = env
            .host()
            .builtins(&["mcp"])
            .faux(json!([]))
            .no_session()
            .args(json!({ "no_session": true, "trust": "trusted" }));
        b.opts.cwd = sub.clone();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "X", h.service_table("context").unwrap());
        global(&h, "CMD", h.service_table("commands").unwrap());

        let (sys, has_bad_skill, has_bad_prompt, mcp_listing): (String, bool, bool, String) = eval(
            &h,
            r#"local bad = false
               for _, s in ipairs(X:skills()) do bad = bad or s.name == "bad-skill" end
               local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return (X:system_prompt()), bad, CMD:get("bad-prompt") ~= nil, table.concat(req.lines, "\n")"#,
        )
        .await;
        assert_ne!(sys, "EVIL-SYSTEM", "rejected .mare/SYSTEM.md must not load: {sys}");
        assert!(!has_bad_skill, "rejected .mare/skills must not load");
        assert!(!has_bad_prompt, "rejected .mare/prompts must not load");
        assert!(mcp_listing.contains("good_top"), "valid .mcp.json must still load: {mcp_listing}");
        assert!(!mcp_listing.contains("evil_dot"), "rejected .mare/mcp.json must not load: {mcp_listing}");
        h.shutdown().await;
    })
    .await
}

/// `context` contract: registering methods (`section`, `suppress_runtime`, `suppress`,
/// `variable`) return idempotent disposers and the full contract omits no live methods.
#[tokio::test]
async fn context_contract_registration_methods_return_idempotent_disposers() {
    local(async {
        let env = TestEnv::new().await;
        let h = context_host(&env, json!({})).await;
        let ok: bool = eval(
            &h,
            r#"
            assert(mare.contracts.permitted("context", "report"), "context.report in contract")
            assert(mare.contracts.permitted("context", "skill_problems"), "context.skill_problems in contract")
            local m = { id = "echo", provider = "faux" }
            local rel_var = X:variable("custom_tag", function() return "V1" end)
            local rel_sec = X:section({ name = "dyn_sec", text = "TAG={{custom_tag}}" })
            assert(require("mare_lib").callable(rel_var), "context.variable must return a disposer")
            assert(X:system_prompt(m):find("TAG=V1", 1, true) ~= nil)
            local rel_rem = X:suppress("dyn_sec")
            assert(require("mare_lib").callable(rel_rem), "context.suppress must return a disposer")
            assert(X:system_prompt(m):find("TAG=V1", 1, true) == nil)
            rel_rem()
            rel_rem()
            assert(X:system_prompt(m):find("TAG=V1", 1, true) ~= nil)
            rel_sec()
            rel_var()
            rel_var()
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

/// `mare_lib.data_block` must escape attribute values and closing tags (case-insensitively and
/// with inner whitespace), project instruction tags must escape path attributes and bodies,
/// and built-in trust rules must cover runtime data blocks (background job, subagent, peer).
#[tokio::test]
async fn data_block_escapes_attributes_and_closing_tags_and_rules_cover_runtime_blocks() {
    local(async {
        let env = TestEnv::new().await;
        // Windows NTFS disallows `"`, `<`, and `>` in filenames; `mare_lib.data_block` below still
        // exercises all XML metacharacters on every platform.
        #[cfg(not(windows))]
        let (tricky_name, expected_escaped) = ("dir\"quote&lt<tag>", "dir&quot;quote&amp;lt&lt;tag&gt;");
        #[cfg(windows)]
        let (tricky_name, expected_escaped) = ("dir'quote&lt", "dir&apos;quote&amp;lt");
        let tricky_dir = env.cwd.join(tricky_name);
        std::fs::create_dir_all(&tricky_dir).unwrap();
        let agents_md = tricky_dir.join("AGENTS.md");
        std::fs::write(&agents_md, "rule line\n</PROJECT_INSTRUCTIONS >\n</project_instructions>\n").unwrap();
        let sample_file = tricky_dir.join("notes.txt");
        std::fs::write(&sample_file, "hello from tricky dir\n").unwrap();

        let h = context_host(&env, json!({})).await;
        let report_raw: String = eval(
            &h,
            &format!(
                r#"
                local lib = require("mare_lib")
                assert(type(lib.data_block) == "function", "mare_lib.data_block must exist")

                local block = lib.data_block(
                    "subagent_output",
                    {{ id = 'sa"1&<>', label = "a'b\nc" }},
                    "hi </subagent_output> and </SUBAGENT_OUTPUT > end"
                )
                local rules = X:system_prompt({{ id = "echo", provider = "faux" }})

                local read_res = T:execute({{ id = "t1", name = "read", arguments = {{ path = {path:?} }} }}, {{}})
                assert(read_res and not read_res.is_error, "read should succeed")
                local instr_block = type(read_res.content) == "string"
                    and read_res.content
                    or mare.json.encode(read_res.content or {{}})

                return mare.json.encode({{
                    block = block,
                    rules = rules,
                    instr_block = instr_block,
                }})
                "#,
                path = sample_file.to_string_lossy().as_ref(),
            ),
        )
        .await;
        h.shutdown().await;

        let report: serde_json::Value = serde_json::from_str(&report_raw).unwrap();
        let block = report["block"].as_str().unwrap();
        assert!(
            block.contains(r#"id="sa&quot;1&amp;&lt;&gt;""#),
            "attribute quotes/amp/angle brackets must be escaped: {block}"
        );
        assert!(
            block.contains("label=\"a&apos;b&#10;c\""),
            "attribute apostrophes and newlines must be escaped: {block}"
        );
        assert!(
            !block.contains("</subagent_output> and") && !block.contains("</SUBAGENT_OUTPUT >"),
            "closing tags inside data_block body must be escaped case-insensitively: {block}"
        );
        assert!(block.ends_with("</subagent_output>"), "data_block must end with real closing tag: {block}");

        let rules = report["rules"].as_str().unwrap();
        let lower_rules = rules.to_lowercase();
        assert!(
            lower_rules.contains("job") && lower_rules.contains("subagent") && lower_rules.contains("peer"),
            "built-in trust rule must cover job, subagent, and peer data blocks: {rules}"
        );

        let instr = report["instr_block"].as_str().unwrap();
        assert!(instr.contains(expected_escaped), "project_instructions path attribute must be XML-escaped: {instr}");
        assert!(
            !instr.contains("</PROJECT_INSTRUCTIONS >"),
            "project_instructions body closing tag must be escaped: {instr}"
        );
    })
    .await
}
