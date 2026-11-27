use super::*;
use crate::snapshot::describe;

pub(crate) const ELEMENTS: &str = "\
# Heading one

Paragraph with **bold**, *italic*, *斜体中文*, ~~struck~~ and `inline code`.

## Heading two

### Heading three

#### Heading four

- level one
  - level two
    - level three
- loose? no

1. first
2. second
3. third
4. fourth
5. fifth
6. sixth
7. seventh
8. eighth
9. ninth
10. tenth

- [x] done
- [ ] todo

> a quote
> > nested

> [!NOTE]
> note text

> [!TIP]
> tip text

> [!IMPORTANT]
> important text

> [!WARNING]
> warning text

> [!CAUTION]
> caution text

| Name | Count | Note |
|:-----|------:|:----:|
| a    | 1     | ok   |
| bb   | 22    | fine |

---

A [link](https://example.com), a [same](https://same.test) link written as <https://same.test>, and a [file](src/main.rs).

![remote](https://example.com/x.png)

Footnote[^1] here. <span>inline html</span><!-- hidden -->

[^1]: the footnote.

```rust
fn main() {
    println!(\"hi\");
}
```
";

/// The 薄暮 palette mapped onto the md.* styles, as theme-default does.
fn flavor(dark: bool) -> Theme {
    let hex = |s: &str| Color::parse(s).unwrap();
    let p = |d: &str, l: &str| hex(if dark { d } else { l });
    let (accent, accent2, text, muted, dim, border, code_bg) = (
        p("#a0a4ff", "#5558d6"),
        p("#ffa38f", "#d8604c"),
        p("#cdd6f4", "#4c4f69"),
        p("#b2b4d4", "#585b6f"),
        p("#8f91b1", "#656879"),
        p("#707199", "#82839c"),
        p("#181825", "#e5e7eb"),
    );
    let (green, yellow, red, blue) =
        (p("#9cd8b0", "#2f8a5c"), p("#f0d38f", "#a0780f"), p("#f57fa0", "#c8406a"), p("#85c1f5", "#2675c4"));
    let mut t = Theme::default();
    let fg = |c| Style::new().fg(c);
    for (k, st) in [
        ("md.h1", fg(accent).bold()),
        ("md.h2", fg(accent).bold()),
        ("md.h3", fg(text).bold()),
        ("md.h4", fg(muted).bold()),
        ("md.code", fg(accent2).bg(code_bg)),
        ("md.code_block", Style::new().bg(code_bg)),
        ("md.code_edge", fg(code_bg)),
        ("md.code_lang", fg(dim)),
        ("md.bullet", fg(accent)),
        ("md.number", fg(accent)),
        ("md.task_done", fg(green)),
        ("md.task_todo", fg(dim)),
        ("md.quote", fg(muted)),
        ("md.quote_bar", fg(border)),
        ("md.alert_note", fg(blue)),
        ("md.alert_tip", fg(green)),
        ("md.alert_important", fg(accent)),
        ("md.alert_warning", fg(yellow)),
        ("md.alert_caution", fg(red)),
        ("md.table_rule", fg(border)),
        ("md.rule", fg(border)),
        ("md.link", fg(blue).underline()),
        ("md.link_url", fg(dim)),
        ("md.path", fg(accent2).bg(code_bg)),
        ("md.footnote", fg(dim)),
        ("md.html", fg(dim)),
        ("md.strike", fg(dim)),
        ("md.image", fg(dim)),
    ] {
        t.set_style(k, st);
    }
    t
}

#[test]
fn elements_dark_and_light() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (name, dark) in [("elements_dark", true), ("elements_light", false)] {
        insta::assert_snapshot!(name, describe(&render_markdown(ELEMENTS, 60, &flavor(dark))));
    }
}

#[test]
fn links_with_osc8_show_only_their_text() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut t = flavor(true);
    t.md.osc8 = true;
    let p: Vec<String> = render_markdown("A [link](https://example.com) and [file](src/main.rs).", 60, &t)
        .iter()
        .map(Line::plain)
        .collect();
    assert_eq!(p, vec!["A link and file."]);
}

#[test]
fn ambiwidth_double_keeps_columns_aligned() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ambiwidth_double(true);
    let lines = render_markdown("- one\n  - two\n- [x] done\n\n> [!WARNING]\n> wide icons\n", 40, &flavor(true));
    set_ambiwidth_double(false);
    insta::assert_snapshot!(describe(&lines));
}

#[test]
fn math_shows_its_source_in_code_style() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = flavor(true);
    let src = "Euler $e^{i\\pi}+1=0$ and $5 or $6.\n\n$$\\sum_i x_i$$\n\n```math\na^2+b^2\n```\n";
    let lines = render_markdown(src, 60, &t);
    let p: Vec<String> = lines.iter().map(Line::plain).collect();
    assert!(p[0].contains(" e^{i\\pi}+1=0 ") && p[0].contains("$5 or $6"), "{p:?}");
    // the inline formula carries the code style
    let code_bg = t.style("md.code").bg;
    assert!(code_bg.is_some());
    assert!(lines[0].spans.iter().any(|s| s.text.contains("e^{i") && s.style.bg == code_bg), "{p:?}");
    // display math and the math fence are code cards (edge, source row, edge)
    assert!(p.iter().any(|l| l.contains("\\sum_i x_i")) && p.iter().any(|l| l.contains("a^2+b^2")), "{p:?}");
    assert_eq!(lines.iter().filter(|l| l.plain().starts_with('▄')).count(), 2, "{p:?}");
}

#[test]
fn ascii_icons_switch_the_symbols() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut t = flavor(true);
    t.icons = "ascii".into();
    let doc = "- one\n  - two\n- [x] done\n- [ ] todo\n\n> quoted\n\n> [!WARNING]\n> careful\n\n---\n\n| a | b |\n|--|--|\n| 1 | 2 |\n";
    let all = render_markdown(doc, 40, &t).iter().map(Line::plain).collect::<Vec<_>>().join("\n");
    assert!(all.is_ascii(), "{all}");
    assert!(
        all.contains("* one") && all.contains("- two") && all.contains("[x] done") && all.contains("| quoted"),
        "{all}"
    );
    // a user glyph override still wins
    t.md.glyphs.insert("bullet1".into(), "+".into());
    assert!(render_markdown("- one\n", 40, &t)[0].plain().starts_with("+ one"));
}

#[test]
fn narrow_tables_degrade_to_key_value_then_source() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = flavor(true);
    let table = "| Name | Description | Value |\n|--|--|--|\n| alpha | the first entry of many | 1 |\n";
    insta::assert_snapshot!("table_kv", describe(&render_markdown(table, 24, &t)));
    insta::assert_snapshot!("table_raw", describe(&render_markdown(table, 12, &t)));
}
