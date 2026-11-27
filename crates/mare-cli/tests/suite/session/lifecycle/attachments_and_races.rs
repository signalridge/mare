//! Fork attachment handling, prepared switch invalidation, and target/lock replacement races.

use super::*;

#[tokio::test]
async fn cancelled_fork_does_not_mutate_original_images() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        let imgs = json!([
            { "type":"image", "mime":"image/png", "data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC" },
            { "type":"image", "mime":"image/png", "data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNIYWAAAAEwAGXSlZcYAAAAAElFTkSuQmCC" }
        ]);
        h.prompt("IMAGES", json!({"images":imgs})).await.unwrap();
        let (finished, entered, image_type): (bool, bool, String) = eval(
            &h,
            r#"
            local entries = S:entries()
            local message
            for _, e in ipairs(entries) do
                if e.type == 'message' and e.message.role == 'user' then message = e.message; break end
            end
            local images = {}
            for _, b in ipairs(message.content) do if b.type == 'image' then images[#images+1] = b end end
            assert(#images == 2)
            local reserve = mare.store.reserve_staging_attachments
            WRITE_WAITING = false
            mare.store.reserve_staging_attachments = function(...)
                local stage = reserve(...)
                return { write = function(_, ...) WRITE_WAITING = true; mare.task.sleep(500); return stage:write(...) end,
                         close = function() return stage:close() end }
            end
            local finished = mare.task.timeout(80, function() S:fork({ session_id = S:info().session_id }) end)
            mare.store.reserve_staging_attachments = reserve
            return finished, WRITE_WAITING, images[1].type
        "#,
        )
        .await;
        assert!(!finished && entered);
        assert_eq!(image_type, "image", "snapshot hydration must not mutate the active message");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fork_path_collision_never_changes_existing_session_attachments() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        let image = json!([{ "type":"image", "mime":"image/png", "data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC" }]);
        h.prompt("IMAGE", json!({"images":image})).await.unwrap();
        let old: String = eval(&h, "return S:info().path").await;
        let dir = std::path::Path::new(&old).parent().unwrap();
        let collision = dir.join("collision_known123.jsonl");
        let body = "{\"type\":\"session\",\"version\":3,\"session_id\":\"existing\"}\n";
        std::fs::write(&collision, body).unwrap();
        let attachments = dir.join("collision_known123.attachments");
        std::fs::create_dir(&attachments).unwrap();
        std::fs::write(attachments.join("keep"), "existing").unwrap();
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            local date, uuid = mare.date, mare.crypto.uuid
            mare.date = function() return 'collision' end
            mare.crypto.uuid = function() return 'known123' end
            local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
            mare.date, mare.crypto.uuid = date, uuid
            return ok, tostring(err)
        "#,
        )
        .await;
        assert!(!ok && err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&collision).unwrap(), body);
        let names: Vec<_> = std::fs::read_dir(&attachments).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, [std::ffi::OsString::from("keep")]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn aborting_fork_task_releases_its_staging_directory() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "NEXT" }])).build().await;
        expose(&h);
        h.prompt("IMAGE", json!({"images":[{"type":"image", "mime":"image/png", "data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC"}]})).await.unwrap();
        let (id, path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        let (entered, switching, after): (bool, bool, String) = eval(&h, r#"
            WRITE_WAITING = false
            local reserve = mare.store.reserve_staging_attachments
            mare.store.reserve_staging_attachments = function(...)
                local stage = reserve(...)
                return { write = function(_, ...) WRITE_WAITING = true; mare.task.sleep(500); return stage:write(...) end,
                         close = function() return stage:close() end }
            end
            local task = mare.task.spawn(function() S:fork({ session_id = S:info().session_id }) end)
            while not WRITE_WAITING do mare.task.sleep(5) end
            task:abort()
            pcall(task.wait, task)
            mare.store.reserve_staging_attachments = reserve
            return WRITE_WAITING, S:switching(), S:info().session_id
        "#).await;
        assert!(entered && !switching);
        assert_eq!(after, id);
        let dir = std::path::Path::new(&path).parent().unwrap();
        assert!(std::fs::read_dir(dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".u1-")));
        assert_eq!(h.prompt("AFTER", json!({})).await.unwrap()["text"], "NEXT");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn fork_rejects_an_existing_orphan_attachment_directory() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        h.prompt("IMAGE", json!({"images":[{"type":"image", "mime":"image/png", "data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC"}]}))
            .await
            .unwrap();
        let old: String = eval(&h, "return S:info().path").await;
        let dir = std::path::Path::new(&old).parent().unwrap();
        let collision = dir.join("orphan_known123.jsonl");
        let attachments = dir.join("orphan_known123.attachments");
        std::fs::create_dir(&attachments).unwrap();
        std::fs::write(attachments.join("keep"), "existing").unwrap();
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            local date, uuid = mare.date, mare.crypto.uuid
            mare.date = function() return 'orphan' end
            mare.crypto.uuid = function() return 'known123' end
            local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
            mare.date, mare.crypto.uuid = date, uuid
            return ok, tostring(err)
        "#,
        )
        .await;
        assert!(!ok && err.contains("attachment directory already exists"), "{err}");
        assert!(!collision.exists());
        let names: Vec<_> = std::fs::read_dir(&attachments).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, [std::ffi::OsString::from("keep")]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_agent_context_replacement_invalidates_prepared_switch() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("context-race.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let target_before = std::fs::read_to_string(&target).unwrap();
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "AFTER" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        let old_id: String = eval(&h, "return S:info().session_id").await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(&h, r#"
            READ_WAITING = false
            local original = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...) READ_WAITING = true; mare.task.sleep(200); return original(...) end
        "#).await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)").await
        });
        wait_until(5, "target read", || { let h = h.clone(); async move { eval::<bool>(&h, "return READ_WAITING").await } }).await;
        exec(&h, "A:set_messages({{role='user', content={{type='text', text='REPLACEMENT'}}}})").await;
        let (ok, err) = loading.await.unwrap();
        assert!(!ok && err.contains("agent") && err.contains("changed"), "{err}");
        let (id, messages): (String, String) = eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
        assert_eq!(id, old_id);
        assert!(messages.contains("REPLACEMENT") && !messages.contains("TARGET"), "{messages}");
        assert_eq!(std::fs::read_to_string(target).unwrap(), target_before);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn agent_state_mutation_invalidates_prepared_switch() {
    local(async {
        for (field, mutation) in [
            ("messages", "A:messages()[1].content[1].text = 'MUTATED'"),
            ("queue", "A:update_queued(A:list_queue().data[1].input_id, { input = 'MUTATED' })"),
        ] {
            let env = TestEnv::new().await;
            let target = env.write_session("in-place-race.jsonl", &[msg("m1", None, "user", "TARGET")]);
            let original = std::fs::read(&target).unwrap();
            let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
            expose(&h);
            h.prompt("BEFORE", json!({})).await.unwrap();
            exec(&h, "A:set_queued({follow_up={{input_id='queued-id',text='QUEUED'}}})").await;
            let id: String = eval(&h, "return S:info().session_id").await;
            global(&h, "TARGET", target.to_string_lossy().to_string());
            exec(&h, r#"
                local entered_tx,entered_rx=mare.task.channel()
                local release_tx,release_rx=mare.task.channel()
                READ_READY,READ_RELEASE=entered_rx,release_tx
                local original = mare.store.read_tail_snapshot
                mare.store.read_tail_snapshot = function(...) entered_tx:send(true);release_rx:recv();return original(...) end
            "#).await;
            let loader = h.clone();
            let loading = tokio::task::spawn_local(async move {
                eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, (type(e) == 'table' and e.message) or tostring(e)").await
            });
            exec(&h,"READ_READY:recv()").await;
            exec(&h, mutation).await;
            exec(&h,"READ_RELEASE:send(true)").await;
            let (ok, err) = loading.await.unwrap();
            assert!(!ok && err.contains("agent") && err.contains("changed"), "{field}: {err}");
            assert_eq!(eval::<String>(&h, "return S:info().session_id").await, id);
            let state: String = eval(&h, if field == "messages" { "return mare.json.encode(A:list_messages().data)" } else { "return mare.json.encode(A:list_queue().data)" }).await;
            assert!(state.contains("MUTATED"), "{field}: {state}");
            assert_eq!(std::fs::read(target).unwrap(), original);
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn in_place_session_entry_mutation_invalidates_prepared_switch() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("session-entry-race.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let original = std::fs::read(&target).unwrap();
        let h = env.host().faux(json!([{ "text": "SEED" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        exec(&h, "ENTRIES = S:entries(); ENTRIES[1].race_marker = {value='ORIGINAL'}").await;
        let id: String = eval(&h, "return S:info().session_id").await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(&h, r#"
            READ_WAITING = false
            local original = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...) READ_WAITING = true; mare.task.sleep(200); return original(...) end
        "#).await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)").await
        });
        wait_until(5, "target read", || { let h = h.clone(); async move { eval::<bool>(&h, "return READ_WAITING").await } }).await;
        exec(&h, "ENTRIES[1].race_marker.value = 'MUTATED'").await;
        let (ok, err) = loading.await.unwrap();
        assert!(!ok && err.contains("active session changed"), "{err}");
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, id);
        assert_eq!(eval::<String>(&h, "return S:entries()[1].race_marker.value").await, "MUTATED");
        assert_eq!(std::fs::read(target).unwrap(), original);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn yielding_agent_method_lookup_cannot_cross_publication_boundary() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("yield-lookup.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let original = std::fs::read(&target).unwrap();
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "AFTER" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        let (id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(&h, r#"
            READ_WAITING = false
            local original = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...) READ_WAITING = true; mare.task.sleep(200); return original(...) end
        "#).await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)").await
        });
        wait_until(5, "target read", || { let h = h.clone(); async move { eval::<bool>(&h, "return READ_WAITING").await } }).await;
        exec(&h, r#"
            ORIGINAL_ADOPT = A.adopt_session
            ORIGINAL_MT = getmetatable(A)
            A.adopt_session = nil
            setmetatable(A, setmetatable({__index = function(t, key)
                if key == 'adopt_session' then mare.task.sleep(100); return ORIGINAL_ADOPT end
                local index = ORIGINAL_MT.__index
                if type(index) == 'function' then return index(t, key) end
                return index[key]
            end}, {__index = ORIGINAL_MT}))
        "#).await;
        let (ok, err) = loading.await.unwrap();
        exec(&h, "setmetatable(A, ORIGINAL_MT); A.adopt_session = ORIGINAL_ADOPT").await;
        assert!(!ok && err.contains("yield"), "{err}");
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, id);
        assert_eq!(std::fs::read(target).unwrap(), original);
        assert!(h.prompt("CONTINUED", json!({})).await.is_err());
        assert!(eval::<bool>(&h, "return S:switching()").await);
        h.shutdown().await;
        assert!(!std::fs::read_to_string(old_path).unwrap().contains("CONTINUED"));
    }).await
}

#[cfg(unix)]
#[tokio::test]
async fn reloading_current_session_rejects_path_replaced_during_read() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        let (id, path, messages): (String, String, String) = eval(&h, "return S:info().session_id, S:info().path, mare.json.encode(A:list_messages().data)").await;
        global(&h, "TARGET", path.clone());
        exec(&h, r#"
            READ_WAITING = false
            local original = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...) local result = table.pack(original(...)); READ_WAITING = true; mare.task.sleep(200); return table.unpack(result, 1, result.n) end
        "#).await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)").await
        });
        wait_until(5, "target read", || { let h = h.clone(); async move { eval::<bool>(&h, "return READ_WAITING").await } }).await;
        let moved = format!("{path}.moved");
        std::fs::rename(&path, &moved).unwrap();
        let replacement = b"{\"type\":\"session\",\"version\":3,\"session_id\":\"replacement\"}\n";
        std::fs::write(&path, replacement).unwrap();
        let (ok, err) = loading.await.unwrap();
        assert!(!ok && err.contains("file changed"), "{err}");
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, id);
        assert_eq!(eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await, messages);
        assert_eq!(std::fs::read(&path).unwrap(), replacement);
        h.shutdown().await;
    }).await
}

#[cfg(unix)]
#[tokio::test]
async fn loading_another_session_rejects_replaced_target_or_lock() {
    local(async {
        for replace_lock in [false, true] {
            let env = TestEnv::new().await;
            let target = env.write_session("target-identity.jsonl", &[msg("m1", None, "user", "TARGET")]);
            let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
            expose(&h);
            h.prompt("BEFORE", json!({})).await.unwrap();
            let old_id: String = eval(&h, "return S:info().session_id").await;
            global(&h, "TARGET", target.to_string_lossy().to_string());
            exec(
                &h,
                r#"
                READ_WAITING = false
                local original = mare.store.read_tail_snapshot
                mare.store.read_tail_snapshot = function(...)
                    local result = table.pack(original(...))
                    READ_WAITING = true
                    mare.task.sleep(200)
                    return table.unpack(result, 1, result.n)
                end
            "#,
            )
            .await;
            let loader = h.clone();
            let loading = tokio::task::spawn_local(async move {
                eval::<(bool, String)>(
                    &loader,
                    "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)",
                )
                .await
            });
            wait_until(5, "target read", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return READ_WAITING").await }
            })
            .await;
            let path = if replace_lock { mare_store::lock_path(&target) } else { target.clone() };
            let moved = path.with_extension("moved");
            std::fs::rename(&path, &moved).unwrap();
            std::fs::write(&path, b"replacement").unwrap();
            let (ok, err) = loading.await.unwrap();
            assert!(!ok && err.contains("file changed"), "lock={replace_lock}: {err}");
            assert_eq!(eval::<String>(&h, "return S:info().session_id").await, old_id);
            assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            h.shutdown().await;
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn fork_rejects_target_or_lock_replaced_during_revision_check() {
    local(async {
        for replace_lock in [false, true] {
            let env = TestEnv::new().await;
            let replacement_path = env.cwd.join("replacement-file");
            std::fs::write(&replacement_path, b"replacement").unwrap();
            let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
            expose(&h);
            h.prompt("BEFORE", json!({})).await.unwrap();
            let (id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
            global(&h, "REPLACE_LOCK", replace_lock);
            global(&h, "REPLACEMENT", replacement_path.to_string_lossy().to_string());
            let (ok, error, replaced): (bool, String, String) = eval(
                &h,
                r#"
                local target
                local open = mare.store.open
                mare.store.open = function(path, opts)
                    local file = open(path, opts)
                    if opts and opts.create_new then target = path end
                    return file
                end
                local original = A.check_session_revision
                local replaced
                A.check_session_revision = function(...)
                    if target and not replaced then
                        replaced = REPLACE_LOCK and (target .. '.lock') or target
                        mare.fs.rename(replaced, replaced .. '.moved')
                        mare.fs.rename(REPLACEMENT, replaced)
                    end
                    return original(...)
                end
                local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
                A.check_session_revision = original
                mare.store.open = open
                return ok, tostring(err), replaced or ''
            "#,
            )
            .await;
            assert!(!ok && error.contains("file changed"), "lock={replace_lock}: {error}");
            assert!(!replaced.is_empty());
            assert_eq!(std::fs::read(replaced).unwrap(), b"replacement");
            assert_eq!(eval::<String>(&h, "return S:info().session_id").await, id);
            assert!(h.prompt("AFTER", json!({})).await.is_ok());
            assert!(std::fs::read_to_string(old_path).unwrap().contains("AFTER"));
            h.shutdown().await;
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn fork_target_replaced_during_adoption_poison_barrier_blocks_turns() {
    local(async {
        let env = TestEnv::new().await;
        let replacement = env.cwd.join("replacement-file");
        std::fs::write(&replacement, b"replacement").unwrap();
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        global(&h, "REPLACEMENT", replacement.to_string_lossy().to_string());
        let (ok, error, switching, replaced): (bool, String, bool, String) = eval(
            &h,
            r#"
            local target
            local open = mare.store.open
            mare.store.open = function(path, opts)
                local file = open(path, opts)
                if opts and opts.create_new then target = path end
                return file
            end
            local original = A.adopt_session
            A.adopt_session = function(...)
                original(...)
                mare.fs.rename(target, target .. '.moved')
                mare.fs.rename(REPLACEMENT, target)
            end
            local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
            A.adopt_session = original
            mare.store.open = open
            return ok, tostring(err), S:switching(), target
        "#,
        )
        .await;
        assert!(!ok && error.contains("target file changed") && switching, "{error}");
        assert_eq!(std::fs::read(replaced).unwrap(), b"replacement");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fork_of_reloaded_persisted_image_refuses_a_path_hydrated_attachment() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        h.prompt("IMAGE", json!({"images":[{"type":"image", "mime":"image/png", "data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC"}]})).await.unwrap();
        let (source, ok, error): (String, bool, String) = eval(
            &h,
            r#"
            local source = S:info().path
            S:load({ path = source })
            local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
            return source, ok, tostring(err)
        "#,
        ).await;
        assert!(!ok && error.contains("attachment"), "{error}");
        assert!(std::fs::read_to_string(source).unwrap().contains("attachment"));
        h.shutdown().await;
    })
    .await
}
