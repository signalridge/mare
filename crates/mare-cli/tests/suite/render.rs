//! Render snapshots of the Lua UI components through the real renderer (Codex tui insta
//! snapshots): layout regressions show up as reviewable `.snap` diffs. Plain text (styles are
//! named and themeable, so they're not part of the snapshot). Update: `cargo insta review` or
//! `INSTA_UPDATE=always cargo test -p mare-cli --test all render::`.

use crate::common::*;

/// Render a Lua expression producing a node at `width` columns (inside a fresh host).
async fn render(env: &TestEnv, width: usize, setup: &str, expr: &str) -> String {
    let h = env.host().no_session().build().await;
    exec(&h, setup).await;
    let out: String = eval(&h, &format!("return mare.ui.render_text({expr}, {width})")).await;
    h.shutdown().await;
    out.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

macro_rules! snap {
    ($name:ident, $width:expr, $setup:expr, $expr:expr) => {
        #[tokio::test]
        async fn $name() {
            local(async {
                let env = TestEnv::new().await;
                insta::assert_snapshot!(render(&env, $width, $setup, $expr).await);
            })
            .await
        }
    };
}

const CALL: &str = r#"CALL = { id = "c1", name = "probe", arguments = { command = "cargo test" } }
TEXT = "running 6 tests\ntest a ... ok\ntest b ... ok\ntest c ... ok\ntest d ... ok\ntest result: ok"
RESULT = { content = { { type = "text", text = TEXT } } }
INFO = { summary = "Probe(cargo test)", text = TEXT }"#;

snap!(user_message, 60, "", r#"ui.UserMessage:render("fix the failing test in src/lib.rs")"#);
snap!(tool_call_compact, 60, CALL, "ui.ToolCall:render(CALL, RESULT, false, INFO)");
snap!(tool_call_expanded, 60, CALL, "ui.ToolCall:render(CALL, RESULT, true, INFO)");
snap!(
    word_diff,
    60,
    "",
    r#"ui.Diff({ path = "src/a.rs", old = "let x = 1;\nlet y = 2;\n", new = "let x = 10;\nlet y = 2;\n" })"#
);
snap!(
    block_list_with_footer,
    40,
    "",
    r#"ui.Block(ui.List({ "alpha", "beta", "gamma", "delta", "epsilon" }, { selected = 2, height = 3, focused = true }),
         { title = "Models", footer = "2 of 5" })"#
);
snap!(
    split_side_by_side,
    100,
    "",
    r#"ui.Split(ui.List({ "one", "two" }, { selected = 1 }), ui.Paragraph("preview of one"))"#
);
snap!(
    split_stacks_when_narrow,
    60,
    "",
    r#"ui.Split(ui.List({ "one", "two" }, { selected = 1 }), ui.Paragraph("preview of one"))"#
);
snap!(
    status_segments_nerd_font,
    60,
    r#"ui.set_theme_data({ icons = "nerd" })"#,
    r#"ui.Line(ui.Segments({ { text = "auto", style = "MareModeAuto" }, { text = "opus" } }, "left"))"#
);
snap!(
    status_segments_ascii,
    60,
    r#"ui.set_theme_data({ icons = "ascii" })"#,
    r#"ui.Line(ui.Segments({ { text = "auto", style = "MareModeAuto" }, { text = "opus" } }, "left"))"#
);

// brand: pixel logo (body + one-pixel drop shadow) and its one-line fallback
const INFO_BRAND: &str = r#"BRAND = { version = "0.1.0", cwd = "~/src/mare", model = "openai/gpt-x" }"#;
snap!(logo_full, 60, INFO_BRAND, "ui.Logo:render(BRAND)");
snap!(logo_compact, 60, INFO_BRAND, "ui.Logo:compact(BRAND)");

#[tokio::test]
async fn full_logo_has_a_top_margin_but_compact_logo_does_not() {
    local(async {
        let env = TestEnv::new().await;
        let full = render(&env, 60, INFO_BRAND, "ui.Logo:render(BRAND)").await;
        let compact = render(&env, 60, INFO_BRAND, "ui.Logo:compact(BRAND)").await;
        assert!(full.starts_with('\n'), "the full logo needs one blank line above it: {full:?}");
        assert!(!compact.starts_with('\n'), "the compact form must stay one line: {compact:?}");
    })
    .await
}

/// A single tool call reads in the present tense while it runs and in the past tense once done
/// tools without a verb keep their `Name(args)` form.
#[tokio::test]
async fn tool_call_header_switches_from_present_to_past_tense() {
    local(async {
        let env = TestEnv::new().await;
        let head = |summary: &str, state: &str| format!("ui.ToolCall:header({summary:?}, {state:?})");
        let running = render(&env, 60, "", &head("Read(src/lib.rs)", "running")).await;
        let done = render(&env, 60, "", &head("Read(src/lib.rs)", "ok")).await;
        let edited = render(&env, 60, "", &head("Edit(a.rs)", "error")).await;
        let other = render(&env, 60, "", &head("Probe(cargo test)", "ok")).await;
        assert!(running.contains("Reading src/lib.rs"), "{running:?}");
        assert!(done.contains("● Read src/lib.rs") && !done.contains("Reading"), "{done:?}");
        assert!(edited.contains("Edited a.rs"), "{edited:?}");
        assert!(other.contains("Probe(cargo test)"), "{other:?}");
    })
    .await
}

/// `icons = "auto"` follows the capability table: a terminal that draws emoji one cell wide
/// (`emoji_wide = false`, e.g. Terminal.app before macOS Tahoe) gets the ASCII icons.
#[tokio::test]
async fn auto_icons_switch_to_ascii_when_emoji_are_narrow() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (narrow, wide): (String, String) = eval(
            &h,
            "return mare.ui.detect_icons({ emoji_wide = false }), mare.ui.detect_icons({ emoji_wide = true })",
        )
        .await;
        assert_eq!((narrow.as_str(), wide.as_str()), ("ascii", "unicode"));
        h.shutdown().await;
    })
    .await
}

/// local backgrounds follow the real terminal background (OSC 11 → ui/background {r,g,b});
/// the terminal background and body text color are never set (MareText stays empty)
#[tokio::test]
async fn theme_blends_local_backgrounds_from_terminal_background() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        for (r, g, b) in [(0u8, 0u8, 0u8), (16, 32, 48)] {
            h.emit_json("ui/background", serde_json::json!({ "dark": true, "r": r, "g": g, "b": b })).await;
            let (add, sel, code, text): (String, String, String, bool) = eval(
                &h,
                r#"local function bg(n) local s = mare.ui.style(n) or {}; return tostring(s.bg):lower() end
                   local t = mare.ui.style("MareText") or {}
                   return bg("MareDiffAdd"), bg("MareSelection"), bg("MareCodeBlock"), t.fg == nil and t.bg == nil"#,
            )
            .await;
            let term = format!("#{r:02x}{g:02x}{b:02x}");
            let ch = |c: &str, i: usize| u8::from_str_radix(&c[i..i + 2], 16).unwrap();
            assert!(
                code.starts_with('#') && [1, 3, 5].iter().all(|&i| ch(&code, i) <= ch(&term, i)),
                "code blocks stay on or just below the terminal background {term}: {code}"
            );
            assert!(add.starts_with('#') && add != term, "diff-add is tinted away from {term}: {add}");
            assert!(sel.starts_with('#') && sel != term && sel != add, "selection stands out from {term}: {sel}");
            assert!(text, "MareText must not set colors");
        }
        h.shutdown().await;
    })
    .await
}

/// palette keys are extendable (`colors` option) and `theme/palette` can adjust them;
/// accent2 is the one key behind the search border, inline code and logo shadow
#[tokio::test]
async fn theme_palette_is_extendable_and_hookable() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(serde_json::json!({ "plugins": { "theme-default": { "colors": { "dark": { "accent2": "#123456" } } } } }))
            .no_session()
            .build()
            .await;
        let (code, search, shadow): (String, String, String) = eval(
            &h,
            r#"local function fg(n) return tostring((mare.ui.style(n) or {}).fg) end
               return fg("MareInlineCode"), fg("MareBorderSearch"), fg("MareLogoShadow")"#,
        )
        .await;
        for got in [&code, &search, &shadow] {
            assert_eq!(got.to_lowercase(), "#123456", "colors option overrides a palette key");
        }
        expose(&h);
        exec(&h, r##"C.cordis:on("theme/palette", function(data, ev) data.palette.accent = "#abcdef"; return ev.next(data) end)"##).await;
        h.emit_json("ui/background", serde_json::json!({ "dark": true, "r": 0, "g": 0, "b": 0 })).await;
        let accent: String = eval(&h, r#"return tostring((mare.ui.style("MareAccent") or {}).fg)"#).await;
        assert_eq!(accent.to_lowercase(), "#abcdef", "theme/palette hook applied");
        h.shutdown().await;
    })
    .await
}

/// Auto chooses a configured pair; runtime changes and unknown names share fixed-flavor behavior.
#[tokio::test]
async fn theme_auto_uses_configured_pair_and_tracks_runtime_changes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(serde_json::json!({
                "flavor_dark": "night", "flavor_light": "day",
                "plugins": { "theme-default": { "flavors": {
                    "night": { "MareAccent": { "fg": "#123456" } },
                    "day": { "MareAccent": { "fg": "#abcdef" } }
                } } }
            }))
            .no_session()
            .build()
            .await;
        let accent =
            async || -> String { eval(&h, r#"return tostring(mare.ui.style("MareAccent").fg):lower()"#).await };
        assert_eq!(accent().await, "#123456", "unknown background uses the dark side");
        h.emit_json("ui/background", serde_json::json!({ "dark": false })).await;
        assert_eq!(accent().await, "#abcdef");
        exec(&h, r#"mare.o.flavor_light = "night""#).await;
        wait_until(WAIT_SECS, "pair change", async || accent().await == "#123456").await;
        exec(&h, r#"mare.o.flavor_light = "auto""#).await;
        wait_until(WAIT_SECS, "invalid pair restores prior choice", async || {
            eval::<String>(&h, r#"return mare.o.flavor_light"#).await == "night"
        })
        .await;
        expose(&h);
        exec(&h, r#"C.cordis:get("theme"):select("day")"#).await;
        assert_eq!(accent().await, "#abcdef", "fixed selection ignores the pair");
        exec(&h, r#"assert(not pcall(function() C.cordis:get("theme"):select("missing") end))"#).await;
        assert_eq!(accent().await, "#abcdef", "unknown selection is rejected without changing styles");
        exec(&h, r#"mare.o.flavor_dark = "day"; C.cordis:get("theme"):select("auto")"#).await;
        h.emit_json("ui/background", serde_json::json!({ "dark": true })).await;
        assert_eq!(accent().await, "#abcdef");
        exec(&h, r##"mare.ui.style("MareAccent", { fg = "#987654" }); mare.o.flavor_dark = "night""##).await;
        wait_until(WAIT_SECS, "explicit override survives switching", async || accent().await == "#987654").await;
        h.shutdown().await;
    })
    .await
}

/// WCAG 2 contrast ratio of two #rrggbb colors.
fn contrast(a: &str, b: &str) -> f64 {
    fn lum(h: &str) -> f64 {
        let ch = |i: usize| {
            let v = u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(1) + 0.7152 * ch(3) + 0.0722 * ch(5)
    }
    let (x, y) = (lum(a), lum(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// text and the gray foregrounds reach their contrast floors on the measured
/// background (Catppuccin, Dracula #282a36, Latte, white), keep text > muted > dim > subtle, and
/// the selected row stands out (≥ 1.6) while body text on it stays readable (≥ 5)
#[tokio::test]
async fn gray_foregrounds_reach_their_contrast_floors_on_common_backgrounds() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        for (bg, dark) in [("#1e1e2e", true), ("#282a36", true), ("#eff1f5", false), ("#ffffff", false)] {
            let n = |i: usize| u8::from_str_radix(&bg[i..i + 2], 16).unwrap();
            h.emit_json("ui/background", serde_json::json!({ "dark": dark, "r": n(1), "g": n(3), "b": n(5) })).await;
            // MareFileDoc is the palette's `text`
            let (text, muted, dim, subtle, border, sel): (String, String, String, String, String, String) = eval(
                &h,
                r#"local function fg(n) return tostring((mare.ui.style(n) or {}).fg):lower() end
                   return fg("MareFileDoc"), fg("MareMuted"), fg("MareDim"), fg("MareSubtle"), fg("MareBorder"),
                       tostring((mare.ui.style("MareSelection") or {}).bg):lower()"#,
            )
            .await;
            let (ct, csel, cts) = (contrast(&text, bg), contrast(&sel, bg), contrast(&text, &sel));
            assert!(ct >= 4.5, "text {text} on {bg}: {ct:.2} < 4.5");
            assert!(csel >= 1.6, "selection {sel} on {bg}: {csel:.2} < 1.6");
            assert!(cts >= 5.0, "text {text} on selection {sel} ({bg}): {cts:.2} < 5");
            let (fm, fd) = if dark { (7.0, 4.5) } else { (5.5, 4.5) };
            let (cm, cd, cs, cb) =
                (contrast(&muted, bg), contrast(&dim, bg), contrast(&subtle, bg), contrast(&border, bg));
            assert!(cm >= fm, "muted {muted} on {bg}: {cm:.2} < {fm}");
            assert!(cd >= fd, "dim {dim} on {bg}: {cd:.2} < {fd}");
            assert!(cs >= 3.0, "subtle {subtle} on {bg}: {cs:.2} < 3");
            assert!(cb >= 3.0, "border {border} on {bg}: {cb:.2} < 3");
            assert!(
                ct > cm && cm > cd && cd > cs,
                "text > muted > dim > subtle on {bg}: {ct:.2} {cm:.2} {cd:.2} {cs:.2}"
            );
        }
        h.shutdown().await;
    })
    .await
}

/// Q5: the Rust fallback palette (`Theme::default()`: the frame before any Lua runs, the
/// startup-failure dialog, `theme-default` turned off) and `theme-default`'s dark flavor agree
/// on every style both define, so the fallback never shows a different look.
#[tokio::test]
async fn the_rust_fallback_palette_matches_theme_default_dark() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().config(serde_json::json!({ "flavor": "dark" })).no_session().build().await;
        let lua = h.ui.theme.read().unwrap().clone();
        let rust = mare_shared::style::Theme::default();
        let mut drift = vec![];
        for name in rust.names() {
            if let Some(l) = lua.styles.get(&name) {
                let r = rust.styles[&name];
                if (r.fg, r.bg, r.bold, r.italic, r.underline) != (l.fg, l.bg, l.bold, l.italic, l.underline) {
                    drift.push(format!("{name}: rust {r:?} lua {l:?}"));
                }
            }
        }
        assert!(drift.is_empty(), "{} styles drifted:\n{}", drift.len(), drift.join("\n"));
        h.shutdown().await;
    })
    .await
}

/// The core-drawn spinner's glyphs are theme data (`mare.o.spinner` → `ui.set_theme_data`):
/// the default is the braille set, an override takes effect, and resetting restores the default.
#[tokio::test]
async fn the_spinner_glyphs_are_theme_data_with_a_braille_default() {
    local(async {
        let env = TestEnv::new().await;
        let spin = r#"ui.Spinner(ui.Line({ ui.Span("work") }))"#;
        assert_eq!(render(&env, 40, "", spin).await, "⠋ work", "default unchanged");
        assert_eq!(
            render(&env, 40, r#"mare.ui.set_theme_data({ glyphs = { frames = { "-", "+" }, still = "o" } })"#, spin)
                .await,
            "- work"
        );
        // the next section of a run: back to the built-in glyphs
        let reset =
            r#"mare.ui.set_theme_data({ glyphs = { frames = { "-" } } }); mare.ui.set_theme_data({ glyphs = false })"#;
        assert_eq!(render(&env, 40, reset, spin).await, "⠋ work");
        // an empty list or an empty glyph is not a spinner: the built-in stays
        assert_eq!(render(&env, 40, r#"mare.ui.set_theme_data({ glyphs = {} })"#, spin).await, "⠋ work");
        // four independent fields: changing border/markdown leaves icons/glyphs untouched
        let check_indep = r#"
            mare.ui.set_theme_data({ icons = "ascii", glyphs = { frames = { "*", "+" }, still = "!" } })
            mare.ui.set_theme_data({ border = "double", markdown = { code_line_numbers = true } })
            assert(mare.ui.icon_set() == "ascii")
        "#;
        assert_eq!(render(&env, 40, check_indep, spin).await, "* work");
        // the option reaches the theme through the `ui` plugin
        let h = env.host().config(serde_json::json!({ "spinner": ["a", "b", "c"] })).no_session().build().await;
        assert_eq!(h.ui.theme.read().unwrap().spinner, ["a", "b", "c"]);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn theme_registry_preview_cancel_and_owned_layers() {
    local(async {
        let env = TestEnv::new().await;
        let themes = env.home.path().join("config/themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("custom syntax.tmTheme"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>settings</key><array><dict><key>settings</key><dict><key>foreground</key><string>#123456</string></dict></dict></array></dict></plist>"#).unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, r##"
            local theme = C.cordis:get("theme")
            assert(theme, "public theme contract available")
            local discovered = false
            for _, d in ipairs(theme:list().data) do if d.name == "custom syntax" then discovered = true end end
            assert(discovered, "valid custom tmTheme discovered in public config directory")
            theme:preview("custom syntax")
            theme:cancel_preview()
            local before = theme:get()
            local remove = theme:register({name="test-custom", base="dark", icons={file="F"}, styles={TestOnly={fg="#123456"}}})
            local snapshot = theme:preview("test-custom")
            assert(snapshot.preference == before.preference and snapshot.active == "test-custom")
            assert(mare.ui.icon("file") == "F")
            snapshot.styles.TestOnly.fg = "#ffffff"
            assert(theme:get().styles.TestOnly.fg == "#123456", "snapshots must be immutable copies")
            theme:cancel_preview()
            assert(theme:get().active == before.active)
            assert(mare.ui.style("TestOnly").fg == nil, "preview-only styles withdrawn")
            assert(not pcall(function() theme:select("does-not-exist") end))
            assert(theme:get().preference == before.preference)
            assert(not pcall(function() theme:register({name="test-custom", palette={}}) end))
            local old = theme:register_override("test-layer", {OverrideOnly={fg="#abcdef"}})
            local new = theme:register_override("test-layer", {OverrideOnly={fg="#fedcba"}})
            old()
            assert(mare.ui.style("OverrideOnly").fg == "#fedcba")
            local found = false
            for _, token in ipairs(theme:inspect()) do if token.name == "OverrideOnly" then found = true end end
            assert(found, "override-only tokens remain inspectable")
            new()
            theme:select("test-custom")
            remove()
            assert(theme:get().preference == "auto" and theme:get().active ~= "test-custom")
            assert(mare.ui.icon("file") ~= "F", "glyph replacement withdrawn with theme")
            assert(not pcall(function() theme:register({name="bad-syntax", palette={}, syntax="invalid xml"}) end))
            for _, d in ipairs(theme:list().data) do assert(d.name ~= "bad-syntax", "invalid theme was never published") end
        "##).await;
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn bundled_codex_themes_aliases_and_owned_icon_withdrawal() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, r##"
            local theme = C.cordis:get("theme")
            local defaults = require("theme-default.defaults")
            local listed = {}
            for _, d in ipairs(theme:list().data) do
                listed[d.name] = d
            end
            for _, name in ipairs({
                "ansi",
                "base16",
                "base16-256",
                "base16-eighties-dark",
                "base16-mocha-dark",
                "base16-ocean-dark",
                "base16-ocean-light",
                "inspired-github",
            }) do
                assert(listed[name] and listed[name].origin == "builtin", "missing builtin " .. name)
                local snap = theme:select(name)
                assert(snap.active == name and type(snap.syntax) == "string")
                mare.ui.syntax_theme(snap.syntax, true)
            end
            assert(#listed["ansi"].variants == 2 and #listed["base16"].variants == 2 and #listed["base16-256"].variants == 2)

            local rm_half = theme:register({ name = "one-halfdark", palette = { accent = "#112233" } })
            local aliased = theme:select("one-half-dark")
            assert(aliased.active == "one-halfdark" and aliased.preference == "one-half-dark")
            theme:preview("one-half-dark")
            rm_half()
            assert(theme:get().preference == "auto" and theme:get().preview == nil)

            assert(not pcall(function()
                theme:register({
                    name = "bad-variant-base",
                    variants = {
                        dark = { base = "light", palette = {} },
                        light = { base = "light", palette = {} },
                    },
                })
            end))

            local rm_icons = theme:register({
                name = "icon-owner-a",
                palette = {},
                icons = { file = "A_FILE", theme_only = "A_ONLY" },
            })
            theme:select("icon-owner-a")
            assert(mare.ui.icon("file") == "A_FILE" and mare.ui.icon("theme_only") == "A_ONLY")
            for _, icons in pairs(mare.ui.icons) do
                icons.file = "USR_FILE"
                icons.user_custom = "USR_CUSTOM"
            end
            theme:select("dark")
            assert(mare.ui.icon("file") == "USR_FILE", "user-modified icon must survive theme withdrawal")
            assert(mare.ui.icon("theme_only") == "", "unmodified theme-added icon must be withdrawn")
            for _, icons in pairs(mare.ui.icons) do
                assert(icons.theme_only == nil, "theme-added icon key must be withdrawn")
                assert(icons.user_custom == "USR_CUSTOM", "user-added icon must survive apply")
            end
            rm_icons()

            local sem = defaults.syntax(defaults.colors.dark)
            mare.ui.syntax_theme(sem, true)
            for _, scope in ipairs({
                "keyword.operator",
                "variable.parameter",
                "entity.name.type",
                "markup.heading",
                "markup.bold",
                "diff.inserted",
                "diff.deleted",
            }) do
                assert(sem:find(scope, 1, true), "missing semantic scope: " .. scope)
            end
        "##).await;
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn codex_model_themes_accents_terminal_default_diff_and_custom_override() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        exec(&h, r##"
            local theme = C.cordis:get("theme")
            local listed = {}
            for _, d in ipairs(theme:list().data) do
                listed[d.name] = d
            end
            local expected_accents = {
                ada = "#5fafff",
                babbage = "#008700",
                curie = "#d7af00",
                cushman = "#0087ff",
                dali = "#d7af00",
                davinci = "#d75f00",
            }
            for name, accent in pairs(expected_accents) do
                assert(listed[name] and listed[name].origin == "builtin", "missing model theme " .. name)
                local snap = theme:select(name)
                assert(snap.active == name and type(snap.syntax) == "string")
                assert(snap.styles.MareAccent and snap.styles.MareAccent.fg == accent, name .. " accent mismatch")
                mare.ui.syntax_theme(snap.syntax, true)
                local diff_rendered = mare.ui.render_text(
                    ui.Diff({ path = "src/a.rs", old = "let a = 1;\n", new = "let a = 2;\n" }),
                    60
                )
                assert(diff_rendered:find("let a = 2", 1, true), "diff failed to render for " .. name)
            end

            -- Verify custom .tmTheme precedence in config/themes/ over bundled model theme.
            local dir = mare.paths.config .. "/themes"
            mare.fs.mkdir(dir)
            mare.fs.write(dir .. "/ada.tmTheme", table.concat({
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<plist version=\"1.0\"><dict><key>settings</key><array>",
                "<dict><key>settings</key><dict><key>foreground</key><string>#112233</string></dict></dict>",
                "<dict><key>scope</key><string>keyword</string><key>settings</key><dict><key>foreground</key><string>#445566</string></dict></dict>",
                "</array></dict></plist>",
            }, "\n"))
        "##).await;
        h.shutdown().await;

        let h2 = env.host().user_plugins().no_session().build().await;
        expose(&h2);
        exec(&h2, r##"
            local theme = C.cordis:get("theme")
            local after = {}
            for _, d in ipairs(theme:list().data) do
                after[d.name] = d
            end
            assert(after.ada and after.ada.origin == "user", "custom ada.tmTheme must take precedence")
            local snap = theme:select("ada")
            assert(snap.syntax:find("#445566", 1, true), "custom ada.tmTheme syntax should be active")
        "##).await;
        h2.shutdown().await;
    }).await
}

/// A full reload reads the theme files again: a user theme whose file was deleted is no longer
/// listed and stops being the active choice, even though theme-default itself (definition and
/// config unchanged) keeps running on the plugin tree (flip e2e-themes regression).
#[tokio::test]
async fn a_reload_drops_a_deleted_theme_file_and_its_choice() {
    local(async {
        let env = TestEnv::new().await;
        let themes = env.home.path().join("config/themes");
        std::fs::create_dir_all(&themes).unwrap();
        let file = themes.join("temporary-user.lua");
        std::fs::write(&file, r##"return { styles = { TemporaryToken = { fg = "#abcdef" } } }"##).unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let listed = r#"local names = {}
            for _, d in ipairs(C.cordis:get("theme"):list().data) do names[d.name] = true end
            return names["temporary-user"] == true"#;
        assert!(eval::<bool>(&h, listed).await, "the user theme is listed before the reload");
        exec(&h, r#"C.cordis:get("theme"):select("temporary-user")"#).await;
        std::fs::remove_file(&file).unwrap();
        h.kernel.reload(None).await.unwrap();
        assert!(!eval::<bool>(&h, listed).await, "the deleted theme is still listed after the reload");
        let after: (String, bool) = eval(
            &h,
            r#"local t = C.cordis:get("theme"):get()
            return t.active, t.styles.TemporaryToken ~= nil"#,
        )
        .await;
        assert!(after.0 != "temporary-user" && !after.1, "the deleted theme is still applied: {after:?}");
        h.shutdown().await;
    })
    .await;
}

/// theme hierarchy & validation: `defaults < theme palette < cfg.colors < plugin layers < user
/// theme.colors / theme.styles.<name>.<field>` field-by-field merge, diagnostics for unknown
/// names/fields/colors and `Mare*` namespace protection, live redraw + `terminal.ambiwidth`.
#[tokio::test]
async fn theme_merge_order_diagnostics_namespace_and_live_ambiwidth() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r##"
            mare.setup {
                model = "faux/echo",
                flavor = "merge-test",
                terminal = { ambiwidth = "single" },
                theme = {
                    MareToolName = { fg = "#7aa2f7" },
                    styles = { MareDim = { fg = "#5c6370", italic = true } },
                    colors = { dark = { accent = "#ff5500" } },
                    MareUnknownCore = { fg = "#123456" },
                    MareToolOk = { fg = "not-a-color", nope_field = true },
                },
                plugins = {
                    ["theme-default"] = {
                        colors = { dark = { green = "#00cc66" } },
                    },
                },
            }
            "##,
        )
        .unwrap();
        let themes_dir = env.config_dir().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(
            themes_dir.join("merge-test.lua"),
            r##"return {
                base = "dark",
                palette = { accent = "#112233", green = "#223344" },
            }"##,
        )
        .unwrap();

        let h = env.host().user_config(true).user_plugins().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r##"
            local st = mare.ui.style("MareToolName")
            assert(st.fg == "#7aa2f7" and st.bold == true, "user theme field override must preserve bold=true: " .. mare.json.encode(st))
            local dim = mare.ui.style("MareDim")
            assert(dim.fg == "#5c6370" and dim.italic == true, "user theme.styles merge failed")
            -- cfg.colors overrides theme palette, and theme.colors overrides cfg.colors
            assert(mare.ui.style("MareAccent").fg == "#ff5500", "theme.colors must beat theme palette")
            assert(mare.ui.style("MareToolOk").fg == "#00cc66", "cfg.colors must beat theme palette and ignore invalid color")
            assert(mare.ui.style("MareUnknownCore").fg == nil, "Mare* namespace must be protected from unknown names")

            -- Plugin custom style name (non-Mare) succeeds via theme override.
            local th = C.cordis:get("theme")
            local rem = th:register_override("p-custom", { CustomPluginBadge = { fg = "#abcdef", bold = true } })
            assert(mare.ui.style("CustomPluginBadge").fg == "#abcdef")
            rem()

            -- terminal.ambiwidth switches width calculation live.
            assert(mare.text.width("•") == 1, "default ambiwidth should be single")
            assert(mare.ui.render_text(mare.ui.Line("••x"), 4) == "••x", "single-width renders on 1 line at width 4")
            mare.o.terminal.ambiwidth = "double"
            mare.task.sleep(20)
            assert(mare.text.width("•") == 2, "live terminal.ambiwidth=double should take effect")
            assert(mare.ui.render_text(mare.ui.Line("••x"), 4) ~= "••x", "double-width bullets + x must exceed width 4")
            mare.o.terminal.ambiwidth = "single"
            mare.task.sleep(20)
            assert(mare.text.width("•") == 1, "live terminal.ambiwidth=single should restore width")

            -- Imperative theme.register_override / theme.register must reject reserved unknown Mare* and bad colors strictly.
            local ok_mare = pcall(function() th:register_override("bad-mare", { MareNopeStrict = { fg = "#123456" } }) end)
            assert(not ok_mare, "th:register_override must error on unknown Mare* style")
            local ok_col = pcall(function() th:register_override("bad-col", { CustomBadgeStrict = { fg = "not-a-color" } }) end)
            assert(not ok_col, "th:register_override must error on invalid color")

            -- Hook mare.log to capture theme diagnostic warnings on runtime theme update.
            local orig_log = mare.log
            local logs = {}
            mare.log = function(lvl, msg)
                logs[#logs + 1] = tostring(lvl) .. ":" .. tostring(msg)
                return orig_log(lvl, msg)
            end
            mare.o.theme = {
                MareUnknownRuntime = { fg = "#123456" },
                MareToolOk = { fg = "bad-color", extra_nope = true },
            }
            mare.task.sleep(20)
            mare.log = orig_log
            local joined = table.concat(logs, "\n")
            assert(joined:find("MareUnknownRuntime", 1, true), "missing reserved Mare* diagnostic: " .. joined)
            assert(joined:find("extra_nope", 1, true), "missing invalid field diagnostic: " .. joined)
            assert(joined:find("bad%-color"), "missing invalid color diagnostic: " .. joined)
            "##,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn markdown_fenced_code_renders_nerd_font_icons_and_ascii_fallback() {
    local(async {
        let env = TestEnv::new().await;
        let md = r#"ui.Markdown("```rust\nfn main() {}\n```")"#;
        let nerd = render(&env, 60, r#"ui.set_theme_data({ icons = "nerd" })"#, md).await;
        let ascii = render(&env, 60, r#"ui.set_theme_data({ icons = "ascii" })"#, md).await;
        assert!(nerd.contains("\u{e7a8} rust"), "expected Nerd Font Rust icon in header: {nerd:?}");
        assert!(ascii.contains(" rust") && ascii.is_ascii(), "expected clean ASCII fallback: {ascii:?}");
        for line in nerd.lines() {
            assert!(mare_shared::style::str_width(line) <= 60, "nerd line exceeded 60 cells: {line:?}");
        }
    })
    .await
}

#[tokio::test]
async fn bundled_themes_are_parsed_lazily_on_first_use() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        // Default startup ("dark") must not load any bundled theme module.
        for module in [
            "theme-default.bundled_ansi",
            "theme-default.bundled_base16_256",
            "theme-default.bundled_base16_eighties",
            "theme-default.bundled_base16_mocha",
            "theme-default.bundled_base16_ocean_dark",
            "theme-default.bundled_base16_ocean_light",
            "theme-default.bundled_inspired_github_a",
            "theme-default.bundled_inspired_github_b",
            "theme-default.bundled_codex_models",
        ] {
            assert!(
                !h.kernel.is_module_loaded("theme-default", module),
                "{module} must not load during default startup"
            );
        }
        // Listing themes must expose all bundled metadata without parsing any bundled module.
        exec(
            &h,
            r#"
            local theme = C.cordis:get("theme")
            local before_list = {}
            local listed = {}
            for _, d in ipairs(theme:list().data) do
                listed[d.name] = d
                if d.origin == "builtin" then
                    before_list[#before_list + 1] = {
                        name = d.name,
                        label = d.label,
                        variants = table.concat(d.variants, ","),
                    }
                end
            end
            assert(#before_list == 16, "expected 2 core + 14 bundled builtin themes, got " .. #before_list)
            assert(listed["base16-eighties-dark"] and listed["base16-eighties-dark"].origin == "builtin")
            assert(listed["ansi"] and #listed["ansi"].variants == 2)
            assert(listed["inspired-github"] and listed["inspired-github"].variants[1] == "light")
            assert(listed["ada"] and listed["ada"].label == "Ada")
            mare.g.theme_before_list = before_list
            "#,
        )
        .await;
        assert!(
            !h.kernel.is_module_loaded("theme-default", "theme-default.bundled_base16_eighties"),
            "theme:list() must not load bundled theme modules"
        );
        // Selecting one bundled theme loads only its module and leaves the rest untouched.
        exec(
            &h,
            r#"
            local theme = C.cordis:get("theme")
            local snap = theme:select("base16-eighties-dark")
            assert(snap.active == "base16-eighties-dark" and type(snap.syntax) == "string")
            mare.ui.syntax_theme(snap.syntax, true)
            "#,
        )
        .await;
        assert!(
            h.kernel.is_module_loaded("theme-default", "theme-default.bundled_base16_eighties"),
            "selecting base16-eighties-dark must load its module"
        );
        assert!(
            !h.kernel.is_module_loaded("theme-default", "theme-default.bundled_ansi"),
            "selecting one bundled theme must not load unrelated bundled modules"
        );
        assert!(
            !h.kernel.is_module_loaded("theme-default", "theme-default.bundled_codex_models"),
            "selecting one bundled theme must not load unrelated codex model themes"
        );
        // Materializing every bundled theme via inspect() must leave name/label/variants identical
        // to the lazy manifest reported by list() before loading.
        exec(
            &h,
            r#"
            local theme = C.cordis:get("theme")
            theme:inspect()
            local after_list = {}
            for _, d in ipairs(theme:list().data) do
                if d.origin == "builtin" then
                    after_list[#after_list + 1] = {
                        name = d.name,
                        label = d.label,
                        variants = table.concat(d.variants, ","),
                    }
                end
            end
            local before_list = mare.g.theme_before_list
            assert(#after_list == #before_list, "builtin theme count drifted after inspect()")
            for i = 1, #before_list do
                local b, a = before_list[i], after_list[i]
                assert(
                    b.name == a.name and b.label == a.label and b.variants == a.variants,
                    ("bundled manifest drift for %s: before=(%s, %s) after=(%s, %s)"):format(
                        b.name, b.label, b.variants, a.label, a.variants
                    )
                )
            end
            "#,
        )
        .await;
        for module in [
            "theme-default.bundled_ansi",
            "theme-default.bundled_base16_256",
            "theme-default.bundled_base16_eighties",
            "theme-default.bundled_base16_mocha",
            "theme-default.bundled_base16_ocean_dark",
            "theme-default.bundled_base16_ocean_light",
            "theme-default.bundled_inspired_github_a",
            "theme-default.bundled_inspired_github_b",
            "theme-default.bundled_codex_models",
        ] {
            assert!(
                h.kernel.is_module_loaded("theme-default", module),
                "{module} should be materialized after theme:inspect()"
            );
        }
        h.shutdown().await;
    })
    .await
}
