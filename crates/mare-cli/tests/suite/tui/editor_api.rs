//! Editor state/edit API (`mare.ui`, `swallow_printable`, minimal Vim prototype, `editor/changed`)
//! and completion registration (`mare.ui.completion.register`, concurrent timeout/stale-seq/release/score merge).

use super::*;

#[tokio::test]
async fn editor_api_reads_moves_replaces_and_emits_editor_changed() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            CHANGED = {}
            PROBE_READY = false
            C.cordis:plugin({
                name = "editor-changed-probe",
                inject = { "bus" },
                apply = function(ctx)
                    PROBE_READY = true
                    ctx:on("editor/changed", function(ev)
                        CHANGED[#CHANGED + 1] = ev
                    end)
                end,
            })
        "#,
        )
        .await;
        t.until_vt("editor probe ready", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return PROBE_READY")).unwrap_or(false)
        })
        .await;

        t.vt.type_str("hello world");
        t.until("typed text", |s| s.contains("hello world")).await;

        let (text, row, col): (String, usize, usize) = eval(
            &t.vt.host,
            r#"
            local st = mare.ui.editor_state()
            return st.text, st.cursor.row, st.cursor.col
        "#,
        )
        .await;
        assert_eq!((text.as_str(), row, col), ("hello world", 0, 11));

        let (after, sel_text, s_col, e_col): (String, String, usize, usize) = eval(
            &t.vt.host,
            r#"
            mare.ui.set_editor_cursor(0, 5)
            mare.ui.editor_insert(",")
            mare.ui.set_editor_selection({ row = 0, col = 7 }, { row = 0, col = 12 })
            local sel = mare.ui.editor_selection()
            local sel_txt = sel and sel.text or ""
            local sc = sel and sel.start.col or -1
            local ec = sel and sel["end"].col or -1
            mare.ui.editor_replace(nil, "mare")
            return mare.ui.get_editor(), sel_txt, sc, ec
        "#,
        )
        .await;
        assert_eq!((after.as_str(), sel_text.as_str(), s_col, e_col), ("hello, mare", "world", 7, 12));

        // Direct range replace + grapheme clamping on multi-byte UTF-8.
        let replaced: String = eval(
            &t.vt.host,
            r#"
            mare.ui.set_editor("你好world")
            -- byte index 1 lands inside '你' (3 bytes): clamped cleanly to char boundary
            mare.ui.set_editor_cursor(0, 1)
            local c = mare.ui.editor_cursor()
            assert(c.col == 0 or c.col == 3, "col snapped: " .. tostring(c.col))
            mare.ui.editor_replace({ start = { row = 0, col = 6 }, ["end"] = { row = 0, col = 11 } }, "世界")
            return mare.ui.get_editor()
        "#,
        )
        .await;
        assert_eq!(replaced, "你好世界");

        t.until_vt("editor/changed delivered", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(
                &t.vt.host,
                r#"return #CHANGED >= 2 and CHANGED[#CHANGED].text == "你好世界" and type(CHANGED[#CHANGED].cursor.col) == "number""#,
            ))
            .unwrap_or(false)
        })
        .await;
    })
    .await
}

#[tokio::test]
async fn minimal_vim_prototype_handles_modes_counts_operators_swallow_and_cjk_ime() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(
            &t.vt.host,
            r#"
            VIM = { mode = "insert", count = "", pending_op = nil, leaked_global = 0 }
            mare.keymap.set("global", "z", function() VIM.leaked_global = VIM.leaked_global + 1 end, { desc = "leak probe" })
            local normal = mare.keymap.context("editor.vim-normal", { parent = "editor", swallow_printable = true })
            VIM.ctx = normal

            local function enter_normal()
                VIM.mode = "normal"
                VIM.count = ""
                VIM.pending_op = nil
                normal:set_active(true)
            end
            local function enter_insert()
                VIM.mode = "insert"
                VIM.count = ""
                VIM.pending_op = nil
                normal:set_active(false)
            end

            mare.keymap.set("editor", "<Esc>", function()
                if VIM.mode == "insert" then
                    enter_normal()
                    return true
                end
                return false
            end, { desc = "vim normal" })

            mare.keymap.set("editor.vim-normal", "i", function()
                enter_insert()
            end, { desc = "vim insert" })

            for d = 1, 9 do
                local ch = tostring(d)
                mare.keymap.set("editor.vim-normal", ch, function()
                    VIM.count = VIM.count .. ch
                end, { desc = "vim count" })
            end

            local function take_count()
                local n = tonumber(VIM.count) or 1
                VIM.count = ""
                return n
            end

            -- word-forward motion (`w`), UTF-8 char-safe
            mare.keymap.set("editor.vim-normal", "w", function()
                local n = take_count()
                local st = mare.ui.editor_state()
                local lines = {}
                for line in (st.text .. "\n"):gmatch("(.-)\n") do
                    lines[#lines + 1] = line
                end
                local r, c = st.cursor.row, st.cursor.col
                local line = lines[r + 1] or ""
                for _ = 1, n do
                    local rest = line:sub(c + 1)
                    local _, ws = rest:find("^%S+%s*")
                    if ws and (c + ws) < #line then
                        c = c + ws
                    else
                        local _, sp = rest:find("^%s+")
                        if sp then
                            c = math.min(#line, c + sp)
                        else
                            c = #line
                        end
                    end
                end
                mare.ui.set_editor_cursor(r, c)
            end, { desc = "vim w" })

            -- 2-key operator sequence: `d d` deletes N lines (`2dd` / `dd`)
            mare.keymap.set("editor.vim-normal", "d d", function()
                local n = take_count()
                local st = mare.ui.editor_state()
                local lines = {}
                for line in (st.text .. "\n"):gmatch("(.-)\n") do
                    lines[#lines + 1] = line
                end
                local row = st.cursor.row + 1
                for _ = 1, n do
                    if #lines >= row then
                        table.remove(lines, row)
                    end
                end
                if #lines == 0 then lines = { "" } end
                local new_text = table.concat(lines, "\n")
                mare.ui.set_editor(new_text)
                mare.ui.set_editor_cursor(math.min(st.cursor.row, #lines - 1), 0)
            end, { desc = "vim dd" })

            -- `x`: delete N UTF-8 characters under cursor
            mare.keymap.set("editor.vim-normal", "x", function()
                local n = take_count()
                local st = mare.ui.editor_state()
                local cur_line = (st.text .. "\n"):match(("[^\n]*\n"):rep(st.cursor.row) .. "([^\n]*)") or ""
                local start_col = st.cursor.col
                local end_col = start_col
                for _ = 1, n do
                    if end_col >= #cur_line then break end
                    local next_pos = utf8.offset(cur_line, 2, end_col + 1)
                    end_col = next_pos and (next_pos - 1) or #cur_line
                end
                if end_col > start_col then
                    mare.ui.editor_replace(
                        { start = { row = st.cursor.row, col = start_col }, ["end"] = { row = st.cursor.row, col = end_col } },
                        ""
                    )
                end
            end, { desc = "vim x" })
        "#,
        )
        .await;

        // 1. Insert mode + CJK IME input works natively
        t.vt.type_str("你好世界 alpha beta gamma");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "你好世界 alpha beta gamma");

        // 2. Esc -> Normal mode; unbound printable keys ('q', 'z') are swallowed (not inserted & not leaked to global)
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.vt.type_str("qz");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "你好世界 alpha beta gamma");
        assert_eq!(eval::<usize>(&t.vt.host, "return VIM.leaked_global").await, 0);

        // 3. Cursor at start -> `2w` jumps 2 words (past "你好世界 " and "alpha ") -> lands at "beta", `x` deletes 'b'
        exec(&t.vt.host, "mare.ui.set_editor_cursor(0, 0)").await;
        t.vt.type_str("2wx");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "你好世界 alpha eta gamma");

        // 4. `x` on CJK character at (0, 0) deletes full UTF-8 character '你' without corruption
        exec(&t.vt.host, "mare.ui.set_editor_cursor(0, 0)").await;
        t.vt.type_str("x");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "好世界 alpha eta gamma");

        // 5. Multi-line `dd`: set 3 lines, press `2dd` -> deletes first 2 lines
        exec(&t.vt.host, r#"mare.ui.set_editor("line1\nline2\nline3"); mare.ui.set_editor_cursor(0, 0)"#).await;
        t.vt.type_str("2dd");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "line3");

        // 6. `i` switches back to Insert mode where printable chars insert normally
        t.vt.type_str("i中文");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "中文line3");
    })
    .await
}

#[tokio::test]
async fn completion_sources_merge_by_score_timeout_discard_stale_and_retract_on_release() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(
            &t.vt.host,
            r##"
            REL_FAST = mare.ui.completion.register({
                name = "fast_hi",
                triggers = { "#" },
                min_prefix = 1,
                score_offset = 100,
                timeout_ms = 500,
                complete = function(ctx)
                    if ctx.token == "#release" then
                        return { { label = "#retractable", insert = "#retractable ", score = 5 } }
                    end
                    return { { label = "#alpha_hi", insert = "#alpha_hi ", score = 1 } }
                end,
            })
            REL_LOW = mare.ui.completion.register({
                name = "fast_lo",
                triggers = { "#" },
                min_prefix = 1,
                score_offset = 10,
                timeout_ms = 500,
                complete = function(ctx)
                    if ctx.token == "#release" then
                        return { { label = "#survivor", insert = "#survivor ", score = 5 } }
                    end
                    return { { label = "#beta_lo", insert = "#beta_lo ", score = 1 } }
                end,
            })
            REL_SLOW = mare.ui.completion.register({
                name = "slow_timeout",
                triggers = { "#" },
                min_prefix = 1,
                score_offset = 500,
                timeout_ms = 40,
                complete = function()
                    mare.task.sleep(300)
                    return { { label = "#too_late", insert = "#too_late " } }
                end,
            })
        "##,
        )
        .await;

        // min_prefix = 1 -> "#" alone does NOT auto-open
        t.vt.type_str("#");
        t.step().await;
        assert!(!t.screen().contains("#alpha_hi"), "min_prefix=1 must wait for >=1 char: {}", t.screen());

        // "#a" triggers fast_hi + fast_lo immediately; slow_timeout skipped; alpha_hi sorted before beta_lo
        t.vt.type_str("a");
        t.until("score-merged completion popup", |s| s.contains("#alpha_hi") && s.contains("#beta_lo")).await;
        let screen = t.screen();
        let hi_pos = screen.find("#alpha_hi").unwrap();
        let lo_pos = screen.find("#beta_lo").unwrap();
        assert!(hi_pos < lo_pos, "higher score_offset must sort first:\n{screen}");
        assert!(!screen.contains("#too_late"), "timed out source must be skipped:\n{screen}");

        // Stale seq discard: slow source for "#ab" arrives AFTER typing "c" triggers "#abc"
        // First Ctrl+C closes the open popup; second clears the draft.
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "#a", "closing completion must keep the draft");
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "");
        exec(
            &t.vt.host,
            r##"
            STALE_FINISHED = false
            REL_SEQ = mare.ui.completion.register({
                name = "seq_probe",
                triggers = { "#" },
                min_prefix = 2,
                timeout_ms = 500,
                complete = function(ctx)
                    if ctx.token == "#ab" then
                        mare.task.sleep(60)
                        STALE_FINISHED = true
                        return { { label = "#STALE_AB", insert = "#STALE_AB " } }
                    elseif ctx.token == "#abc" then
                        return { { label = "#FRESH_ABC", insert = "#FRESH_ABC " } }
                    end
                    return nil
                end,
            })
        "##,
        )
        .await;
        t.vt.type_str("#ab");
        t.vt.type_str("c");
        t.until("fresh #FRESH_ABC shown", |s| s.contains("#FRESH_ABC")).await;
        t.until_vt("stale completion task settled", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return STALE_FINISHED")).unwrap_or(false)
        })
        .await;
        assert!(!t.screen().contains("#STALE_AB"), "stale seq result must be dropped:\n{}", t.screen());
        assert!(t.screen().contains("#FRESH_ABC"), "fresh seq result must stay:\n{}", t.screen());
        exec(&t.vt.host, "REL_SEQ()").await;

        // Clear and test immediate candidate withdrawal on release()
        t.ctrl('c');
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "");
        t.vt.type_str("#release");
        t.until("both release test items open", |s| s.contains("#retractable") && s.contains("#survivor")).await;
        exec(&t.vt.host, "REL_FAST()").await;
        t.until("released source retracted immediately", |s| !s.contains("#retractable") && s.contains("#survivor"))
            .await;
        exec(&t.vt.host, "REL_LOW(); REL_SLOW()").await;
        t.until("all custom items gone", |s| !s.contains("#survivor")).await;

        // Migrated built-in `/` command completion (popup is already closed, one Ctrl+C clears "#release")
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "");
        t.vt.type_str("/he");
        t.until("migrated slash completion", |s| s.contains("/help")).await;
    })
    .await
}

#[tokio::test]
async fn init_reload_removes_swallow_context_before_the_next_key() {
    local(async {
        let env = TestEnv::new().await;
        let init_path = env.config_dir().join("init.lua");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            &init_path,
            r#"mare.keymap.context("editor.init_swallow", {
            parent = "editor", swallow_printable = true, active = true
        })"#,
        )
        .unwrap();
        let mut t = Term::in_env(env, json!([]), json!({}), json!({}), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.type_str("blocked");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "", "active context must swallow printable input");

        std::fs::write(&init_path, "").unwrap();
        t.vt.host.reload_config().await.unwrap();
        // No frame/drain between reload and typing: input must use the fresh shared keymap.
        t.vt.type_str("restored");
        assert_eq!(t.vt.editor_text(), "restored", "typing must resume immediately after reload");
    })
    .await
}

#[tokio::test]
async fn slash_reload_removes_init_swallow_context_and_restores_typing() {
    local(async {
        let env = TestEnv::new().await;
        let init_path = env.config_dir().join("init.lua");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            &init_path,
            r#"mare.keymap.context("editor.init_swallow", {
                parent = "editor", swallow_printable = true, active = true
            })"#,
        )
        .unwrap();
        let mut t = Term::in_env(env, json!([]), json!({}), json!({}), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.type_str("blocked");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "", "the init context must swallow printable keys");

        std::fs::write(&init_path, "").unwrap();
        // Paste is a terminal event and remains available even when printable keys are swallowed.
        t.vt.paste("/reload");
        assert_eq!(t.vt.editor_text(), "/reload");
        // A cursor key ends the terminal's post-paste Enter guard without a timed wait.
        t.key(KeyCode::End, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("reload completion", |s| s.contains("Reloaded ") && s.contains("plugin(s)")).await;
        t.vt.type_str("restored");
        assert_eq!(t.vt.editor_text(), "restored", "the first keys after /reload must reach the editor");
    })
    .await
}

#[tokio::test]
async fn completion_service_method_withdraws_source_on_reload() {
    local(async {
        let env = TestEnv::new().await;
        let init_path = env.config_dir().join("init.lua");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            &init_path,
            r#"
mare.ui.completion.register({
    name = "init_src",
    triggers = { ":" },
    min_prefix = 1,
    complete = function()
        return { { label = ":init_item", insert = ":init_item " } }
    end,
})
"#,
        )
        .unwrap();
        let mut t = Term::in_env(env, json!([]), json!({}), json!({}), 80, 24).await;
        t.vt.type_str(":i");
        t.until("init completion item shown", |s| s.contains(":init_item")).await;

        // Removing registration in init.lua and reloading retracts the source & its popup items
        std::fs::write(&init_path, "").unwrap();
        t.vt.host.reload_config().await.unwrap();
        t.until("reloaded init source retracted immediately", |s| !s.contains(":init_item")).await;

        // owner cleanup via ui service: stopping the caller plugin retracts it
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            TEMP_P = C.cordis:plugin({
                name = "temp-comp-owner",
                inject = { "ui" },
                apply = function(ctx)
                    ctx.ui:completion_register({
                        name = "temp_owned",
                        triggers = { "~" },
                        complete = function() return { { label = "~temp", insert = "~temp " } } end,
                    })
                end,
            })
        "#,
        )
        .await;
        t.until_vt("temp_owned registered", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(
                &t.vt.host,
                r#"for _, m in ipairs(mare.ui.completion.list()) do if m.name == "temp_owned" then return true end end return false"#,
            ))
            .unwrap_or(false)
        })
        .await;
        exec(&t.vt.host, "TEMP_P:dispose()").await;
        t.until_vt("temp_owned retracted on dispose", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(
                &t.vt.host,
                r#"for _, m in ipairs(mare.ui.completion.list()) do if m.name == "temp_owned" then return false end end return true"#,
            ))
            .unwrap_or(false)
        })
        .await;
    })
    .await
}

#[tokio::test]
async fn swallow_printable_restores_on_plugin_dispose_and_grapheme_shell_token_clamp_holds() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            SWALLOW_P = C.cordis:plugin({
                name = "swallow-owner",
                inject = { "ui" },
                apply = function(ctx)
                    local c = ctx.ui:context("editor.vim_own", { parent = "editor", swallow_printable = true, active = true })
                    SWALLOW_CTX = c
                end,
            })
        "#,
        )
        .await;
        t.until_vt("swallow context active", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return SWALLOW_CTX ~= nil and SWALLOW_CTX:is_active()"))
                .unwrap_or(false)
        })
        .await;

        t.vt.type_str("abc");
        t.step().await;
        assert_eq!(eval::<String>(&t.vt.host, "return mare.ui.get_editor()").await, "");

        // Disposing the owning plugin restores normal typing immediately.
        exec(&t.vt.host, "SWALLOW_P:dispose()").await;
        t.until_vt("swallow owner disposed", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return not SWALLOW_CTX:is_active()"))
                .unwrap_or(false)
        })
        .await;
        t.vt.type_str("ok");
        t.until("normal typing restored after dispose", |s| s.contains("ok")).await;

        // Multi-codepoint grapheme cluster clamping (`界ﾞ`, Thai combining `ก้`) + shell `!` coordinate alignment.
        let (c_half, c_thai, shell_col, shell_after): (usize, usize, usize, String) = eval(
            &t.vt.host,
            r#"
            mare.ui.set_editor("界ﾞ")
            -- `界ﾞ` is 6 UTF-8 bytes (1 grapheme cluster); byte 4 lies between `界` and `ﾞ`.
            mare.ui.set_editor_cursor(0, 4)
            local c1 = mare.ui.editor_cursor().col
            mare.ui.set_editor("ก้x")
            -- `ก้` is 6 UTF-8 bytes (2 codepoints, 1 cluster); byte 3 lies before combining mark.
            mare.ui.set_editor_cursor(0, 3)
            local c2 = mare.ui.editor_cursor().col
            mare.ui.set_editor("!ls")
            local st = mare.ui.editor_state()
            mare.ui.editor_replace({ start = { row = 0, col = 1 }, ["end"] = { row = 0, col = 3 } }, "pwd")
            return c1, c2, st.cursor.col, mare.ui.get_editor()
        "#,
        )
        .await;
        assert_eq!(c_half, 0);
        assert_eq!(c_thai, 0);
        assert_eq!(shell_col, 3);
        assert_eq!(shell_after, "!pwd");

        // Atomic paste token snapping via `clamp_pos` when calling `set_editor_cursor` inside placeholder.
        let pasted = (1..=12).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        t.vt.paste(&pasted);
        t.until("paste placeholder shown", |s| s.contains("[Pasted text")).await;
        let snapped_col: usize = eval(
            &t.vt.host,
            r#"
            local text = mare.ui.get_editor()
            local idx = text:find("Pasted text")
            mare.ui.set_editor_cursor(0, idx)
            return mare.ui.editor_cursor().col
        "#,
        )
        .await;
        let placeholder_start: usize = eval(&t.vt.host, "return (mare.ui.get_editor():find('%[Pasted text')) - 1").await;
        assert_eq!(snapped_col, placeholder_start);
    })
    .await
}

#[tokio::test]
async fn completion_error_isolation_path_tab_and_unicode_scalar_min_prefix() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("alpha_file.txt"), "hello").unwrap();
        let dir = env.cwd.join("my dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("笔记.txt"), "hi").unwrap();

        let mut t = Term::in_env(env, json!([]), json!({}), json!({}), 80, 24).await;
        exec(
            &t.vt.host,
            r##"
            mare.ui.completion.register({
                name = "broken_complete",
                triggers = { "#" },
                min_prefix = 1,
                complete = function() error("boom complete") end,
            })
            mare.ui.completion.register({
                name = "broken_enabled",
                triggers = { "#" },
                min_prefix = 1,
                enabled = function() error("boom enabled") end,
                complete = function() return { { label = "#never", insert = "#never" } } end,
            })
            mare.ui.completion.register({
                name = "slow_enabled",
                triggers = { "#" },
                min_prefix = 1,
                timeout_ms = 20,
                enabled = function() mare.task.sleep(500) return true end,
                complete = function() return { { label = "#slow", insert = "#slow" } } end,
            })
            mare.ui.completion.register({
                name = "healthy_unicode",
                triggers = { "#" },
                min_prefix = 1,
                complete = function() return { { label = "#你好ok", insert = "#你好ok " } } end,
            })
        "##,
        )
        .await;

        // With only `#`, `min_prefix = 1` does not fire; typing `#你` (1 scalar, 3 bytes) completes via healthy source
        // even though sibling sources throw or time out inside `enabled`/`complete`.
        t.vt.type_str("#");
        t.step().await;
        assert!(!t.screen().contains("#你好ok"));

        t.vt.type_str("你");
        t.until("healthy source overrides errors and slow enabled", |s| {
            s.contains("#你好ok") && !s.contains("#never") && !s.contains("#slow")
        })
        .await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "#你", "Escape must close completion without changing the draft");
        t.key(KeyCode::Backspace, Modifiers::NONE);
        t.key(KeyCode::Backspace, Modifiers::NONE);

        // Migrated Tab path completion (`./al...` -> `./alpha_file.txt`)
        t.vt.type_str("check ./al");
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("tab path completion shown", |s| s.contains("./alpha_file.txt")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("tab path completion inserts ./alpha_file.txt", |s| s.contains("check ./alpha_file.txt")).await;

        // Migrated `@` quoted path completion with space + CJK filename (`@"my dir/笔记"`)
        t.vt.type_str(" @\"my dir/笔");
        t.until("quoted CJK+space path completion shown", |s| s.contains("@\"my dir/笔记.txt\"")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("quoted CJK+space path inserted", |s| s.contains("check ./alpha_file.txt @\"my dir/笔记.txt\"")).await;
    })
    .await
}

#[tokio::test]
async fn forward_delete_with_at_completion_and_equal_len_replace_emit_editor_changed() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            FD_EVENTS = {}
            FD_READY = false
            mare.ui.completion.register({
                name = "at_probe",
                triggers = { "@" },
                min_prefix = 1,
                complete = function()
                    return { { label = "@ab_match", insert = "@ab_match" }, { label = "@b_match", insert = "@b_match" } }
                end,
            })
            C.cordis:plugin({
                name = "fd-probe",
                inject = { "bus" },
                apply = function(ctx)
                    FD_READY = true
                    ctx:on("editor/changed", function(ev)
                        FD_EVENTS[#FD_EVENTS + 1] = ev
                    end)
                end,
            })
            "#,
        )
        .await;
        t.until_vt("fd probe ready", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return FD_READY")).unwrap_or(false)
        })
        .await;

        t.vt.type_str("@ab");
        t.until("completion popup shown", |s| s.contains("@ab_match")).await;
        t.key(KeyCode::Left, Modifiers::NONE);
        t.key(KeyCode::Left, Modifiers::NONE);
        t.step().await;
        let rev_before: u64 = eval(&t.vt.host, "return mare.ui.editor_state().revision").await;

        // Forward-Delete removes 'a' while leaving cursor at col 1 under active `@` completion.
        t.key(KeyCode::Delete, Modifiers::NONE);
        t.until_vt("editor/changed fired for forward-delete under completion", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(
                &t.vt.host,
                &format!(
                    "local n = #FD_EVENTS; return n > 0 and FD_EVENTS[n].text == '@b' and FD_EVENTS[n].revision > {rev_before}"
                ),
            ))
            .unwrap_or(false)
        })
        .await;
        assert_eq!(eval::<String>(&t.vt.host, "return mare.ui.editor_state().text").await, "@b");

        // Equal-length Lua replace with unchanged cursor also advances revision and emits editor/changed.
        let rev_mid: u64 = eval(&t.vt.host, "return mare.ui.editor_state().revision").await;
        exec(
            &t.vt.host,
            r#"
            mare.ui.editor_replace({ start = { row = 0, col = 0 }, ["end"] = { row = 0, col = 2 } }, "@z")
            mare.ui.set_editor_cursor(0, 1)
            "#,
        )
        .await;
        t.until_vt("editor/changed fired for equal-length replace", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(
                &t.vt.host,
                &format!(
                    "local n = #FD_EVENTS; return n > 0 and FD_EVENTS[n].text == '@z' and FD_EVENTS[n].revision > {rev_mid}"
                ),
            ))
            .unwrap_or(false)
        })
        .await;
        assert_eq!(eval::<String>(&t.vt.host, "return mare.ui.editor_state().text").await, "@z");
    })
    .await
}

#[tokio::test]
async fn slash_command_popup_clears_when_arguments_and_enter_arrive_together() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        t.vt.type_str("/lua");
        t.until("command completion popup open", |s| s.contains("/lua")).await;

        t.vt.type_str(" print('ARG_BURST_OK')");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("lua command executed instead of accepting stale completion", |s| s.contains("ARG_BURST_OK")).await;
    })
    .await
}
