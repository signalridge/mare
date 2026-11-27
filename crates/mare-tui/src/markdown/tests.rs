use super::*;

fn plain(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|l| l.plain()).collect()
}

const DOC: &str = "# Title\n\nSome **bold** and *italic* text with `code` and a [link](https://x.y).\n\n- one\n- two\n  - nested 中文 item that is long enough to wrap around the width\n  - [x] done\n  - [ ] todo\n- three\n\n1. first\n2. second\n\n> quoted text that is fairly long and should wrap nicely in the gutter\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n| 名前 | Value | Note |\n|:--|--:|:-:|\n| 中文 | 12 | ok |\n| b | 3456 | a much longer note cell |\n\n---\n\nEnd ~~struck~~.\n";

#[test]
fn widths_bounded() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    // everything but code lines (never wrapped) stays within the width
    let prose = DOC.replace("```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n", "");
    for w in [8, 20, 33, 40, 80] {
        for l in render_markdown(&prose, w, &t) {
            assert!(l.width() <= w, "width {w}: {:?} ({})", l.plain(), l.width());
        }
    }
}

#[test]
fn structure() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let p = plain(&render_markdown(DOC, 80, &t));
    assert_eq!(p[0], "Title");
    assert!(p.iter().any(|l| l.starts_with("• one")));
    assert!(p.iter().any(|l| l.starts_with("  ◦ nested")));
    assert!(p.iter().any(|l| l.starts_with("  [✓] done")));
    assert!(p.iter().any(|l| l.starts_with("  [ ] todo")));
    assert!(p.iter().any(|l| l.starts_with("2. second")));
    assert!(p.iter().any(|l| l.starts_with("▎ quoted")));
    assert!(p.iter().any(|l| l.starts_with(" fn main() {") && l.ends_with("rust ")));
    assert!(p.iter().any(|l| l.contains("link (https://x.y)")));
    // tight list: no blank between items
    let i = p.iter().position(|l| l.starts_with("• one")).unwrap();
    assert!(p[i + 1].starts_with("• two"));
}

#[test]
fn nested_hanging_indent() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let p = plain(&render_markdown("- outer item\n  - inner item with quite a few words to wrap\n", 20, &t));
    let i = p.iter().position(|l| l.starts_with("  ◦ inner")).unwrap();
    assert!(p[i + 1].starts_with("    "), "{p:?}");
}

#[test]
fn cjk_table() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let p = render_markdown("| 名前 | v |\n|--|--|\n| 中文中文 | 1 |\n", 40, &t);
    let rows: Vec<String> = plain(&p);
    // columns aligned: the second column starts at the same cell on every content row
    let col = |s: &str| str_width(s.trim_end().rsplit_once("  ").map(|(a, _)| a).unwrap_or(""));
    assert_eq!(col(&rows[0]), col(&rows[2]));
    assert!(rows[1].starts_with('━'));
    // narrow width: table shrinks and wraps
    for l in render_markdown("| aaaaaaaaaa | bbbbbbbbbbbb |\n|--|--|\n| 中文中文中文 | x |\n", 12, &t) {
        assert!(l.width() <= 12);
    }
}

#[test]
fn unclosed_fence_is_code() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let p = plain(&render_markdown("intro\n\n```python\ndef f():\n\n    return 1\n", 40, &t));
    // still streaming: the card's label says so
    assert!(p.iter().any(|l| l.starts_with(" def f():") && l.ends_with("python ⋯ ")), "{p:?}");
    assert!(p.iter().any(|l| l == "     return 1 "));
    assert_eq!(stable_split("intro\n\n```python\ndef f():\n\n    return 1\n"), "intro\n\n".len());
}

#[test]
fn streaming_render_from_is_the_tail_of_render() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let mut sm = StreamingMarkdown::new();
    let mut reference = StreamingMarkdown::new();
    for (i, ch) in DOC.char_indices() {
        let mut b = [0u8; 4];
        sm.push(ch.encode_utf8(&mut b));
        reference.push(ch.encode_utf8(&mut b));
        if i % 7 != 0 {
            continue;
        }
        let full = plain(&reference.render(40, &t));
        assert_eq!(full, plain(&render_markdown_stream_ref(&DOC[..i + ch.len_utf8()], 40, &t)));
        for skip in [0, 1, 3, full.len().saturating_sub(1), full.len(), full.len() + 2] {
            let want: Vec<String> = full.iter().skip(skip).cloned().collect();
            assert_eq!(plain(&sm.render_from(40, &t, skip)), want, "at {i}, skip {skip}");
        }
    }
}

/// What a from-scratch streaming render shows for `src` (no incremental state).
fn render_markdown_stream_ref(src: &str, w: usize, t: &Theme) -> Vec<Line> {
    let mut sm = StreamingMarkdown::new();
    sm.push(src);
    sm.render(w, t)
}

#[test]
fn streaming_matches_full_and_prefix_stable() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let mut s = StreamingMarkdown::new();
    let mut last_stable = 0;
    let mut prev_split_lines: Vec<String> = vec![];
    for (i, ch) in DOC.char_indices() {
        s.push(&DOC[i..i + ch.len_utf8()]);
        let r = plain(&s.render(40, &t));
        let split = stable_split(s.text());
        assert!(split >= last_stable);
        if split > 0 {
            let pre = plain(&render_markdown(&DOC[..split], 40, &t));
            assert_eq!(&r[..pre.len()], &pre[..]);
            // previously stable lines never change
            assert_eq!(&r[..prev_split_lines.len()], &prev_split_lines[..]);
            prev_split_lines = pre;
        }
        last_stable = split;
    }
    assert_eq!(plain(&s.render(40, &t)), plain(&render_markdown(DOC, 40, &t)));
}

#[test]
fn footnotes_are_collected_at_the_end_while_streaming_too() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let doc = "Claim[^1] and more[^n].\n\n[^1]: first note.\n\nMiddle paragraph.\n\n[^n]: second note.\n\n```\nraw [^1] stays\n```\n\nLast words.\n";
    let full = plain(&render_markdown(doc, 40, &t));
    assert_eq!(full[0], "Claim[1] and more[n].");
    let tail: Vec<&str> = full.iter().rev().take(3).rev().map(String::as_str).collect();
    assert_eq!(tail, ["[1] first note.", "", "[n] second note."], "{full:#?}");
    assert!(full.iter().any(|l| l.contains("raw [^1] stays")), "code is left alone: {full:#?}");
    let last_words = full.iter().position(|l| l == "Last words.").unwrap();
    assert!(last_words < full.len() - 3);
    // the reference is dim like a resolved one even when its block is rendered on its own
    let alone = render_markdown("Claim[^1] here.\n", 40, &t);
    assert_eq!(alone[0].plain(), "Claim[1] here.");
    assert!(alone[0].spans.iter().any(|s| s.text == "[1]" && s.style == t.style("md.footnote")));
    // streaming: the notes stay below the growing tail and the result matches the full render
    let mut s = StreamingMarkdown::new();
    for (i, ch) in doc.char_indices() {
        s.push(&doc[i..i + ch.len_utf8()]);
        let r = plain(&s.render(40, &t));
        if r.iter().any(|l| l.starts_with("[n] ")) {
            assert!(r.last().unwrap().starts_with("[n] "), "{r:#?}");
        }
        assert_eq!(s.rows_for(s.stable_src(), 40, &t), s.stable_lines());
    }
    assert_eq!(plain(&s.render(40, &t)), full);
}

#[test]
fn unchanged_stream_frame_cache_tracks_content_width_and_theme() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut theme = Theme::default();
    let mut sm = StreamingMarkdown::new();
    sm.set_text("A paragraph.\n\n```rust\nlet one = 1;\n");
    let check = |sm: &mut StreamingMarkdown, width, theme: &Theme| {
        let mut fresh = StreamingMarkdown::new();
        fresh.set_text(sm.text());
        let expected = fresh.render(width, theme);
        assert_eq!(sm.render(width, theme), expected);
        assert_eq!(sm.render(width, theme), expected);
        for skip in [0, 1, expected.len() / 2, expected.len(), expected.len() + 1] {
            assert_eq!(sm.render_from(width, theme, skip), expected.iter().skip(skip).cloned().collect::<Vec<_>>());
        }
    };
    check(&mut sm, 40, &theme);
    sm.set_text("A paragraph.\n\n```rust\nlet two = 2;\n"); // same byte length
    check(&mut sm, 40, &theme);
    sm.push("let more = 3;\n");
    check(&mut sm, 24, &theme);
    theme.md.code_line_numbers = true; // direct caller change, no global generation bump
    check(&mut sm, 24, &theme);
    mare_shared::style::theme_changed();
    check(&mut sm, 24, &theme);
    sm.drop_cache();
    check(&mut sm, 24, &theme);
}

#[test]
fn incremental_code_highlighting_matches_fresh_streams() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut theme = Theme::default();
    theme.md.code_line_numbers = true;
    for doc in [
        "intro\n\n```rust\n/* multi\nline */\nlet s = \"héllo\\nworld\";\n\tlet x = 1;\n```\n",
        "> ```python\n> def greet():\n>     return 'hi'\n> ```\n",
        "```diff\n-old\n+new\n```\n",
        "```unknown-language-xyz\nhello\r\nworld\r\n```\n",
    ] {
        let mut streamed = StreamingMarkdown::new();
        for (i, ch) in doc.char_indices() {
            streamed.push(&doc[i..i + ch.len_utf8()]);
            let mut fresh = StreamingMarkdown::new();
            fresh.set_text(streamed.text());
            for width in [23, 70] {
                assert_eq!(streamed.render(width, &theme), fresh.render(width, &theme), "at {i} in {doc:?}");
            }
        }
        streamed.set_text("```rust\nlet x = 1;\nlet z = 3;");
        let _ = streamed.render(40, &theme);
        streamed.set_text("```rust\nlet y = 2;\nlet z = 3;");
        let mut fresh = StreamingMarkdown::new();
        fresh.set_text(streamed.text());
        assert_eq!(streamed.render(40, &theme), fresh.render(40, &theme), "same-length replacement");
    }
}

#[test]
fn incremental_code_styles_match_the_full_renderer_at_every_character() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut dark = Theme::default();
    dark.md.code_line_numbers = true;
    let mut light = dark.clone();
    light.set_style("md.code_block", Style::new().bg(Color::Rgb(240, 240, 240)));
    let doc = "```rust\n/* multi\nline */\nlet s = \"héllo\\nworld\";\r\n\tlet x = 1;\n```\n";
    for theme in [&dark, &light] {
        let mut sm = StreamingMarkdown::new();
        for (i, ch) in doc.char_indices() {
            let text = &doc[i..i + ch.len_utf8()];
            sm.push(text);
            assert_eq!(sm.render(40, theme), render_markdown(sm.text(), 40, theme), "at {i}");
        }
    }
}

#[test]
fn presented_fences_keep_source_and_nested_parser_frames() {
    use mare_shared::ui::Node;
    let source =
        "> intro\n>\n> ```diagram\n> x\n> ```\n\n- item\n\n  ~~~diagram\n  x\n  ~~~\n\n```rust\nfn untouched() {}\n```";
    let t = Theme::default();
    let fences = mare_shared::markdown::fences(source);
    let blocks = fences[..2].iter().map(|b| (b.start, Node::Line(Line::raw("PICTURE")))).collect();
    let lines = plain(&render_presented(source, 40, &t, &blocks, 0));
    assert_eq!(lines.iter().filter(|l| l.contains("PICTURE")).count(), 2);
    assert!(lines.iter().any(|l| l.contains("▎ PICTURE")), "{lines:?}");
    assert!(lines.iter().any(|l| l == "  PICTURE"), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("fn untouched()")));
    assert_eq!(
        plain(&render_presented(source, 40, &t, &std::collections::BTreeMap::new(), 0)),
        plain(&render_markdown(source, 40, &t))
    );
}

#[test]
fn unavailable_artifacts_keep_complete_source_instead_of_image_placeholder() {
    use mare_shared::ui::Node;
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let protocol = crate::image::protocol();
    crate::image::set_protocol(crate::image::Protocol::None);
    let source = "```diagram\nfirst\nsecond\n```";
    let blocks = [(
        0,
        Node::Lines(vec![Node::Image {
            resource: None,
            fallback: None,
            image_id: 1,
            path: None,
            bytes: Some(std::sync::Arc::from(&b"bad image"[..])),
            max_rows: 16,
            max_cols: 80,
        }]),
    )]
    .into();
    let lines = plain(&render_presented(source, 40, &Theme::default(), &blocks, 0));
    crate::image::set_protocol(protocol);
    assert!(lines.iter().any(|line| line.contains("first")));
    assert!(lines.iter().any(|line| line.contains("second")));
    assert!(!lines.iter().any(|line| line.contains("[image:")));
}

#[test]
fn open_fence_1000_line_streaming_matches_full_render_across_digits_resize_and_close() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut theme = Theme::default();
    theme.md.code_line_numbers = true;
    let mut sm = StreamingMarkdown::new();
    let intro = "Intro paragraph with footnote[^1].\n\n[^1]: note stays below.\n\n```rust\n";
    sm.push(intro);
    assert_eq!(sm.render(48, &theme), render_markdown(sm.text(), 48, &theme));
    for i in 1..=1005 {
        let line = format!("let v{i} = {i};\n");
        sm.push(&line);
        // Verify every step around 1..12, 97..103 (99->100 digits), 998..1002 (999->1000), and checkpoints.
        let check = i <= 12 || (97..=103).contains(&i) || (998..=1002).contains(&i) || i % 200 == 0;
        if check {
            let width = match i {
                100 => 32,
                500 => 64,
                _ => 48,
            };
            let full = render_markdown(sm.text(), width, &theme);
            assert_eq!(sm.render(width, &theme), full, "mismatch at line {i} width {width}");
            for skip in [0, 2, full.len() / 2, full.len().saturating_sub(2), full.len() + 1] {
                let want: Vec<Line> = full.iter().skip(skip).cloned().collect();
                assert_eq!(sm.render_from(width, &theme, skip), want, "skip {skip} at line {i}");
            }
        }
    }
    // Split closing fence across deltas, then trailing paragraph.
    sm.push("``");
    assert_eq!(sm.render(48, &theme), render_markdown(sm.text(), 48, &theme));
    sm.push("`\n\nAfter fence.\n");
    assert_eq!(sm.render(48, &theme), render_markdown(sm.text(), 48, &theme));
}

#[test]
fn consecutive_same_language_open_fences_in_one_update_do_not_reuse_stale_offsets_or_panic_on_utf8() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let theme = Theme::default();
    let mut sm = StreamingMarkdown::new();
    sm.push("```rust\nlet first_short = 1;\n");
    assert_eq!(sm.render(60, &theme), render_markdown(sm.text(), 60, &theme));
    // Close first fence and immediately open a longer second rust fence containing multibyte UTF-8 in one update.
    sm.push("let first_second_line = 2;\n```\n\n```rust\nlet emoji = \"🦀日本語\";\nlet tail = \"ééé\";\n");
    assert_eq!(sm.render(60, &theme), render_markdown(sm.text(), 60, &theme));
    // Append more UTF-8 lines where the old completed byte offset would have landed inside a codepoint.
    sm.push("let more = \"αβγ\";\n");
    assert_eq!(sm.render(60, &theme), render_markdown(sm.text(), 60, &theme));
}

#[test]
fn open_fence_info_with_escapes_or_entities_matches_full_render() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let theme = Theme::default();
    for info in ["r\\ust", "r&#117;st"] {
        let mut sm = StreamingMarkdown::new();
        sm.push(&format!("```{info}\nlet x = 1;\n"));
        assert_eq!(sm.render(48, &theme), render_markdown(sm.text(), 48, &theme), "{info} open");
        sm.push("let y = 2;\n```\n");
        assert_eq!(sm.render(48, &theme), render_markdown(sm.text(), 48, &theme), "{info} close");
    }
}

#[test]
fn fenced_code_renders_nerd_font_language_icons_and_clean_fallbacks() {
    let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = "```Rust\nfn main() {}\n```\n\n```unknownlang\nhello\n```\n";

    // Nerd Font mode: known language gets its Nerd Font icon; unknown language stays plain text.
    assert_eq!(mare_shared::style::width_method(), mare_shared::style::WidthMethod::Unicode);
    assert_eq!(mare_shared::style::str_width(" \u{e7a8} Rust "), 8);
    let mut nerd = Theme { icons: "nerd".into(), ..Theme::default() };
    let nerd_lines = render_markdown(src, 48, &nerd);
    let nerd_plain = plain(&nerd_lines);
    let rust_row = nerd_lines.iter().find(|l| l.plain().contains("fn main()")).expect("rust code line");
    assert_eq!(rust_row.width(), 48, "inline nerd label must align exactly to card width: {:?}", rust_row.plain());
    assert_eq!(
        mare_shared::style::str_width(&rust_row.plain()),
        48,
        "Unicode width of nerd label row must equal 48: {:?}",
        rust_row.plain()
    );
    assert!(rust_row.plain().ends_with("\u{e7a8} Rust "), "{:?}", rust_row.plain());
    let unknown_row = nerd_plain.iter().find(|l| l.contains("hello")).expect("unknown code line");
    assert!(unknown_row.ends_with(" unknownlang "), "unsupported language falls back to plain text: {unknown_row:?}");

    // Streaming open fence in Nerd Font mode includes both language icon and streaming glyph.
    let mut sm = StreamingMarkdown::new();
    sm.push("```python\ndef f():\n");
    let stream_lines = sm.render(48, &nerd);
    assert_eq!(stream_lines, render_markdown(sm.text(), 48, &nerd));
    assert!(
        stream_lines.iter().any(|l| l.plain().ends_with("\u{e73c} python ⋯ ") && l.width() == 48),
        "{:?}",
        plain(&stream_lines)
    );

    // ASCII and Unicode fallback modes omit Nerd Font glyphs cleanly.
    nerd.icons = "ascii".into();
    let ascii_plain = plain(&render_markdown(src, 48, &nerd));
    assert!(ascii_plain.iter().all(|l| l.is_ascii()), "ascii fallback must stay strictly ASCII: {ascii_plain:?}");
    assert!(ascii_plain.iter().any(|l| l.ends_with(" Rust ")), "{ascii_plain:?}");

    let unicode = Theme::default();
    let unicode_plain = plain(&render_markdown(src, 48, &unicode));
    assert!(unicode_plain.iter().any(|l| l.ends_with(" Rust ") && !l.contains('\u{e7a8}')), "{unicode_plain:?}");

    // Ambiwidth=double accounts for PUA display width without shifting/overflowing layout.
    nerd.icons = "nerd".into();
    set_ambiwidth_double(true);
    let wide_lines = render_markdown("```rust\nx\n```\n", 48, &nerd);
    let wide_rust = wide_lines.iter().find(|l| l.plain().contains(" x ")).unwrap();
    let wide_w = wide_rust.width();
    set_ambiwidth_double(false);
    assert_eq!(
        wide_w,
        48,
        "ambiwidth=double width accounting must match card width (got {}): {:?}",
        wide_w,
        wide_rust.plain()
    );

    // Narrow width keeps header label row bounded to available width.
    for l in render_markdown("```typescript\nconst a = 1;\n```\n", 8, &nerd) {
        if !l.plain().contains("const a") {
            assert!(l.width() <= 8, "header/edge overflowed narrow width: {:?} ({})", l.plain(), l.width());
        }
    }
}

#[test]
fn backtick_in_info_string_and_quoted_fences_follow_commonmark() {
    // CommonMark §4.5: backtick fence info string cannot contain backticks (inline code span).
    let src = "```foo`bar\n\nNext para\n";
    assert_eq!(stable_split(src), "```foo`bar\n\n".len());

    // Blockquote-quoted closed code fence must not show streaming `⋯` indicator.
    let quoted = "> ```rust\n> let x = 1;\n> ```\n";
    let t = Theme::default();
    let lines = plain(&render_markdown(quoted, 50, &t));
    assert!(!lines.iter().any(|l| l.contains("⋯")), "closed quoted fence marked open: {lines:?}");
}
