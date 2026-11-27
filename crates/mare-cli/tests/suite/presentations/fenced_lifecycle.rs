use crate::common::*;

#[tokio::test]
async fn owned_fenced_presenters_snapshot_deduplicate_and_restore_on_dispose() {
    local(async {
        let env = TestEnv::new().await;
// owners are real plugin ctxs: O the main owner, P the provider that goes, D independent
        ctx_probes(&env, &["o", "p", "d"]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, "O, P, D = mare._pctx.o, mare._pctx.p, mare._pctx.d").await;
        exec(&h, r##"
            local settled_tx, settled_rx = mare.task.channel()
            local function watch_doc()
                return function(_, _, state)
                    if state.pending == 0 then
                        settled_tx:send(state.revision)
                    end
                end
            end
            function doc_wait(owner, id, min_revision)
                while true do
                    local value = owner.ui:get_markdown(id)
                    if value and value.pending == 0 and (not min_revision or value.revision >= min_revision) then
                        return value
                    end
                    settled_rx:recv()
                end
            end
            source = "> ```DRAW title\n> same\n> ```\n\n```draw\nsame\n```"
            low = O.ui:markdown_presenter("draw", {render=function(r) return mare.ui.Code("lower:"..r.source,"text") end})
            renders, shared = 0, mare.ui.Code("upper", "text")
            P.ui:markdown_presenter("DRAW", {render=function(r)
                assert(r.session_id and r.session_epoch and r.width > 0)
                assert(r.language == "draw" and r.phase == "closed")
                renders = renders + 1
                last_theme = r.theme
                return shared
            end})
            O.ui:render_markdown("same-id", source, {on_update=watch_doc()})
            O.ui:render_markdown("same-id", source.."\ntrailing", {on_update=watch_doc()})
            local current = doc_wait(O, "same-id")
            assert(renders == 2, "unchanged occurrences must retain their work")
            assert(#current.node.blocks == 2 and current.node.blocks[1].start ~= current.node.blocks[2].start)
            shared.code = "mutated after delivery"
            assert(O.ui:get_markdown("same-id").node.blocks[1].node.code == "upper")
            O.ui:render_markdown("same-id", "```draw\nother\n```", {base_node=mare.ui.Indent("x ", mare.ui.Markdown("```draw\nother\n```")), on_update=watch_doc()})
            assert(doc_wait(O,"same-id").node.type == "indent")
            D.ui:render_markdown("same-id", "```draw\nindependent owner\n```", {replay=true, on_update=watch_doc()})
            assert(doc_wait(D,"same-id").source:find("independent owner",1,true))
            assert(O.ui:get_markdown("same-id").source:find("other",1,true))

            local shared_image = mare.ui.image({bytes=mare.crypto.base64_decode("iVBORw0KGgoAAAANSUhEUgAAACAAAAAQCAYAAAB3AH1ZAAAAJElEQVR4nGOQ23Lp/0BihlEHjDpg1AGjDhh1wKgDRh0w0A4AALXQRkwBCanJAAAAAElFTkSuQmCC")})
            local original_image_id = shared_image.image_id
            O.ui:markdown_presenter("cached-image",{render=function() return shared_image end})
            O.ui:render_markdown("cached-images","```cached-image\nx\n```\n\n```cached-image\nx\n```",{viewport={supports_images=true}, on_update=watch_doc()})
            local cached = doc_wait(O,"cached-images").node.blocks
            assert(cached[1].node.image_id ~= cached[2].node.image_id)
            assert(shared_image.image_id == original_image_id, "provider cache must not be mutated")
            assert(O.ui:get_markdown("cached-images").node.blocks[1].node.image_id == cached[1].node.image_id)

            local png_bytes = mare.crypto.base64_decode("iVBORw0KGgoAAAANSUhEUgAAACAAAAAQCAYAAAB3AH1ZAAAAJElEQVR4nGOQ23Lp/0BihlEHjDpg1AGjDhh1wKgDRh0w0A4AALXQRkwBCanJAAAAAElFTkSuQmCC")
            O.ui:markdown_presenter("dual-zoom", {render=function()
                return {
                    compact = mare.ui.image({bytes=png_bytes, max_rows=10, max_cols=40}),
                    expanded = mare.ui.image({bytes=png_bytes, max_rows=120, max_cols=80}),
                }
            end})
            O.ui:render_markdown("dual-zoom-doc", "```dual-zoom\nzoom\n```", {viewport={supports_images=true}, on_update=watch_doc()})
            local dual = doc_wait(O, "dual-zoom-doc")
            assert(dual.node.blocks[1].node.max_rows == 10)
            assert(dual.expanded_node.blocks[1].node.items[1].max_rows == 120)
            assert(dual.expanded_node.blocks[1].node.items[2].type == "code")
            local before_theme = O.ui:get_markdown("same-id").revision
            C.cordis:get('theme'):select("light")
            local themed = doc_wait(O,"same-id", before_theme + 1)
            assert(last_theme.active == "light" and last_theme.code_background)
            local stable_revision = themed.revision
            C.cordis:get("bus"):emit("ui/reflow", {rows=0})
            assert(O.ui:get_markdown("same-id").revision == stable_revision, "unchanged reflow must not loop")
            mare.ui.style("MareCodeBlock", {bg="#123456"})
            C.cordis:get("bus"):emit("ui/reflow", {rows=0})
            assert(doc_wait(O,"same-id", stable_revision + 1).revision > stable_revision and last_theme.code_background == "#123456")
            assert(not pcall(function() O.ui:render_markdown("bad", "x", {viewport={width=0}}) end))
            assert(not pcall(function() O.ui:render_markdown("bad", "x", {surprise=true}) end))
            local before_invalid = O.ui:get_markdown("same-id")
            assert(not pcall(function() O.ui:render_markdown("same-id", "must not publish", {base_node=function()end}) end))
            local after_invalid = O.ui:get_markdown("same-id")
            assert(before_invalid.source == after_invalid.source and before_invalid.revision == after_invalid.revision)
            C_REV_BEFORE_DISPOSE = after_invalid.revision
            D_REV_BEFORE_DISPOSE = D.ui:get_markdown("same-id").revision
        "##).await;
        // the provider plugin goes (its registrations with it) while its owners keep running: a
        // rows-driven unload, not a user's full /reload (which restarts O and D too, 520e66ea)
        unload_plugin(&h.kernel, "ctx-p").await;
        assert!(!plugin_active(&h.kernel, "ctx-p"));
        let restored: bool = eval(&h, r##"
            assert(O == mare._pctx.o and D == mare._pctx.d, "document owners must remain active")
            assert(not pcall(function() return P.ui end), "provider context must be disposed")
            local current = doc_wait(O,"same-id", C_REV_BEFORE_DISPOSE + 1)
            assert(current.node.child.type == "presented_markdown")
            assert(current.node.child.blocks[1].node.code:find("lower:",1,true))
            assert(doc_wait(D,"same-id", D_REV_BEFORE_DISPOSE + 1).node.blocks[1].node.code:find("lower:",1,true))
            low()
            assert(#O.ui:get_markdown("same-id").node.child.blocks == 0)
            local presenters = O.ui:list_markdown_presenters()
            assert(#presenters.data >= 1 and presenters.data[1].language)
            assert(O.ui:delete_markdown("same-id"))
            local ok_missing, err_missing = pcall(function() return O.ui:get_markdown("same-id") end)
            assert(not ok_missing and err_missing.code == "ui.unknown_document", tostring(err_missing))
            assert(D.ui:get_markdown("same-id"))
            return true
        "##).await;
        assert!(restored);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn supersede_cancel_timeout_and_invalid_nodes_keep_source() {
    local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["o"]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, "O = mare._pctx.o").await;
        exec(
            &h,
            r##"
            local settled_tx, settled_rx = mare.task.channel()
            local function watch_doc()
                return function(_, _, state)
                    if state.pending == 0 then
                        settled_tx:send(state.revision)
                    end
                end
            end
            function wait(id, min_revision)
                while true do
                    local value = O.ui:get_markdown(id)
                    if value and value.pending == 0 and (not min_revision or value.revision >= min_revision) then
                        return value
                    end
                    settled_rx:recv()
                end
            end
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local done_tx, done_rx = mare.task.channel()
            old_token = nil
            O.ui:markdown_presenter("delayed", {render=function(r)
                if r.source:find("old",1,true) then
                    old_token = r.cancel
                    entered_tx:send(true)
                    release_rx:recv()
                    done_tx:send(true)
                end
                return mare.ui.Code(r.source,"text")
            end})
            O.ui:render_markdown("edit", "```delayed\nold\n```", {on_update=watch_doc()})
            assert(entered_rx:recv())
            O.ui:render_markdown("edit", "```delayed\nnew\n```", {on_update=watch_doc()})
            assert(old_token:is_cancelled())
            assert(wait("edit").node.blocks[1].node.code == "new\n")
            release_tx:send(true)
            assert(done_rx:recv())
            assert(wait("edit").node.blocks[1].node.code == "new\n")
            O.ui:render_markdown("cancel", "```delayed\nold\n```", {on_update=watch_doc()})
            assert(entered_rx:recv())
            assert(O.ui:cancel_markdown("cancel"))
            C.cordis:get("bus"):emit("ui/presentation-invalidate", {})
            release_tx:send(true)
            assert(done_rx:recv())
            local cancelled = wait("cancel")
            assert(cancelled.pending == 0 and not cancelled.enhanced and #cancelled.node.blocks == 0)
            O.ui:markdown_presenter("markdown-image", {render=function() return mare.ui.Markdown("![local](/not-read)") end})
            O.ui:render_markdown("markdown-image", "```markdown-image\ncomplete source\n```", {on_update=watch_doc()})
            assert(not wait("markdown-image").enhanced)
            local timeout_tx, timeout_rx = mare.task.channel()
            O.ui:markdown_presenter("invalid", {render=function() return {type="unknown"} end})
            O.ui:markdown_presenter("errors", {render=function() error("renderer failure") end})
            O.ui:markdown_presenter("timeout", {timeout_ms=5,render=function(r)
                timeout_rx:recv()
                return mare.ui.Code("late", "text")
            end})
            for _, language in ipairs({"invalid","errors","timeout"}) do
                local source = "```"..language.."\ncomplete source\n```"
                O.ui:render_markdown(language,source,{on_update=watch_doc()})
                local result = wait(language)
                assert(result.source == source and not result.enhanced and #result.node.blocks == 0)
            end
            timeout_tx:send(true)
            local old_generation = O.ui:get_markdown("edit").render_seq
            local late_updates = 0
            O.ui:render_markdown("old-session", "```delayed\nold\n```", {on_update=function() late_updates=late_updates+1 end})
            assert(entered_rx:recv())
            C.cordis:get("session"):create()
            assert(old_token:is_cancelled())
            release_tx:send(true)
            assert(done_rx:recv())
            for _, dropped in ipairs({"edit", "old-session"}) do
                local ok_drop, err_drop = pcall(function() return O.ui:get_markdown(dropped) end)
                assert(not ok_drop and err_drop.code == "ui.unknown_document", tostring(err_drop))
            end
            O.ui:render_markdown("new-session", "```delayed\nnew\n```", {on_update=watch_doc()})
            assert(wait("new-session").render_seq > old_generation)
            assert(late_updates == 0, "old-session delivered late update after session/new")
        "##,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

// Adapted from DeepSeek Harness `python/sdk/tests/test_client.py:506`
#[tokio::test]
async fn render_done_correlates_unique_requests_across_same_block_rerender() {
    local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["o"]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, "O = mare._pctx.o").await;
        let ok: bool = eval(
            &h,
            r##"
            local entered_tx, entered_rx = mare.task.channel()
            local release_a_tx, release_a_rx = mare.task.channel()
            local release_b_tx, release_b_rx = mare.task.channel()
            local done_tx, done_rx = mare.task.channel()
            C.cordis:on("ui/presentation-render-completed", function(ev)
                done_tx:send(ev)
            end)
            local reqs = {}
            O.ui:markdown_presenter("rerender", {
                render = function(r)
                    reqs[#reqs + 1] = {
                        request_id = r.request_id,
                        block_id = r.block_id,
                        generation = r.generation,
                        revision = r.revision,
                    }
                    entered_tx:send(#reqs)
                    if #reqs == 1 then
                        release_a_rx:recv()
                    else
                        release_b_rx:recv()
                    end
                    return mare.ui.Code("ok:" .. tostring(r.revision), "text")
                end,
            })
            O.ui:render_markdown("doc", "```rerender\nsame\n```")
            assert(entered_rx:recv() == 1)
            -- Force a same-block, same-generation rerender at a new revision while A is in-flight.
            C.cordis:get('theme'):select("light")
            assert(entered_rx:recv() == 2)
            local a, b = reqs[1], reqs[2]
            assert(a.request_id ~= nil and b.request_id ~= nil and a.request_id ~= b.request_id, "unique request_id required")
            assert(a.block_id == b.block_id and a.render_seq == b.render_seq and a.revision ~= b.revision)
            release_a_tx:send(true)
            local ev1 = done_rx:recv()
            assert(ev1.request_id == a.request_id and ev1.revision == a.revision and ev1.accepted == false)
            release_b_tx:send(true)
            local ev2 = done_rx:recv()
            assert(ev2.request_id == b.request_id and ev2.revision == b.revision and ev2.accepted == true)
            return true
            "##,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn external_plugin_can_invalidate_presentations_via_ui_service() {
    local(async {
        let env = TestEnv::new().await;
        ctx_probes(&env, &["ext"]);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, "EXT = mare._pctx.ext").await;
        let ok: bool = eval(
            &h,
            r##"
            local invalidations = 0
            C.cordis:on("ui/presentation-invalidate", function()
                invalidations = invalidations + 1
            end)
            EXT.ui:invalidate_presentations()
            assert(invalidations == 1, "invalidate_presentations must emit ui/presentation-invalidate")
            return true
            "##,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}
