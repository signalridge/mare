//! Images across compaction: offload to files, limits, estimates and pruning.

use super::*;

#[tokio::test]
async fn image_wire_budget_counts_repeated_images_before_first_provider_request() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({"plugins":{"agent":{"max_image_wire_bytes":12},"compact":{"auto":false}}});
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text":"done" }])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"
            local data = mare.crypto.base64_encode('PNG-REF')
            A:set_messages({{role='user', content={{type='image',mime='image/png',data=data},{type='image',mime='image/png',data=data}}}})
            C.cordis:on('request/build', function(req, next)
                req.messages[#req.messages + 1] = {role='assistant', content={{type='image',mime='image/png',data=data}}}
                return next()
            end)
            REQUESTS, WIRED = 0, ''
            local p = L:provider('faux')
            local stream = p.stream
            p.stream = function(req, sink)
                REQUESTS = REQUESTS + 1
                WIRED = mare.json.encode(req.messages)
                return stream(req, sink)
            end
        "#).await;
        let result = h.prompt("continue", json!({})).await.unwrap();
        assert_eq!(result["text"], "done", "{result}");
        let (requests, wire): (i64, String) = eval(&h, "return REQUESTS, WIRED").await;
        assert_eq!(requests, 1, "preflight must occur before the first provider request");
        let messages: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert!(messages[0]["content"].as_array().unwrap().iter().all(|block| block["type"] == "text"), "{wire}");
        let assistant = messages.as_array().unwrap().iter().find(|message| message["role"] == "assistant").unwrap();
        assert_eq!(assistant["content"][0]["data"], "UE5HLVJFRg==", "assistant content must not be offloaded: {wire}");
        assert!(wire.contains("[Attachment:"), "{wire}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn image_wire_budget_rechecks_fallback_after_request_hook_duplicates_image() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({"model":"faux/echo", "fallback_models":["imgfallback/one"], "plugins":{
            "agent":{"max_image_wire_bytes":12}, "compact":{"auto":false}, "llm":{"retries":0,"overloaded_retries":0}
        }});
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"
            local data = mare.crypto.base64_encode('PNG-REF')
            A:set_messages({{role='user', content={{type='image',mime='image/png',data=data}}}})
            PRIMARY_WIRED, FALLBACK_WIRED = '', ''
            local p = L:provider('faux')
            p.stream = function(req) PRIMARY_WIRED = mare.json.encode(req.messages); error({message='quota',kind='quota',retryable=false}) end
            L:register_provider({provider='imgfallback', is_configured=function() return true end,
                models={{model_id='one',context_window=100000,max_output=4096}},
                stream=function(req, sink)
                    FALLBACK_WIRED = mare.json.encode(req.messages)
                    return {role='assistant',stop_reason='stop',content={{type='text',text='fallback done'}}}
                end})
            C.cordis:on('request/build', function(req, next)
                if req.model.provider == 'imgfallback' then req.messages[#req.messages + 1] = table.copy(req.messages[1]) end
                return next()
            end)
        "#).await;
        let result = h.prompt("continue", json!({})).await.unwrap();
        assert_eq!(result["text"], "fallback done", "{result}");
        let (primary, fallback): (String, String) = eval(&h, "return PRIMARY_WIRED, FALLBACK_WIRED").await;
        assert!(primary.contains("UE5HLVJFRg=="), "the exact limit must not offload: {primary}");
        assert!(!fallback.contains("UE5HLVJFRg==") && fallback.contains("[Attachment:"), "{fallback}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn optional_image_offload_preserves_the_provider_error_and_diagnoses_invalid_values() {
    local(async {
        for invalid in [false, true] {
            let env = TestEnv::new().await;
            let refused = json!({ "error": "Too many images in request", "kind": "image_limit" });
            let h = env.host().builtins(&["compact", "usage"]).faux(json!([refused])).no_session().build().await;
            expose(&h);
            global(&h, "K", h.service_table("compact").unwrap());
            exec(&h, r#"NOTICES = {} C.cordis:on("host/notice", function(ev) NOTICES[#NOTICES + 1] = ev.text end)"#)
                .await;
            exec(&h, if invalid { "K.offload_images = true" } else { "K.offload_images = nil" }).await;
            let res = h.prompt("look", json!({})).await.unwrap();
            assert!(res.to_string().contains("Too many images in request"), "{res}");
            let notices: String = eval(&h, r#"return table.concat(NOTICES, "\n")"#).await;
            assert_eq!(notices.contains("compact.offload_images must be a function"), invalid, "{notices}");
            h.shutdown().await;
        }
    })
    .await
}

/// A provider's "too many images" goes through the image
/// offload stage — the older images first — and the request is sent again.
#[tokio::test]
async fn an_image_limit_error_offloads_images_and_sends_again() {
    local(async {
        let env = TestEnv::new().await;
        let refused = json!({ "error": "Too many images in request", "kind": "image_limit" });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([refused, { "text": "seen them" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"local msgs = {}
               for i = 1, 4 do
                   msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = "look " .. i },
                       { type = "image", mime = "image/png", data = mare.crypto.base64_encode("PNG-" .. i) } } }
                   msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "seen" } } }
               end
               A:set_messages(msgs)"#,
        )
        .await;
        let res = h.prompt("and now?", json!({})).await.unwrap();
        assert_eq!(res["text"], "seen them", "{res}");
        let (inline, refs, errors): (i64, i64, i64) = eval(
            &h,
            r#"local inline, refs, errors = 0, 0, 0
               for _, m in ipairs(A:list_messages().data) do
                   if m.stop_reason == "error" then errors = errors + 1 end
                   for _, b in ipairs(m.content) do
                       if b.type == "image" then inline = inline + 1 end
                       if b.type == "text" and b.text:find("[Attachment:", 1, true) then refs = refs + 1 end
                   end
               end
               return inline, refs, errors"#,
        )
        .await;
        assert_eq!((inline, refs), (2, 2), "the older two images were offloaded, the latest two kept");
        assert_eq!(errors, 0, "the refused reply is not kept");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn older_images_are_offloaded_to_files_not_dropped() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "stages": ["offload"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        assert!(!h.kernel.is_module_loaded("compact", "compact.projection"));
        // four images (base64 of "PNG-<n>"); the two most recent stay inline
        exec(
            &h,
            r#"local msgs = {}
               for i = 1, 4 do
                   msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = "look " .. i },
                       { type = "image", mime = "image/png", data = mare.crypto.base64_encode("PNG-" .. i) } } }
                   msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "seen" } } }
               end
               A:set_messages(msgs)"#,
        )
        .await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, inline, refs): (bool, i64, Vec<String>) = eval(
            &h,
            r#"local res = C.cordis:get("bus"):waterfall("compact/overflow", {}).res
               local inline, refs = 0, {}
               for _, m in ipairs(A:list_messages().data) do
                   for _, b in ipairs(m.content) do
                       if b.type == "image" then inline = inline + 1 end
                       if b.type == "text" then
                           local p = b.text:match("%[Attachment: (.-) %(read%-only")
                           if p then refs[#refs + 1] = p end
                       end
                   end
               end
               return res.status == "compacted", inline, refs"#,
        )
        .await;
        assert!(ok);
        assert_eq!(inline, 2, "the two most recent images stay");
        assert_eq!(refs.len(), 2, "the older two became file references");
        let mut contents: Vec<String> = refs.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
        contents.sort();
        assert_eq!(contents, ["PNG-1", "PNG-2"], "the files hold the original image bytes");
        assert!(h.kernel.is_module_loaded("compact", "compact.projection"));
        h.kernel.reload(Some("compact")).await.unwrap();
        assert!(!h.kernel.is_module_loaded("compact", "compact.projection"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn offloaded_images_from_reused_positions_keep_their_own_contents() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "stages": ["offload"], "offload_keep_images": 0 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (first_path, second_path, first, second): (String, String, String, String) = eval(
            &h,
            r#"local function offload(bytes)
                 A:set_messages({{ role = "user", content = {
                   { type = "image", mime = "image/png", data = mare.crypto.base64_encode(bytes) }
                 } }})
                 C.cordis:get("bus"):waterfall("compact/overflow", {})
                 return A:list_messages().data[1].content[1].text:match("%[Attachment: (.-) %(read%-only")
               end
               local a = offload("FIRST-IMAGE")
               local b = offload("SECOND-IMAGE")
               return a, b, mare.fs.read(a), mare.fs.read(b)"#,
        )
        .await;
        assert_ne!(first_path, second_path, "different contents must not share an attachment path");
        assert_eq!(first, "FIRST-IMAGE");
        assert_eq!(second, "SECOND-IMAGE");
        h.shutdown().await;
    })
    .await
}

/// A failed image write or projection record must not let the summary discard the image.
#[tokio::test]
async fn image_offload_failures_stop_before_summary_and_leave_context_intact() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "stages": ["local"], "keep_recent_tokens": 1 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "must not be requested" }])).config(cfg).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        let (first, second, requests, intact): (String, String, i64, bool) = eval(
            &h,
            r#"local function history(data)
                 A:set_messages({
                   { role = "user", content = {{ type = "text", text = "look" }, { type = "image", mime = "image/png", data = data }} },
                   { role = "assistant", content = {{ type = "text", text = "seen" }} },
                   { role = "user", content = {{ type = "text", text = "latest" }} },
                 })
               end
               history("")
               local before = mare.json.encode(A:list_messages().data)
               local _, first = pcall(K.run, K)
               local intact1 = before == mare.json.encode(A:list_messages().data)
               history(mare.crypto.base64_encode("PNG"))
               before = mare.json.encode(A:list_messages().data)
               S.append_prune = function() return nil, "disk full" end
               local _, second = pcall(K.run, K)
               return tostring(first), tostring(second), #SENT, intact1 and before == mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(first.contains("failed to offload"), "{first}");
        assert!(second.contains("disk full"), "{second}");
        assert_eq!(requests, 0);
        assert!(intact);
        h.shutdown().await;
    })
    .await
}

/// One estimator (pi compaction.ts:276-288, Codex truncate.rs:71-74): an image is a constant, not
/// its base64; text is bytes / 4 (CJK isn't undercounted); /context's total equals measure().
#[tokio::test]
async fn the_estimate_counts_images_as_a_constant_cjk_by_bytes_and_matches_the_context_breakdown() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (empty, with_image, cjk, total, measured): (i64, i64, i64, i64, i64) = eval(
            &h,
            r#"local empty = K:measure()
               A:set_messages({ { role = "user", content = {
                   { type = "text", text = "screenshot" },
                   { type = "image", mime = "image/png", data = string.rep("A", 2000000) } } } })
               local with_image = K:measure()
               local b = K:measure({ by_source = true })
               return empty, with_image, mare.text.tokens(string.rep("中", 400)), b.total, K:measure()"#,
        )
        .await;
        let image = with_image - empty;
        assert!((1200..1300).contains(&image), "a 2 MB screenshot is ~1.2k tokens, not 500k: {image}");
        assert_eq!(cjk, 300, "400 CJK characters = 1200 bytes / 4");
        assert_eq!(total, measured, "/context and the compaction trigger agree");
        assert!(empty > 500, "the system prompt and tool declarations count even with no messages: {empty}");
        h.shutdown().await;
    })
    .await
}

/// Tool declarations can change in place without changing their names.
#[tokio::test]
async fn context_estimate_tracks_changes_to_same_named_tool_declarations() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "T", h.service_table("tools").unwrap());
        let (before, after, restored): (i64, i64, i64) = eval(
            &h,
            r#"local declaration = { name = "same", description = "short", parameters = { type = "object" } }
               T.definitions = function() return { declaration } end
               local before = K:measure()
               declaration.parameters.description = string.rep("x", 12000)
               local after = K:measure()
               declaration.parameters.description = nil
               return before, after, K:measure()"#,
        )
        .await;
        assert!(after >= before + 3000, "changed schema must affect estimated budget: {before} -> {after}");
        assert_eq!(restored, before, "shrinking the declaration must restore the estimate");
        h.shutdown().await;
    })
    .await
}

/// Pruning shrinks text blocks one by one and keeps images;
/// a result whose images were offloaded is still prunable.
#[tokio::test]
async fn pruning_keeps_images_and_still_prunes_results_whose_images_were_offloaded() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "stages": ["prune"], "prune_tool_result_chars": 2000,
            "prune_head_chars": 500, "prune_tail_chars": 100, "prune_protect_tokens": 10, "prune_min_saving_tokens": 10 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"local big = "HEAD-ü " .. string.rep("ünïcödé ", 2000) .. " TAIL"
               A:set_messages({
                 { role = "user", content = { { type = "text", text = "go" } } },
                 { role = "assistant", content = { { type = "tool_call", id = "a", name = "x", arguments = {} },
                                                   { type = "tool_call", id = "b", name = "x", arguments = {} } } },
                 { role = "tool_result", tool_call_id = "a", content = { { type = "text", text = big },
                     { type = "image", mime = "image/png", data = mare.crypto.base64_encode("PNG") },
                     { type = "text", text = "short note" } } },
                 { role = "tool_result", tool_call_id = "b", pruned = true, content = { { type = "text", text = big },
                     { type = "text", text = "[Attachment: /tmp/x.png (read-only image offloaded to save context; re-read the file to view it)]" } } },
                 { role = "assistant", content = { { type = "text", text = "done" } } },
                 { role = "user", content = { { type = "text", text = "next" } } },
               })"#,
        )
        .await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, a, b): (bool, String, String) = eval(
            &h,
            r#"local res = C.cordis:get("bus"):waterfall("compact/overflow", {}).res
               local m = A:list_messages().data
               return res.status == "compacted", mare.json.encode(m[3].content), mare.json.encode(m[4].content)"#,
        )
        .await;
        let _ = ok;
        assert!(a.contains(r#""type":"image""#) && a.contains("short note"), "other blocks stay: {a}");
        assert!(a.contains("tool result middle pruned") && a.contains("HEAD-ü") && a.contains("TAIL"), "{a}");
        assert!(b.contains("tool result middle pruned") && b.contains("[Attachment:"), "offloaded, then pruned: {b}");
        // a second prune leaves already pruned text alone
        let again: String = eval(&h, r#"C.cordis:get("bus"):waterfall("compact/overflow", {}) return mare.json.encode(A:list_messages().data[3].content)"#).await;
        assert_eq!(again.matches("tool result middle pruned").count(), 1, "{again}");
        h.shutdown().await;
    })
    .await
}

/// Persisted `prune` and `offload` projection entries in the session JSONL both record
/// `tokens_after` alongside `tokens_before`.
#[tokio::test]
async fn prune_and_offload_persist_tokens_after_in_session_journal() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "stages": ["offload", "prune"],
                    "offload_keep_images": 0,
                    "prune_tool_result_chars": 200,
                    "prune_head_chars": 20,
                    "prune_tail_chars": 20,
                    "prune_protect_tokens": 10,
                    "prune_min_saving_tokens": 10
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let path: String = eval(
            &h,
            r#"
            local img = mare.crypto.base64_encode(string.rep("PNGDATA-", 64))
            local big_tool = "HEAD-" .. string.rep("x", 1200) .. "-TAIL"
            A:append({ role = "user", content = { { type = "text", text = "see img" }, { type = "image", mime = "image/png", data = img } } })
            A:append({ role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = {} } } })
            A:append({ role = "tool_result", tool_call_id = "c1", content = { { type = "text", text = big_tool } } })
            A:append({ role = "assistant", content = { { type = "text", text = "done reading" } } })
            A:append({ role = "user", content = { { type = "text", text = "next step" } } })
            A:append({ role = "assistant", content = { { type = "text", text = "continuing" } } })
            C.cordis:get("bus"):waterfall("compact/overflow", {})
            return S:info().path
            "#,
        )
        .await;
        h.shutdown().await;

        let journal = std::fs::read_to_string(&path).unwrap();
        let prunes: Vec<serde_json::Value> = journal
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|e| e["type"] == "prune")
            .collect();
        let offload = prunes.iter().find(|e| e["method"] == "offload").unwrap_or_else(|| panic!("missing offload prune entry: {prunes:?}"));
        let prune = prunes.iter().find(|e| e["method"] == "prune").unwrap_or_else(|| panic!("missing prune entry: {prunes:?}"));
        for (label, entry) in [("offload", offload), ("prune", prune)] {
            let before = entry["tokens_before"].as_i64().unwrap_or_else(|| panic!("{label} missing tokens_before: {entry}"));
            let after = entry["tokens_after"].as_i64().unwrap_or_else(|| panic!("{label} missing tokens_after: {entry}"));
            assert!(after > 0 && after < before, "{label} expected 0 < tokens_after < tokens_before: {entry}");
        }
    })
    .await
}
