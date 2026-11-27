//! Artifact Registry, Contract Tests, Closure Verification, and SPEC Table Check.

use crate::common::*;
use crate::suite::mcp::Fake;
use crate::suite::packages::package_repo;
use mare_shared::artifact::ArtifactId;
use mare_shared::paths::WriteMode;
use std::path::Path;

mod version_tests;

#[cfg(unix)]
fn assert_file_mode(path: &Path, expected: WriteMode) {
    use std::os::unix::fs::PermissionsExt;
    let perm =
        std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display())).permissions().mode()
            & 0o777;
    let want = expected.unix_mode();
    assert_eq!(perm, want, "file {} has mode {perm:04o}, expected {want:04o}", path.display());
}

#[cfg(unix)]
fn assert_dir_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perm =
        std::fs::symlink_metadata(dir).unwrap_or_else(|e| panic!("stat {}: {e}", dir.display())).permissions().mode()
            & 0o777;
    assert_eq!(perm, 0o700, "dir {} has mode {perm:04o}, expected 0700", dir.display());
}

#[cfg(not(unix))]
fn assert_file_mode(path: &Path, _expected: WriteMode) {
    mare_shared::private::check(path).unwrap();
}

#[cfg(not(unix))]
fn assert_dir_private(dir: &Path) {
    mare_shared::private::check(dir).unwrap();
}

fn find_first_matching(dir: &Path, suffix: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(hit) = find_first_matching(&p, suffix) {
                return Some(hit);
            }
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(suffix)) {
            return Some(p);
        }
    }
    None
}

fn session_header(id: &str, cwd: &Path, version: u32) -> serde_json::Value {
    serde_json::json!({
        "type": "session",
        "version": version,
        "session_id": id,
        "cwd": cwd,
        "created_at_ms": 1767225600000i64
    })
}

/// Verify `mare_host::artifacts::all()` integrity (unique ids/names, valid fields, and rendered rows).
#[test]
fn artifacts_registry_integrity_and_table_rendering() {
    let items = mare_host::artifacts::all();
    assert!(!items.is_empty(), "artifacts registry must not be empty");
    let mut ids = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for a in &items {
        assert!(ids.insert(a.id), "duplicate ArtifactId {:?}", a.id);
        let name = a.id.as_str();
        assert!(!name.is_empty() && names.insert(name), "invalid/duplicate artifact name {name:?}");
        assert!(!a.pattern.trim().is_empty(), "{name}: empty pattern");
        assert!(!a.owner.trim().is_empty(), "{name}: empty owner");
    }
    let rendered = mare_host::artifacts::render_spec_table();
    let data_rows: Vec<&str> =
        rendered.lines().filter(|l| l.starts_with('|') && !l.starts_with("| ID |") && !l.starts_with("|---")).collect();
    assert_eq!(data_rows.len(), items.len(), "rendered table row count mismatch");
    for a in &items {
        let col = format!("`{}`", a.id.as_str());
        assert!(data_rows.iter().any(|row| row.contains(&col)), "rendered table missing artifact {}", a.id.as_str());
    }
}

#[tokio::test]
async fn assert_registered_rejects_unregistered_file_and_wrong_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    mare_shared::private::create_dir_all(&home.join("state")).unwrap();
    mare_shared::paths::write_atomic(&home.join("state/trust.json"), b"{}", WriteMode::Private).unwrap();
    assert_registered(home);

    // Unregistered stray file fails the closure check.
    let rogue = home.join("state/rogue-unregistered.json");
    mare_shared::paths::write_atomic(&rogue, b"{}", WriteMode::Private).unwrap();
    let err = mare_host::artifacts::assert_registered(home).unwrap_err();
    assert!(err.contains("unregistered file State:rogue-unregistered.json"), "{err}");
    std::fs::remove_file(&rogue).unwrap();

    // Wrong permission mode on a registered private artifact fails the closure check on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let trust = home.join("state/trust.json");
        std::fs::set_permissions(&trust, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = mare_host::artifacts::assert_registered(home).unwrap_err();
        assert!(err.contains("has mode 0644 (expected 0600)"), "{err}");
    }
}

/// contract dispatcher: exhaustive `match` over `ArtifactId` guarantees every registered row has
/// a production-path contract test checking permissions, version handling, and corruption policy.
async fn verify_contract(id: ArtifactId) {
    let meta = mare_host::artifacts::get(id).expect("artifact registered");
    match id {
        ArtifactId::SessionJournal => {
            let env = TestEnv::new().await;
            let dir = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(&env.cwd));
            mare_store::private_dir(&dir).unwrap();
            let path = dir.join("c1-j1.jsonl");
            let mut s = mare_store::SessionFile::create_new(&path).unwrap();
            s.append_batch(&[session_header("c1-j1", &env.cwd, mare_store::CURRENT_VERSION as u32)], false).unwrap();
            assert_dir_private(path.parent().unwrap());
            assert_file_mode(&path, meta.mode);
            s.append_batch(&[serde_json::json!({"type": "name", "name": "ok"})], false).unwrap();
            drop(s);

            // Corrupt middle line -> CORRUPT, bytes preserved, refuses open/append.
            let orig = std::fs::read(&path).unwrap();
            let mut bad = orig.clone();
            bad.extend_from_slice(b"{not-json\n");
            std::fs::write(&path, &bad).unwrap();
            let err = mare_store::SessionFile::open_existing(&path, None).err().unwrap().to_string();
            assert!(err.contains(mare_store::CORRUPT), "{err}");
            assert_eq!(std::fs::read(&path).unwrap(), bad, "corrupt journal untouched");

            // Unsupported header version -> UNSUPPORTED_VERSION, bytes preserved.
            let ver_bad = session_header("c1-j2", &env.cwd, 999).to_string() + "\n";
            let p2 = dir.join("badver.jsonl");
            std::fs::write(&p2, &ver_bad).unwrap();
            let err = mare_store::SessionFile::open_existing(&p2, None).err().unwrap().to_string();
            assert!(err.contains(mare_store::UNSUPPORTED_VERSION), "{err}");
            assert_eq!(std::fs::read(&p2).unwrap(), ver_bad.as_bytes());
        }
        ArtifactId::SessionLock => {
            let env = TestEnv::new().await;
            let dir = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(&env.cwd));
            mare_store::private_dir(&dir).unwrap();
            let path = dir.join("c1-lock.jsonl");
            let mut s = mare_store::SessionFile::create_new(&path).unwrap();
            s.append_batch(&[session_header("c1-lock", &env.cwd, mare_store::CURRENT_VERSION as u32)], false).unwrap();
            let lock_p = mare_store::lock_path(&path);
            assert_file_mode(&lock_p, meta.mode);
            let err = mare_store::SessionFile::open_existing(&path, None).err().unwrap().to_string();
            assert!(err.contains(mare_store::OPEN_HERE) || err.contains(mare_store::LOCKED), "{err}");
            drop(s);
        }
        ArtifactId::SessionRollback => {
            let env = TestEnv::new().await;
            let dir = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(&env.cwd));
            mare_store::private_dir(&dir).unwrap();
            let path = dir.join("c1-rb.jsonl");
            let mut s = mare_store::SessionFile::create_new(&path).unwrap();
            s.append_batch(&[session_header("c1-rb", &env.cwd, mare_store::CURRENT_VERSION as u32)], true).unwrap();
            let rb = mare_store::rollback_path(&path);
            drop(s);
            // Corrupt rollback marker is never silently discarded.
            mare_shared::paths::write_atomic(&rb, b"CORRUPT_ROLLBACK", meta.mode).unwrap();
            assert_file_mode(&rb, meta.mode);
            let err = mare_store::SessionFile::open_existing(&path, None).err().unwrap().to_string();
            assert!(err.contains("rollback") || err.contains("invalid"), "{err}");
            assert_eq!(std::fs::read(&rb).unwrap(), b"CORRUPT_ROLLBACK");
        }
        ArtifactId::SessionCommit => {
            let env = TestEnv::new().await;
            let dir = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(&env.cwd));
            mare_store::private_dir(&dir).unwrap();
            let path = dir.join("c1-cm.jsonl");
            let mut s = mare_store::SessionFile::create_new(&path).unwrap();
            s.append_batch(
                &[
                    session_header("c1-cm", &env.cwd, mare_store::CURRENT_VERSION as u32),
                    serde_json::json!({"type": "name", "name": "durable"}),
                ],
                true,
            )
            .unwrap();
            let cm = mare_store::commit_path(&path);
            assert!(cm.exists(), "commit marker written on durable sync");
            assert_file_mode(&cm, meta.mode);
            drop(s);

            // Corrupt commit marker is rejected and bytes are preserved until cleared/rebuilt cleanly.
            std::fs::write(&cm, b"BAD").unwrap();
            let err = mare_store::SessionFile::open_existing(&path, None).err().unwrap().to_string();
            assert!(err.contains("commit marker") || err.contains("invalid"), "{err}");
            assert_eq!(std::fs::read(&cm).unwrap(), b"BAD");
        }
        ArtifactId::SessionIndex => {
            let env = TestEnv::new().await;
            let sessions_dir = mare_shared::paths::sessions_dir();
            let dir = sessions_dir.join(mare_store::cwd_slug(&env.cwd));
            mare_store::private_dir(&dir).unwrap();
            let path = dir.join("20260101_000000_c1-idx.jsonl");
            let mut s = mare_store::SessionFile::create_new(&path).unwrap();
            s.append_batch(
                &[
                    session_header("c1-idx", &env.cwd, mare_store::CURRENT_VERSION as u32),
                    serde_json::json!({"type": "name", "name": "indexed"}),
                ],
                false,
            )
            .unwrap();
            drop(s);
            let shared = mare_store::index::shared(&sessions_dir).unwrap();
            let res = mare_store::index::reconcile(&shared).unwrap();
            assert!(res.updated >= 1 || res.files >= 1);
            let db = sessions_dir.join(mare_store::index::DB_NAME);
            assert!(db.is_file(), "index.sqlite created");
            assert_dir_private(&sessions_dir);
            assert_file_mode(&db, meta.mode);
            let lock_p = sessions_dir.join("index.sqlite.lock");
            assert!(lock_p.is_file(), "index.sqlite.lock created");
            assert_file_mode(&lock_p, meta.mode);
            for suffix in ["index.sqlite-wal", "index.sqlite-shm"] {
                let sidecar = sessions_dir.join(suffix);
                if sidecar.exists() {
                    assert_file_mode(&sidecar, meta.mode);
                }
            }
            drop(shared);
            mare_store::index::close_idle(&sessions_dir).unwrap();

            // Inject garbage SQLite bytes -> quarantined as `index.sqlite.bad-*` and rebuilt.
            std::fs::write(&db, b"not a database, just some text that is long enough to have a header").unwrap();
            let shared2 = mare_store::index::shared(&sessions_dir).unwrap();
            let res2 = mare_store::index::reconcile(&shared2).unwrap();
            assert!(res2.files >= 1);
            drop(shared2);
            mare_store::index::close_idle(&sessions_dir).unwrap();
            let has_bad = std::fs::read_dir(&sessions_dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("index.sqlite.bad-"));
            assert!(has_bad, "corrupt sqlite index quarantined to .bad-*");
        }
        ArtifactId::SessionAttachment => {
            let env = TestEnv::new().await;
            let cfg = serde_json::json!({ "plugins": { "compact": { "stages": ["offload"] } } });
            let h = env.host().builtins(&["compact", "usage"]).faux(serde_json::json!([])).config(cfg).build().await;
            expose(&h);
            exec(
                &h,
                r#"local msgs = {}
                   for i = 1, 4 do
                       msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = "img " .. i },
                           { type = "image", mime = "image/png", data = mare.crypto.base64_encode("PNG-" .. i) } } }
                       msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "ok" } } }
                   end
                   A:set_messages(msgs)"#,
            )
            .await;
            global(&h, "K", h.service_table("compact").unwrap());
            let ok: bool =
                eval(&h, r#"return C.cordis:get("bus"):waterfall("compact/overflow", {}).res.status == "compacted""#)
                    .await;
            assert!(ok);
            h.shutdown().await;
            let att = find_first_matching(&env.home.path().join("state"), ".bin")
                .or_else(|| find_first_matching(&env.home.path().join("state"), ".png"))
                .expect("attachment offloaded");
            assert_dir_private(att.parent().unwrap());
            assert_file_mode(&att, meta.mode);
        }
        ArtifactId::SessionSnapshot => {
            let env = TestEnv::new().await;
            std::fs::write(env.cwd.join("snap.txt"), "before\n").unwrap();
            let script = serde_json::json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "snap.txt" } }] },
                { "tool_calls": [{ "name": "edit", "arguments": { "path": "snap.txt", "old_string": "before", "new_string": "after" } }] },
                { "text": "done" }
            ]);
            let h = env.host().builtins(&["changes"]).faux(script).build().await;
            let res = h.prompt("edit snap.txt", serde_json::json!({})).await.unwrap();
            assert!(res["error"].is_null(), "{res}");
            h.shutdown().await;
            let idx = find_first_matching(&env.home.path().join("state/sessions"), "index.jsonl")
                .expect("snapshot index.jsonl written");
            assert_dir_private(idx.parent().unwrap());
            assert_file_mode(&idx, meta.mode);
            let blobs_dir = idx.parent().unwrap().join("blobs");
            assert_dir_private(&blobs_dir);
            let blob = std::fs::read_dir(&blobs_dir).unwrap().flatten().next().unwrap().path();
            assert_file_mode(&blob, meta.mode);
        }
        ArtifactId::StateFile => {
            let env = TestEnv::new().await;
            let h = env.host().no_session().build().await;
            expose(&h);
            exec(&h, r#"mare.state.set("c1_key", "c1_val")"#).await;
            let state_p = env.home.path().join("state/state.json");
            assert_file_mode(&state_p, meta.mode);
            assert_dir_private(state_p.parent().unwrap());
            let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&state_p).unwrap()).unwrap();
            assert_eq!(meta.version, Some(1));
            assert_eq!(saved["version"], 1);
            assert_eq!(saved["c1_key"], "c1_val");
            let reserved: bool = eval(
                &h,
                r#"return not pcall(mare.state.set, "version", 2)
                and not pcall(mare.state.update, function(s) s.version = 2 end)"#,
            )
            .await;
            assert!(reserved, "state updates must not replace the format version");
            let saved_again: serde_json::Value = serde_json::from_slice(&std::fs::read(&state_p).unwrap()).unwrap();
            assert_eq!(saved_again, saved);

            for bad in ["{broken", "[1, 2, 3]"] {
                std::fs::write(&state_p, bad).unwrap();
                let (ok, err): (bool, String) =
                    eval(&h, "local v, e = mare.state.read(); return v ~= nil, tostring(e)").await;
                assert!(!ok && err.contains("corrupt"), "{bad}: {err}");
                let upd_ok: bool = eval(&h, "return pcall(mare.state.set, 'k2', 'v2')").await;
                assert!(!upd_ok, "corrupt state.json must refuse updates ({bad})");
                assert_eq!(std::fs::read_to_string(&state_p).unwrap(), bad, "bytes untouched");
            }
            h.shutdown().await;
        }
        ArtifactId::NamespaceState => {
            let env = TestEnv::new().await;
            let h = env.host().no_session().build().await;
            expose(&h);
            let (ok, rev): (bool, i64) = eval(
                &h,
                r#"local r = mare.state.compare_exchange({ namespace = "c1.ns", expected_revision = 0, value = { n = 1 } })
                   return r.status == "ok", r.snapshot.revision"#,
            )
            .await;
            assert!(ok && rev == 1);
            let ns_file = find_first_matching(&env.home.path().join("state/domains"), ".json").unwrap();
            assert_dir_private(ns_file.parent().unwrap());
            assert_file_mode(&ns_file, meta.mode);

            for (bad, want) in
                [("{broken", "corrupt"), ("[1]", "corrupt"), (r#"{"version":99}"#, "unsupported_version")]
            {
                std::fs::write(&ns_file, bad).unwrap();
                let (res_ok, code): (bool, String) = eval(
                    &h,
                    r#"local r = mare.state.snapshot({ namespace = "c1.ns" }); return r.status == "ok", tostring(r.error and r.error.code)"#,
                )
                .await;
                assert!(!res_ok && code == want, "{bad}: got {code}");
                let cas_ok: bool = eval(
                    &h,
                    r#"return mare.state.compare_exchange({ namespace = "c1.ns", expected_revision = 1, value = { n = 2 } }).status == "ok""#,
                )
                .await;
                assert!(!cas_ok);
                assert_eq!(std::fs::read_to_string(&ns_file).unwrap(), bad);
            }
            h.shutdown().await;
        }
        ArtifactId::Auth => {
            let env = TestEnv::new().await;
            let h = env.host().no_session().build().await;
            expose(&h);
            global(&h, "LLM", h.service_table("llm").unwrap());
            exec(&h, r#"LLM:auth_set("openai", { type = "api", key = "sk-c1-test" })"#).await;
            let auth_p = env.config_dir().join("auth.json");
            assert_file_mode(&auth_p, meta.mode);
            let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&auth_p).unwrap()).unwrap();
            assert_eq!(meta.version, Some(1));
            assert_eq!(saved["version"], 1);
            assert_eq!(saved["openai"]["key"], "sk-c1-test");
            let reserved: bool = eval(&h, r#"return not pcall(LLM.auth_set, LLM, "version", 2)"#).await;
            assert!(reserved, "credentials must not replace the format version");
            let saved_again: serde_json::Value = serde_json::from_slice(&std::fs::read(&auth_p).unwrap()).unwrap();
            assert_eq!(saved_again, saved);

            for bad in ["{broken", "42"] {
                mare_shared::paths::write_atomic(&auth_p, bad.as_bytes(), meta.mode).unwrap();
                let (read_ok, err): (bool, String) =
                    eval(&h, "local ok, e = pcall(LLM.auth_get, LLM, 'openai'); return ok, tostring(e)").await;
                assert!(!read_ok && err.contains("corrupt"), "corrupt auth.json must fail closed: {bad}: {err}");
                let write_ok: bool =
                    eval(&h, r#"return pcall(LLM.auth_set, LLM, "openai", { type = "api", key = "x" })"#).await;
                assert!(!write_ok, "corrupt auth.json must refuse writes: {bad}");
                assert_eq!(std::fs::read_to_string(&auth_p).unwrap(), bad, "bytes untouched");
            }
            h.shutdown().await;
        }
        ArtifactId::Permissions => {
            let env = TestEnv::new().await;
            let cfg = serde_json::json!({ "plugins": { "permissions": { "enabled": true } } });
            let h = env.host().config(cfg).no_session().build().await;
            expose(&h);
            let bash_call = r#"
                local tools = C.cordis:get("tools")
                local def = tools and tools:get("bash")
                local info = {
                    side_effect = def and def.side_effect or "external",
                    idempotent = def and def.idempotent,
                    unconfined = def and def.unconfined,
                    command_tool = def and def.command_tool == true or true,
                    command = def and def.command,
                }
                local function decide(id, cmd)
                    local c = { id = id, name = "bash", arguments = { command = cmd, sandbox_permissions = "all", justification = "t" }, summary = "t" }
                    return c, C.cordis:get("policy"):decide("tool/before", { c, info })
                end
            "#;
            let wrote: bool = eval(
                &h,
                &format!(
                    r#"{bash_call}
                       local c, v = decide("1", "git push origin main")
                       assert(v and v.amendment, "expected pending amendment")
                       C.cordis:get("bus"):emit("pipeline/approval-decided", {{ call = c, decision = "allow_amend", amendment = v.amendment }})
                       return mare.fs.exists(mare.paths.state .. "/permissions.json")"#
                ),
            )
            .await;
            assert!(wrote, "permissions.json written via production bus event");
            let perm_p = env.home.path().join("state/permissions.json");
            assert_file_mode(&perm_p, meta.mode);

            for bad in ["{broken", "[1,2]", r#"{"version":99,"rules":[]}"#] {
                std::fs::write(&perm_p, bad).unwrap();
                exec(
                    &h,
                    &format!(
                        r#"{bash_call}
                           local c, v = decide("2", "cargo test")
                           assert(v and v.decision == "deny", "corrupt permissions.json must fail closed")
                           C.cordis:get("bus"):emit("pipeline/approval-decided", {{ call = c, decision = "allow_amend", amendment = {{ argv = {{ "cargo" }}, decision = "allow" }} }})"#
                    ),
                )
                .await;
                assert_eq!(
                    std::fs::read_to_string(&perm_p).unwrap(),
                    bad,
                    "corrupt permissions.json preserved ({bad})"
                );
            }
            h.shutdown().await;
        }
        ArtifactId::Trust => {
            let env = TestEnv::new().await;
            mare_config::trust::set_dir(&env.cwd, true).unwrap();
            let trust_p = mare_shared::paths::trust_file();
            assert_dir_private(trust_p.parent().unwrap());
            assert_file_mode(&trust_p, meta.mode);
            assert_eq!(mare_config::trust::decision(&env.cwd), Some(true));

            for bad in ["{broken", "[1]", r#"{"version":99,"decisions":{}}"#] {
                std::fs::write(&trust_p, bad).unwrap();
                assert_eq!(mare_config::trust::decision(&env.cwd), None, "trust fails closed on {bad}");
                assert!(mare_config::trust::set_dir(&env.cwd, true).is_err());
                assert_eq!(std::fs::read_to_string(&trust_p).unwrap(), bad);
            }
        }
        ArtifactId::McpApprovals => {
            let env = TestEnv::new().await;
            let fake = Fake::new(&env, "c1-mcp-app");
            let mut entry = fake.entry(serde_json::json!({}));
            entry["tier"] = serde_json::json!("direct");
            std::fs::write(
                env.cwd.join(".mcp.json"),
                serde_json::json!({ "mcpServers": { "srv": entry } }).to_string(),
            )
            .unwrap();
            mare_config::trust::set_dir(&env.cwd, true).unwrap();
            let mut hb =
                env.host().builtins(&["mcp"]).args(serde_json::json!({ "no_session": true, "approve_mcps": true }));
            hb.opts.project_trusted = true;
            let h = hb.build().await;
            expose(&h);
            wait_until(15, "approved mcp tool", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return T:get('mcp__srv__echo') ~= nil").await }
            })
            .await;
            h.shutdown().await;

            let p = env.home.path().join("state/mcp-approvals.json");
            assert_file_mode(&p, meta.mode);
            for bad in ["{broken", "[1]"] {
                std::fs::write(&p, bad).unwrap();
                let mut hb = env.host().builtins(&["mcp"]).no_session();
                hb.opts.project_trusted = true;
                let h = hb.build().await;
                expose(&h);
                let state_out: String = eval(
                    &h,
                    r#"local req = { action = "list", lines = {}, code = 0 }
                       C.cordis:get("bus"):emit("mcp/cli", req)
                       return table.concat(req.lines, "\n")"#,
                )
                .await;
                assert!(
                    state_out.contains("srv") && state_out.contains("error") && !state_out.contains("connected"),
                    "corrupt mcp-approvals.json must surface error state without connecting ({bad}): {state_out}"
                );
                let has_tool: bool = eval(&h, "return T:get('mcp__srv__echo') ~= nil").await;
                assert!(!has_tool, "corrupt mcp-approvals.json must fail closed: {bad}");
                assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
                h.shutdown().await;
            }
        }
        ArtifactId::McpDisabled => {
            let env = TestEnv::new().await;
            let fake = Fake::new(&env, "c1-mcp-dis");
            std::fs::create_dir_all(env.config_dir()).unwrap();
            let cfg_p = env.config_dir().join("mcp.json");
            let mut entry = fake.entry(serde_json::json!({}));
            entry["tier"] = serde_json::json!("direct");
            std::fs::write(&cfg_p, serde_json::json!({ "mcpServers": { "usr": entry } }).to_string()).unwrap();
            let h = env.host().builtins(&["mcp"]).no_session().build().await;
            expose(&h);
            wait_until(15, "usr server", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return T:get('mcp__usr__echo') ~= nil").await }
            })
            .await;
            exec(
                &h,
                r#"local req = { action = "disable", server = "usr", lines = {}, code = 0 }; C.cordis:get("bus"):emit("mcp/cli", req)"#,
            )
            .await;
            h.shutdown().await;

            let p = env.home.path().join("state/mcp-disabled.json");
            assert_file_mode(&p, meta.mode);
            for bad in ["{broken", "[1]"] {
                std::fs::write(&p, bad).unwrap();
                let h = env.host().builtins(&["mcp"]).no_session().build().await;
                expose(&h);
                let code: i64 = eval(
                    &h,
                    r#"local req = { action = "disable", server = "usr", lines = {}, code = 0 }; C.cordis:get("bus"):emit("mcp/cli", req); return req.code"#,
                )
                .await;
                assert_ne!(code, 0, "corrupt mcp-disabled.json must fail write: {bad}");
                assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
                h.shutdown().await;
            }
        }
        ArtifactId::McpOauth => {
            let _env = TestEnv::new().await;
            let p = mare_binding::mcp::oauth_file("srv", "https://example.test/mcp");
            let store = mare_binding::mcp::FileCredentialStore(p.clone());
            store.save_bearer("secret-tok").await.unwrap();
            assert_dir_private(p.parent().unwrap());
            assert_file_mode(&p, meta.mode);
            assert!(store.load_present().await.unwrap());

            for bad in ["{broken", "[1]"] {
                std::fs::write(&p, bad).unwrap();
                assert!(store.load_present().await.is_err(), "corrupt mcp-oauth must error: {bad}");
                assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
            }
        }
        ArtifactId::McpToolsCache => {
            let env = TestEnv::new().await;
            let fake = Fake::new(&env, "c1-mcp-cache");
            let first = fake.entry(serde_json::json!({ "tools": ["echo"] }));
            let cfg = serde_json::json!({ "plugins": { "mcp": { "servers": { "fake": first } } } });
            let h = env.host().builtins(&["mcp"]).config(cfg.clone()).no_session().build().await;
            expose(&h);
            exec(&h, r#"T:execute({ id = "s", name = "tool_search", arguments = { query = "select:echo" } }, {})"#)
                .await;
            h.shutdown().await;

            let p = find_first_matching(&env.home.path().join("state/mcp-tools"), ".json").expect("mcp tool cache");
            assert_file_mode(&p, meta.mode);
            for bad in ["{broken", "[1]", r#"{"version":99,"tools":[]}"#] {
                std::fs::write(&p, bad).unwrap();
                let h = env.host().builtins(&["mcp"]).config(cfg.clone()).no_session().build().await;
                expose(&h);
                exec(
                    &h,
                    r#"T:execute({ id = "s2", name = "tool_search", arguments = { query = "select:echo" } }, {})"#,
                )
                .await;
                h.shutdown().await;
                let fresh = std::fs::read_to_string(&p).unwrap();
                assert!(fresh.contains(r#""version":2"#), "Discard cache overwritten cleanly after {bad}: {fresh}");
            }
        }
        ArtifactId::ModelsCache => {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let p = env.home.path().join("state/models-cache/deepseek.json");
            let cfg = serde_json::json!({
                "providers": { "deepseek": { "base_url": format!("{}/compat", server.base) } },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 }
                }
            });
            env.set_var("DEEPSEEK_API_KEY", "sk-c1-test");
            server.route(
                "/compat/models",
                vec![
                    Resp::status(200, r#"{"object":"list","data":[{"id":"listed-1","context_length":65536}]}"#),
                    Resp::status(200, r#"{"object":"list","data":[{"id":"listed-2","context_length":65536}]}"#),
                    Resp::status(200, r#"{"object":"list","data":[{"id":"listed-3","context_length":65536}]}"#),
                ],
            );
            let h = env.host().config(cfg.clone()).no_session().build().await;
            expose(&h);
            global(&h, "LLM", h.service_table("llm").unwrap());
            exec(&h, "C.cordis:get('bus'):emit('ui/ready')").await;
            wait_until(10, "models cache written", || {
                let p = p.clone();
                async move { p.is_file() }
            })
            .await;
            h.shutdown().await;
            assert_file_mode(&p, meta.mode);

            for (bad, expected_id) in [("{broken", "listed-2"), ("[1]", "listed-3")] {
                std::fs::write(&p, bad).unwrap();
                let h = env.host().config(cfg.clone()).no_session().build().await;
                expose(&h);
                global(&h, "LLM", h.service_table("llm").unwrap());
                exec(&h, "C.cordis:get('bus'):emit('ui/ready')").await;
                wait_until(10, "discarded models cache overwritten", || {
                    let p = p.clone();
                    async move { std::fs::read_to_string(&p).is_ok_and(|s| s.contains(expected_id)) }
                })
                .await;
                h.shutdown().await;
            }
        }
        ArtifactId::SkillsCache => {
            let env = TestEnv::new().await;
            let root = env.home.path().join("skills");
            let skill = root.join("one/SKILL.md");
            std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
            std::fs::write(&skill, "---\nname: one\ndescription: registered skill\n---\nbody\n").unwrap();
            let cfg = serde_json::json!({ "plugins": { "context": { "skill_dirs": [root] } } });
            let cache_dir = env.home.path().join("state/skills-cache");
            let h = env.host().config(cfg.clone()).no_session().build().await;
            expose(&h);
            exec(&h, r#"C.cordis:get("context"):skills()"#).await;
            h.shutdown().await;
            let p = find_first_matching(&cache_dir, ".json").expect("skills cache");
            assert_dir_private(&cache_dir);
            assert_file_mode(&p, meta.mode);
            for bad in ["{broken", "[1]", r#"{"version":99,"skills":[]}"#] {
                std::fs::write(&p, bad).unwrap();
                let h = env.host().config(cfg.clone()).no_session().build().await;
                expose(&h);
                exec(&h, r#"C.cordis:get("context"):skills()"#).await;
                h.shutdown().await;
                let fresh = std::fs::read_to_string(&p).unwrap();
                let doc: serde_json::Value = serde_json::from_str(&fresh).unwrap();
                assert_eq!(doc["version"], 1, "discard corrupt skill cache: {bad}");
                assert_eq!(doc["skills"][0]["description"], "registered skill");
            }
        }
        ArtifactId::CrashGuard => {
            let env = TestEnv::new().await;
            let h = env.host().no_session().build().await;
            expose(&h);
            exec(&h, r#"assert(mare.crash.begin("fp-c1") == 0); mare.crash.finish("fp-c1")"#).await;
            let p = env.home.path().join("state/crash-guard.json");
            let lp = env.home.path().join("state/crash-guard.lock");
            assert_dir_private(p.parent().unwrap());
            assert_file_mode(&p, meta.mode);
            assert_file_mode(&lp, meta.mode);

            for bad in ["{broken", "[1]", r#"{"version":99,"entries":{}}"#] {
                std::fs::write(&p, bad).unwrap();
                let (crashes, reset): (i64, bool) = eval(
                    &h,
                    r#"local c, reset = mare.crash.begin("fp-after-corrupt"); mare.crash.finish("fp-after-corrupt"); return c, type(reset) == "string""#,
                )
                .await;
                assert_eq!((crashes, reset), (0, true), "quarantine reset must report on {bad}");
                let latest =
                    mare_binding::crashguard::quarantined_backups(p.parent().unwrap()).into_iter().next().unwrap();
                assert_eq!(std::fs::read_to_string(&latest).unwrap(), bad);
            }

            // Permission/IO error must NOT quarantine or overwrite crash-guard.json.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let before_cnt = mare_binding::crashguard::quarantined_backups(p.parent().unwrap()).len();
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
                let ok: bool = eval(&h, r#"return pcall(mare.crash.begin, "fp-perm")"#).await;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
                assert!(!ok, "IO/permission error must error rather than reset");
                let after_cnt = mare_binding::crashguard::quarantined_backups(p.parent().unwrap()).len();
                assert_eq!(before_cnt, after_cnt, "permission error must not trigger quarantine");
            }
            h.shutdown().await;
        }
        ArtifactId::PackageLock => {
            let _env = TestEnv::new().await;
            let repo = package_repo("c1pkg", None, "v1.0.0");
            let cfg = serde_json::json!({ "packages": [{ "name": "me/c1pkg", "url": repo.path() }] });
            mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
            let lock_p = mare_shared::paths::lock_file();
            assert_file_mode(&lock_p, meta.mode);

            for bad in ["[1]", "[]", "{}", "{broken", "42", r#"{"version":99,"packages":{}}"#] {
                std::fs::write(&lock_p, bad).unwrap();
                assert!(
                    mare_plugin_manager::read_lock(&serde_json::json!({})).is_err(),
                    "corrupt mare-lock.json must fail: {bad}"
                );
                assert!(mare_plugin_manager::sync(&cfg, &mut |_| ()).is_err());
                assert_eq!(std::fs::read_to_string(&lock_p).unwrap(), bad);
            }
        }
        ArtifactId::PackageExtras => {
            let mut env = TestEnv::new().await;
            let _catalog = crate::suite::packages::catalog(&mut env);
            mare_plugin_manager::set_extra("docs", true, None, &serde_json::json!({})).unwrap();
            let extras_p = mare_plugin_manager::extras_path(&serde_json::json!({}));
            assert_file_mode(&extras_p, meta.mode);

            for bad in ["[1]", "[]", "{}", "{broken", "42", r#"{"version":99,"plugins":[]}"#] {
                std::fs::write(&extras_p, bad).unwrap();
                assert!(
                    mare_plugin_manager::read_extras(&serde_json::json!({})).is_err(),
                    "corrupt plugins.json must fail: {bad}"
                );
                assert_eq!(std::fs::read_to_string(&extras_p).unwrap(), bad);
            }
        }
        ArtifactId::PackageCheckoutReferences => {
            let env = TestEnv::new().await;
            let repo = package_repo("reference", None, "v1.0.0");
            let cfg = serde_json::json!({ "packages": [{ "name": "me/reference", "url": repo.path() }] });
            mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
            let locks = mare_shared::paths::packages_dir().join(".locks");
            let refs = std::fs::read_dir(&locks)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|ext| ext == "refs"))
                .unwrap();
            let reference = std::fs::read_dir(&refs).unwrap().next().unwrap().unwrap().path();
            assert_file_mode(&reference, meta.mode);
            let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&reference).unwrap()).unwrap();
            assert_eq!(record["version"], 1);
            assert_eq!(record["lockfile"], mare_shared::paths::lock_file().to_string_lossy().as_ref());
            std::fs::write(&reference, "{broken").unwrap();
            let project = env.cwd.join(".mare");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(project.join("init.lua"), "").unwrap();
            let mut other = serde_json::json!({});
            mare_plugin_manager::set_project_scope(&mut other, &env.cwd, true);
            let mut log = Vec::new();
            mare_plugin_manager::remove_unused(&other, false, &mut |line| log.push(line)).unwrap();
            assert!(mare_plugin_manager::checkout_dir(&repo.path().to_string_lossy()).exists());
            assert!(log.iter().any(|line| line.contains("cannot inspect other scopes")), "{log:?}");
            assert_eq!(std::fs::read_to_string(&reference).unwrap(), "{broken");
        }
        ArtifactId::SpillResults => {
            let env = TestEnv::new().await;
            let h = env.host().no_session().build().await;
            expose(&h);
            exec(&h, r#"ITEM = mare.spill.save("c1-owner", "payload-text")"#).await;
            let sp = find_first_matching(&env.home.path().join("state/spill-results"), "").unwrap();
            assert_dir_private(sp.parent().unwrap());
            assert_file_mode(&sp, meta.mode);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&sp, std::fs::Permissions::from_mode(0o644)).unwrap();
                let ok: bool =
                    eval(&h, r#"return pcall(mare.spill.read, ITEM.spill_id, { session_id = "c1-owner" })"#).await;
                assert!(!ok, "spill-results must refuse non-private permissions");
            }
            h.shutdown().await;
        }
        ArtifactId::BashSpill => {
            let env = TestEnv::new().await;
            let h = env.host().no_session().build().await;
            expose(&h);
            let p_str: String = eval(
                &h,
                r#"local p = mare.paths.state .. "/spill/bash-1-s1-20260101-000000-abc.log"
                   local r = mare.proc.run({
                       cmd = "sh",
                       args = { "-c", "printf 'hello\\n'" },
                       spill = p,
                       spill_prefix = "bash",
                   })
                   assert(r.code == 0, "proc.run failed: " .. tostring(r.error))
                   return p"#,
            )
            .await;
            let sp = Path::new(&p_str);
            assert_dir_private(sp.parent().unwrap());
            assert_file_mode(sp, meta.mode);
            assert_eq!(std::fs::read_to_string(sp).unwrap(), "hello\n");
            exec(&h, &format!("mare.proc.spill_remove('bash', {:?})", p_str)).await;
            assert!(!sp.exists());
            h.shutdown().await;
        }
        ArtifactId::Logs => {
            use std::io::Write;
            let _env = TestEnv::new().await;
            let dir = mare_shared::paths::log_dir();
            let mut w = mare_shared::logfile::Redacting(mare_shared::logfile::writer(
                &dir,
                mare_shared::logfile::MAX_BYTES,
                mare_shared::logfile::KEEP,
            ));
            writeln!(w, "token sk-1234567890abcdefghijklmnop").unwrap();
            w.flush().unwrap();
            assert_dir_private(&dir);
            let log_p = dir.join("mare.log");
            assert_file_mode(&log_p, meta.mode);
            assert!(!std::fs::read_to_string(&log_p).unwrap().contains("sk-1234567890"));
        }
        ArtifactId::HostToken => {
            let env = TestEnv::new().await;
            let token_p = env.home.path().join("state/host/token");
            let lock_p = token_p.with_file_name(".token.lock");
            let t1 = mare_binding::http_serve::ensure_host_token_at(&token_p).unwrap();
            assert_dir_private(token_p.parent().unwrap());
            assert_file_mode(&token_p, meta.mode);
            assert_file_mode(&lock_p, meta.mode);
            assert_eq!(mare_binding::http_serve::ensure_host_token_at(&token_p).unwrap(), t1);

            for bad in ["", "short", "bad$chars_not_base64url_0123456789012345678"] {
                std::fs::write(&token_p, bad).unwrap();
                let err = mare_binding::http_serve::ensure_host_token_at(&token_p).unwrap_err();
                assert!(err.contains("corrupt host token file"), "{err}");
                assert_eq!(std::fs::read_to_string(&token_p).unwrap(), bad);
            }
        }
        ArtifactId::LuacCache => {
            let env = TestEnv::new().await;
            let src = env.cwd.join("c1_mod.lua");
            std::fs::write(&src, "return 42").unwrap();
            let settled = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
            std::fs::File::options().write(true).open(&src).unwrap().set_modified(settled).unwrap();
            let lua = mlua::Lua::new();
            let c1 = mare_kernel::load_cached_chunk(&lua, None, &src, "@c1_mod.lua", None).unwrap().dump(false);
            let luac_dir = env.home.path().join("cache/luac");
            let cache_p = find_first_matching(&luac_dir, ".luac").unwrap();
            assert_dir_private(&luac_dir);
            assert_file_mode(&cache_p, meta.mode);
            let _ = mare_kernel::loader_prune_if_due();
            let stamp = luac_dir.join(".pruned");
            assert!(stamp.is_file(), ".pruned stamp written");
            assert_file_mode(&stamp, meta.mode);
            assert_registered(env.home.path());
            // Corrupt cache discards silently and recompiles identical bytecode.
            std::fs::write(&cache_p, b"not-valid-luac").unwrap();
            let c2 = mare_kernel::load_cached_chunk(&lua, None, &src, "@c1_mod.lua", None).unwrap().dump(false);
            assert_eq!(c1, c2);
        }
        ArtifactId::News => {
            let mut env = TestEnv::new().await;
            let _catalog = crate::suite::packages::catalog(&mut env);
            mare_plugin_manager::set_extra("docs", true, None, &serde_json::json!({})).unwrap();
            let h = env.host().builtins(&["plugins"]).no_session().build().await;
            global(&h, "P", h.service_table("plugins").unwrap());
            let shown: Vec<String> = eval(&h, "return P:news()").await;
            assert!(shown.is_empty(), "fresh install only records versions: {shown:?}");

            let news_p = env.home.path().join("state/news.json");
            let lock_p = env.home.path().join("state/news.json.lock");
            assert_dir_private(news_p.parent().unwrap());
            assert_file_mode(&news_p, meta.mode);
            assert_file_mode(&lock_p, meta.mode);
            let parsed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&news_p).unwrap()).unwrap();
            assert_eq!(parsed["version"], serde_json::json!(mare_host::artifacts::NEWS_VERSION));

            for bad in [
                "{broken",
                "[1,2]",
                r#"{"version":99,"seen":{}}"#,
                r#"{"version":1,"seen":"bad"}"#,
                r#"{"version":1,"seen":null}"#,
                r#"{"version":1,"seen":{"mare":123}}"#,
                r#"{"version":1,"seen":[1]}"#,
            ] {
                std::fs::write(&news_p, bad).unwrap();
                let res: Vec<String> = eval(&h, "return P:news()").await;
                assert!(res.is_empty(), "corrupt/unsupported news.json must not show news: {res:?}");
                assert_eq!(
                    std::fs::read_to_string(&news_p).unwrap(),
                    bad,
                    "corrupt/unsupported news.json must be preserved ({bad})"
                );
            }

            #[cfg(unix)]
            {
                let ext = env.cwd.join("external-target.txt");
                std::fs::write(&ext, "KEEP-EXT-INTACT").unwrap();
                std::fs::remove_file(&news_p).unwrap();
                std::os::unix::fs::symlink(&ext, &news_p).unwrap();
                let res: Vec<String> = eval(&h, "return P:news()").await;
                assert!(res.is_empty(), "symlinked news.json must not show news or follow link: {res:?}");
                assert_eq!(std::fs::read_to_string(&ext).unwrap(), "KEEP-EXT-INTACT");
                assert!(
                    std::fs::symlink_metadata(&news_p).unwrap().file_type().is_symlink(),
                    "symlink at news.json must not be followed/mutated"
                );
                std::fs::remove_file(&news_p).unwrap();
                let _ = eval::<Vec<String>>(&h, "return P:news()").await;
            }
            assert_registered(env.home.path());
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn artifact_contract_session_journal() {
    local(verify_contract(ArtifactId::SessionJournal)).await;
}
#[tokio::test]
async fn artifact_contract_session_lock() {
    local(verify_contract(ArtifactId::SessionLock)).await;
}
#[tokio::test]
async fn artifact_contract_session_rollback() {
    local(verify_contract(ArtifactId::SessionRollback)).await;
}
#[tokio::test]
async fn artifact_contract_session_commit() {
    local(verify_contract(ArtifactId::SessionCommit)).await;
}
#[tokio::test]
async fn artifact_contract_session_index() {
    local(verify_contract(ArtifactId::SessionIndex)).await;
}
#[tokio::test]
async fn artifact_contract_session_attachment() {
    local(verify_contract(ArtifactId::SessionAttachment)).await;
}
#[tokio::test]
async fn artifact_contract_session_snapshot() {
    local(verify_contract(ArtifactId::SessionSnapshot)).await;
}
#[tokio::test]
async fn artifact_contract_state_file() {
    local(verify_contract(ArtifactId::StateFile)).await;
}

#[tokio::test]
async fn artifact_contract_namespace_state() {
    local(verify_contract(ArtifactId::NamespaceState)).await;
}
#[tokio::test]
async fn artifact_contract_auth() {
    local(verify_contract(ArtifactId::Auth)).await;
}

#[tokio::test]
async fn artifact_contract_permissions() {
    local(verify_contract(ArtifactId::Permissions)).await;
}
#[tokio::test]
async fn artifact_contract_trust() {
    local(verify_contract(ArtifactId::Trust)).await;
}
#[tokio::test]
async fn artifact_contract_mcp_approvals() {
    local(verify_contract(ArtifactId::McpApprovals)).await;
}
#[tokio::test]
async fn artifact_contract_mcp_disabled() {
    local(verify_contract(ArtifactId::McpDisabled)).await;
}
#[tokio::test]
async fn artifact_contract_mcp_oauth() {
    local(verify_contract(ArtifactId::McpOauth)).await;
}
#[tokio::test]
async fn artifact_contract_mcp_tools_cache() {
    local(verify_contract(ArtifactId::McpToolsCache)).await;
}
#[tokio::test]
async fn artifact_contract_models_cache() {
    local(verify_contract(ArtifactId::ModelsCache)).await;
}
#[tokio::test]
async fn artifact_contract_skills_cache() {
    local(verify_contract(ArtifactId::SkillsCache)).await;
}
#[tokio::test]
async fn artifact_contract_crash_guard() {
    local(verify_contract(ArtifactId::CrashGuard)).await;
}
#[tokio::test]
async fn artifact_contract_package_lock() {
    local(verify_contract(ArtifactId::PackageLock)).await;
}
#[tokio::test]
async fn artifact_contract_package_extras() {
    local(verify_contract(ArtifactId::PackageExtras)).await;
}
#[tokio::test]
async fn artifact_contract_package_checkout_references() {
    local(verify_contract(ArtifactId::PackageCheckoutReferences)).await;
}
#[tokio::test]
async fn artifact_contract_spill_results() {
    local(verify_contract(ArtifactId::SpillResults)).await;
}
#[tokio::test]
async fn artifact_contract_bash_spill() {
    local(verify_contract(ArtifactId::BashSpill)).await;
}
#[tokio::test]
async fn artifact_contract_logs() {
    local(verify_contract(ArtifactId::Logs)).await;
}
#[tokio::test]
async fn artifact_contract_host_token() {
    local(verify_contract(ArtifactId::HostToken)).await;
}
#[tokio::test]
async fn artifact_contract_luac_cache() {
    local(verify_contract(ArtifactId::LuacCache)).await;
}
#[tokio::test]
async fn artifact_contract_news() {
    local(verify_contract(ArtifactId::News)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn news_lock_sidecar_symlink_cannot_escape_state_dir() {
    local(async {
        let env = TestEnv::new().await;
        let state_dir = env.home.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let outside = env.home.path().join("outside-lock-target.txt");
        let lock_link = state_dir.join("news.json.lock");
        std::os::unix::fs::symlink(&outside, &lock_link).unwrap();

        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        global(&h, "P", h.service_table("plugins").unwrap());
        exec(&h, "P:news()").await;
        assert!(!outside.exists(), "anchored news.json.lock must never create or follow external symlink targets");
        assert!(!state_dir.join("news.json").exists(), "news.json must not be written when lock acquisition fails");
        h.shutdown().await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn news_lock_sidecar_fifo_cannot_stall_state_dir() {
    local(async {
        let env = TestEnv::new().await;
        let state_dir = env.home.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let fifo = state_dir.join("news.json.lock");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success(), "mkfifo failed: {status}");

        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        global(&h, "P", h.service_table("plugins").unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(3), exec(&h, "P:news()"))
            .await
            .expect("FIFO lock sidecar must fail promptly without blocking host");
        assert!(!state_dir.join("news.json").exists(), "news.json must not be written when FIFO lock fails");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn subagent_session_closure_matches_registry() {
    local(async {
        let env = TestEnv::new().await;
        let script = serde_json::json!([
            { "tool_calls": [{ "name": "subagent", "arguments": { "prompt": "child work", "name": "c2-worker", "wait": true } }] },
            { "text": "parent done" }
        ]);
        let h = env.host().builtins(&["subagent"]).faux(script).config(serde_json::json!({ "agents": { "max_depth": 1 } })).build().await;
        let res = h.prompt("delegate", serde_json::json!({})).await.unwrap();
        assert_eq!(res["text"], "parent done");
        h.shutdown().await;

        let child = find_first_matching(&env.home.path().join("state/sessions"), ".jsonl");
        let sessions_root = env.home.path().join("state/sessions");
        assert!(
            std::fs::read_dir(&sessions_root)
                .unwrap()
                .flatten()
                .any(|d| d.path().join("agents").is_dir()),
            "expected child transcript under sessions/*/agents/: {child:?}"
        );
        assert_registered(env.home.path());
    })
    .await;
}
