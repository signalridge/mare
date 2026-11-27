use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::*;

#[tokio::test]
async fn fork_installs_and_syncs_attachments_before_writing_journal_references() {
    local(async {
        let env = TestEnv::new().await;
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        h.prompt("IMAGE", json!({ "images": [{ "type": "image", "mime": "image/png", "data": data }] })).await.unwrap();

        let probed = Arc::new(AtomicBool::new(false));
        let probed_clone = probed.clone();
        mare_binding::store::before_attachment_directory_sync(h.lua(), move |journal| {
            let body = std::fs::read_to_string(journal)?;
            assert!(
                !body.contains("attachment") && !body.contains(data),
                "journal must not contain image references before directory sync completes: {body}"
            );
            let dir = format!("{}.attachments", journal.to_string_lossy().trim_end_matches(".jsonl"));
            let installed: Vec<_> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).collect();
            assert_eq!(installed.len(), 1, "attachment must be installed before directory sync");
            assert!(installed[0].is_file());
            probed_clone.store(true, Ordering::SeqCst);
            Ok(())
        });

        let fork_path: String = eval(&h, "S:fork({ session_id = S:info().session_id }); return S:info().path").await;
        assert!(probed.load(Ordering::SeqCst), "directory sync ordering probe did not run");
        h.shutdown().await;

        let body = std::fs::read_to_string(&fork_path).unwrap();
        assert!(!body.contains(data) && body.contains("attachment"), "{body}");
        let dir = format!("{}.attachments", fork_path.trim_end_matches(".jsonl"));
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].is_file());

        let h = env.host().build().await;
        expose(&h);
        global(&h, "FORK_PATH", fork_path);
        let restored: String = eval(
            &h,
            r#"
            S:load({ path = FORK_PATH })
            for _, m in ipairs(A:list_messages().data) do
                for _, b in ipairs(m.content or {}) do
                    if b.type == 'image' then return b.data end
                end
            end
            return 'missing'
            "#,
        )
        .await;
        assert_eq!(restored, data);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fork_aborts_cleanly_when_attachment_directory_sync_fails() {
    local(async {
        let env = TestEnv::new().await;
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "AFTER" }])).build().await;
        expose(&h);
        h.prompt("IMAGE", json!({ "images": [{ "type": "image", "mime": "image/png", "data": data }] })).await.unwrap();
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        let old_body = std::fs::read_to_string(&old_path).unwrap();
        assert!(old_body.contains("attachment"), "{old_body}");

        let failed_journal = Arc::new(std::sync::Mutex::new(None));
        let failed_journal_clone = failed_journal.clone();
        mare_binding::store::before_attachment_directory_sync(h.lua(), move |journal| {
            *failed_journal_clone.lock().unwrap() = Some(journal.to_path_buf());
            Err(std::io::Error::other("injected attachment directory sync failure"))
        });

        let (ok, err, cur_id, cur_path): (bool, String, String, String) = eval(
            &h,
            r#"
            local ok, err = pcall(function() S:fork({ session_id = S:info().session_id }) end)
            return ok, tostring(err), S:info().session_id, S:info().path
            "#,
        )
        .await;
        assert!(!ok, "fork must fail when attachment directory sync fails");
        assert!(err.contains("injected attachment directory sync failure"), "{err}");
        assert_eq!(cur_id, old_id);
        assert_eq!(cur_path, old_path);
        assert_eq!(std::fs::read_to_string(&old_path).unwrap(), old_body);

        let broken = failed_journal.lock().unwrap().clone().expect("sync hook should have observed target journal");
        assert!(
            std::fs::read(&broken).map_or(true, |bytes| bytes.is_empty()),
            "failed fork journal must contain no uncommitted entries or image references: {}",
            broken.display()
        );

        let after = h.prompt("AFTER", json!({})).await.unwrap();
        assert_eq!(after["text"], "AFTER");
        h.shutdown().await;
    })
    .await
}

/// `session.prepare_fork()` drains/flushes and returns `{ path, images? }` (or nil
/// when unpersisted), and a forked child VM consumes and clears `mare.args.fork_images` so an
/// in-memory-only image reaches the forked child even when the source attachment is absent on disk.
#[tokio::test]
async fn session_prepare_fork_and_fork_images_arg_channel() {
    local(async {
        let env = TestEnv::new().await;
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);

        // 1. Unpersisted session: prepare_fork() returns nil.
        let unpersisted_nil: bool = eval(
            &h,
            r#"
            assert(type(S.prepare_fork) == "function", "session provider must implement session.prepare_fork")
            return S:prepare_fork() == nil
            "#,
        )
        .await;
        assert!(unpersisted_nil, "unpersisted session must return nil from prepare_fork");

        // 2. Persisted session with an image: prepare_fork() returns { path, images }.
        h.prompt("IMAGE", json!({ "images": [{ "type": "image", "mime": "image/png", "data": data }] })).await.unwrap();
        let prep_json: String = eval(&h, "return mare.json.encode(S:prepare_fork())").await;
        let prep: Value = serde_json::from_str(&prep_json).unwrap();
        let source_path = prep["path"].as_str().expect("prepare_fork must return path").to_owned();
        let images = prep["images"].as_object().expect("prepare_fork must return images map");
        assert_eq!(images.len(), 1, "expected 1 verified image in prepare_fork result: {prep}");
        assert_eq!(images.values().next().and_then(Value::as_str), Some(data));

        // Remove the source's on-disk attachment directory so only the in-memory `fork_images`
        // channel can hydrate the forked child.
        let src_att_dir = format!("{}.attachments", source_path.trim_end_matches(".jsonl"));
        std::fs::remove_dir_all(&src_att_dir).unwrap();

        // 3. Boot a forked child VM passing `mare.args.fork_images` (while source `h` still holds the lock).
        let child = env
            .host()
            .args(json!({
                "resume": source_path,
                "fork_session": true,
                "fork_images": prep["images"],
            }))
            .build()
            .await;
        expose(&child);
        let (cleared, restored, resume_err): (bool, String, String) = eval(
            &child,
            r#"
            local found = "missing"
            for _, m in ipairs(A:list_messages().data) do
                for _, b in ipairs(m.content or {}) do
                    if b.type == "image" then found = tostring(b.data) end
                end
            end
            return mare.args.fork_images == nil, found, tostring(S:info().resume_error or "")
            "#,
        )
        .await;
        assert_eq!(resume_err, "", "forked child startup must succeed without resume_error");
        assert!(cleared, "provider must clear mare.args.fork_images after consuming it");
        assert_eq!(restored, data, "in-memory image from prepare_fork must reach the forked child");

        child.shutdown().await;
        h.shutdown().await;
    })
    .await
}

/// Cold fork of an unloaded persisted session containing an inline image preserves the image and
/// installs content-addressed attachment files in the forked child. (Offloaded `attachment` refs
/// on a reloaded session remain intentionally refused per
/// `fork_of_reloaded_persisted_image_refuses_a_path_hydrated_attachment`; live sources pass
/// verified images via `session.prepare_fork` -> `mare.args.fork_images`.)
#[tokio::test]
async fn cold_fork_with_images_keeps_attachments() {
    local(async {
        let env = TestEnv::new().await;
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let source_path = env.write_session(
            "cold-img-source.jsonl",
            &[
                json!({
                    "type": "message",
                    "entry_id": "m1",
                    "parent_entry_id": null,
                    "message": {
                        "role": "user",
                        "content": [
                            { "type": "text", "text": "COLD_IMG" },
                            { "type": "image", "mime": "image/png", "data": data }
                        ],
                        "timestamp": 1
                    }
                }),
                json!({
                    "type": "message",
                    "entry_id": "m2",
                    "parent_entry_id": "m1",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "text", "text": "SEED" }],
                        "stop_reason": "end_turn",
                        "timestamp": 2
                    }
                }),
                json!({
                    "type": "custom",
                    "entry_id": "r1",
                    "parent_entry_id": "m2",
                    "kind": "agent/run-end",
                    "data": { "session_id": "s1", "run_id": "cold-1", "leaf_entry_id": "m2" },
                    "created_at_ms": 3
                }),
            ],
        );

        // Source is unloaded (cold): fork from disk via startup args.
        let child = env
            .host()
            .args(json!({
                "resume": source_path.to_string_lossy(),
                "fork_session": true,
            }))
            .build()
            .await;
        expose(&child);
        let (restored, child_path, resume_err): (String, String, String) = eval(
            &child,
            r#"
            local found = "missing"
            for _, m in ipairs(A:list_messages().data) do
                for _, b in ipairs(m.content or {}) do
                    if b.type == "image" then found = tostring(b.data) end
                end
            end
            return found, tostring(S:info().path or ""), tostring(S:info().resume_error or "")
            "#,
        )
        .await;
        assert_eq!(resume_err, "", "cold fork must not fail");
        assert_eq!(restored, data, "cold fork must preserve image data");
        assert!(!child_path.is_empty(), "cold fork must persist child session");
        let child_att_dir = format!("{}.attachments", child_path.trim_end_matches(".jsonl"));
        assert!(
            std::fs::read_dir(&child_att_dir).map(|mut d| d.next().is_some()).unwrap_or(false),
            "cold fork child must keep attachment files on disk at {child_att_dir}"
        );
        child.shutdown().await;
    })
    .await
}

/// Live multi-runtime fork via `HostManager::fork_session` uses `session.prepare_fork` ->
/// `mare.args.fork_images` to deliver an in-memory-only image to the forked child even when the
/// source `.attachments/` directory is gone, and fails with `unavailable` when a provider omits
/// `prepare_fork` instead of silently falling back to reading `info().path`.
#[tokio::test]
async fn fork_with_memory_only_image_reaches_the_child() {
    local(async {
        let env = TestEnv::new().await;
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let mgr = mare_host::HostManager::new();
        let base_opts = env.host().faux(json!([{ "text": "SEED" }])).opts;
        let s1 = mgr.create_session(base_opts.clone()).await.unwrap();
        expose(&s1.host);
        s1.host
            .prompt("MEM_IMG", json!({ "images": [{ "type": "image", "mime": "image/png", "data": data }] }))
            .await
            .unwrap();
        let source_path: String = eval(&s1.host, "return assert(S:info().path)").await;
        let src_att_dir = format!("{}.attachments", source_path.trim_end_matches(".jsonl"));
        std::fs::remove_dir_all(&src_att_dir).unwrap();

        let s2 = mgr.fork_session(&s1.session_id, base_opts.clone()).await.expect("fork_session must succeed");
        expose(&s2.host);
        let (cleared, restored, resume_err): (bool, String, String) = eval(
            &s2.host,
            r#"
            local found = "missing"
            for _, m in ipairs(A:list_messages().data) do
                for _, b in ipairs(m.content or {}) do
                    if b.type == "image" then found = tostring(b.data) end
                end
            end
            return mare.args.fork_images == nil, found, tostring(S:info().resume_error or "")
            "#,
        )
        .await;
        assert_eq!(resume_err, "");
        assert!(cleared, "child must clear mare.args.fork_images");
        assert_eq!(restored, data, "memory-only image must reach child through prepare_fork + fork_images");

        let _ = mgr.shutdown_all().await;
        drop(env);

        // A session provider that omits optional `prepare_fork` must fail with `unavailable`
        // rather than silently falling back to `info().path` or a kernel registry hook.
        let env2 = TestEnv::new().await;
        let plugins = env2.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let no_prep = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session_replacement.lua"))
            .replace("prepare_fork = function() return { path = cur.path } end,", "");
        std::fs::write(plugins.join("session.lua"), no_prep).unwrap();
        let mgr2 = mare_host::HostManager::new();
        let opts2 = env2.host().user_plugins().faux(json!([])).opts;
        let r1 = mgr2.create_session(opts2.clone()).await.unwrap();
        let err = match mgr2.fork_session(&r1.session_id, opts2).await {
            Err(e) => e,
            Ok(_) => panic!("missing prepare_fork must fail"),
        };
        assert_eq!(err.code(), "unavailable", "missing prepare_fork must report unavailable, got {err:?}");
        let _ = mgr2.shutdown_all().await;
    })
    .await;
}

/// The host must clear `mare.args.fork_images` after startup completes even when startup does not
/// execute the fork path or when startup fails with `resume_error`.
#[tokio::test]
async fn host_clears_fork_images_arg_after_startup_even_without_fork_or_on_failure() {
    local(async {
        let env = TestEnv::new().await;
        let h1 = env.host().faux(json!([])).args(json!({ "fork_images": { "stale.png": "AAAA" } })).build().await;
        let cleared_normal: bool = eval(&h1, "return mare.args.fork_images == nil").await;
        assert!(cleared_normal, "host must clear mare.args.fork_images after non-fork startup");
        h1.shutdown().await;

        let missing = env.cwd.join("does-not-exist.jsonl");
        let h2 = env
            .host()
            .faux(json!([]))
            .args(json!({
                "resume": missing.to_string_lossy(),
                "fork_session": true,
                "fork_images": { "stale.png": "AAAA" },
            }))
            .build()
            .await;
        let cleared_failed: bool = eval(&h2, "return mare.args.fork_images == nil").await;
        assert!(cleared_failed, "host must clear mare.args.fork_images after failed fork startup");
        h2.shutdown().await;
    })
    .await;
}
