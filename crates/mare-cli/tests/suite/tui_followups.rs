//! Terminal capability and working-indicator regressions.

use crate::common::*;
use serde_json::json;

async fn terminal_detail(h: &mare_host::Host) -> String {
    eval::<String>(
        h,
        r#"for _, c in ipairs(require("commands.doctor")(C.cordis)) do
      if c.name == "terminal" then return c.detail end
    end
    error("missing terminal check")"#,
    )
    .await
}

#[tokio::test]
async fn doctor_reports_the_effective_color_depth() {
    local(async {
        let mut env = TestEnv::new().await;
        env.color_terminal();
        env.set_var("NO_COLOR", "1");
        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        exec(&h, r#"mare.ui.connected = function() return true end
            mare.ui.overlay = function() end
            TEST_TRUECOLOR = false
            mare.ui.caps = function() return {term = "test", term_program = "", multiplexer = "", truecolor = TEST_TRUECOLOR, images = "none"} end"#).await;
        assert_eq!(eval::<String>(&h, "return mare.ui.color_depth()").await, "none");
        let none = terminal_detail(&h).await;
        assert!(none.contains("no color") && !none.contains("none color"), "{none}");
        exec(&h, "TEST_TRUECOLOR = true").await;
        let none_with_capability = terminal_detail(&h).await;
        assert!(none_with_capability.contains("no color"), "NO_COLOR overrides terminal capability: {none_with_capability}");

        env.set_var("NO_COLOR", "");
        env.set_var("MARE_COLOR", "truecolor");
        let truecolor = terminal_detail(&h).await;
        assert!(truecolor.contains("true color"), "{truecolor}");
        env.set_var("MARE_COLOR", "256");
        let color256 = terminal_detail(&h).await;
        assert!(color256.contains("256 color") && !color256.contains("true color"), "{color256}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn working_glyphs_use_ascii_for_all_builtin_looks() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).config(json!({"icons": "ascii"})).no_session().build().await;
        let ok: bool = eval(
            &h,
            r#"local ui = mare.ui
            assert(ui.icon_set() == "ascii")
            local draw, data = require("ui.working_draw"), require("ui.working_looks")
            local function check(v, where)
              if type(v) == "string" then
                for _, cp in utf8.codes(v) do assert(cp < 128, where .. ": " .. v) end
              elseif type(v) == "table" then
                for _, value in pairs(v) do check(value, where) end
              end
            end
            for _, glyph in ipairs({"braille", "ember", "moon", "phase", "wave", "sail", "bullet", "pulse"}) do
              for _, color in ipairs({"truecolor", "256", "none"}) do
                for _, still in ipairs({false, true}) do
                  local node = draw.glyph({glyph = glyph}, data, {color = color, still = still})
                  assert(node ~= nil, "missing " .. glyph)
                  check(node, glyph .. "/" .. color .. "/" .. tostring(still))
                end
              end
            end
            return true"#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}
