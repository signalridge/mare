use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, Modifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), Modifiers::CONTROL)
}
fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, Modifiers::ALT)
}
fn typ(e: &mut Editor, s: &str) {
    for c in s.chars() {
        e.handle_key(key(KeyCode::Char(c)));
    }
}

#[test]
fn cjk_and_emoji_cursor() {
    let mut e = Editor::new();
    typ(&mut e, "中文👍🏽x");
    assert_eq!(e.cursor(), (0, "中文👍🏽x".len()));
    e.handle_key(key(KeyCode::Left));
    e.handle_key(key(KeyCode::Left)); // skin-toned emoji is one grapheme
    assert_eq!(e.cursor(), (0, "中文".len()));
    e.handle_key(key(KeyCode::Backspace));
    assert_eq!(e.raw_text(), "中👍🏽x");
    e.handle_key(key(KeyCode::Delete));
    assert_eq!(e.raw_text(), "中x");
}

#[test]
fn word_ops() {
    let mut e = Editor::new();
    typ(&mut e, "foo bar.baz qux");
    e.handle_key(alt(KeyCode::Left));
    assert_eq!(e.cursor().1, "foo bar.baz ".len());
    e.handle_key(alt(KeyCode::Char('b')));
    assert_eq!(e.cursor().1, "foo bar.".len());
    e.handle_key(KeyEvent::new(KeyCode::Right, Modifiers::CONTROL));
    assert_eq!(e.cursor().1, "foo bar.baz".len());
    e.handle_key(ctrl('w'));
    assert_eq!(e.raw_text(), "foo bar. qux");
    e.handle_key(ctrl('e'));
    e.handle_key(ctrl('y'));
    assert_eq!(e.raw_text(), "foo bar. quxbaz");
    e.handle_key(ctrl('a'));
    e.handle_key(ctrl('k'));
    assert_eq!(e.raw_text(), "");
    e.handle_key(ctrl('z'));
    assert_eq!(e.raw_text(), "foo bar. quxbaz");
    typ(&mut e, "x");
    e.handle_key(ctrl('u'));
    assert_eq!(e.raw_text(), "foo bar. quxbaz");
}

#[test]
fn backslash_enter_and_submit() {
    let mut e = Editor::new();
    typ(&mut e, "a\\");
    assert_eq!(e.handle_key(key(KeyCode::Enter)), EditorAction::Changed);
    typ(&mut e, "b");
    assert_eq!(e.raw_text(), "a\nb");
    e.handle_key(KeyEvent::new(KeyCode::Enter, Modifiers::SHIFT));
    typ(&mut e, "c");
    assert_eq!(e.handle_key(key(KeyCode::Enter)), EditorAction::Submit("a\nb\nc".into()));
    assert!(e.is_empty());
}

#[test]
fn history_signals() {
    let mut e = Editor::new();
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::HistoryPrev);
    assert_eq!(e.handle_key(key(KeyCode::Down)), EditorAction::HistoryNext);
    // a typed (not recalled) draft: Up/Down move the cursor and never leave the draft
    e.set_text("one\ntwo");
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
    assert_eq!(e.cursor(), (0, 3));
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
    assert_eq!(e.cursor(), (0, 0), "top row: Up jumps to the start");
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None, "draft gate: no history");
    assert_eq!(e.handle_key(key(KeyCode::Down)), EditorAction::None);
    assert_eq!(e.handle_key(key(KeyCode::Down)), EditorAction::None);
    assert_eq!(e.cursor(), (1, 3), "bottom row: Down jumps to the end");
    assert_eq!(e.handle_key(key(KeyCode::Down)), EditorAction::None);
}

#[test]
fn bang_on_an_empty_input_enters_shell_mode() {
    let mut e = Editor::new();
    typ(&mut e, "!");
    assert_eq!((e.shell_level(), e.raw_text()), (0, "!".into()), "off unless enabled (dialogs)");
    let mut e = Editor::new();
    e.enable_shell_mode();
    typ(&mut e, "!");
    assert_eq!(e.shell_level(), 1);
    assert!(!e.is_empty() && e.raw_text() == "!", "the mark counts as input");
    typ(&mut e, "!ls !x");
    assert_eq!(e.shell_level(), 2, "a second `!` first: `!!`");
    assert_eq!(e.raw_text(), "!!ls !x", "later `!`s are text");
    // Backspace on the empty line leaves one level at a time; undo brings it back
    e.clear();
    typ(&mut e, "!!");
    e.handle_key(key(KeyCode::Backspace));
    assert_eq!(e.shell_level(), 1);
    e.handle_key(key(KeyCode::Backspace));
    assert!(e.is_empty());
    e.handle_key(ctrl('_'));
    assert_eq!(e.shell_level(), 1);
    // a submit is the `!cmd`; set_text / recall of one comes back in shell mode
    typ(&mut e, "echo hi");
    assert_eq!(e.handle_key(key(KeyCode::Enter)), EditorAction::Submit("!echo hi".into()));
    assert_eq!(e.shell_level(), 0);
    e.recall("!!git status", true);
    assert_eq!((e.shell_level(), e.raw_text()), (2, "!!git status".into()));
    e.clear_undoable();
    assert!(e.is_empty());
    e.handle_key(ctrl('_'));
    assert_eq!(e.text(), "!!git status");
}

#[test]
fn multiline_history_recall_gate() {
    let mut e = Editor::new();
    // Up-recall of a multi-line entry: cursor at the start, the next Up keeps browsing
    e.recall("first\nsecond\nthird", true);
    assert_eq!(e.cursor(), (0, 0));
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::HistoryPrev);
    // Down-recall: cursor at the end, Down keeps browsing
    e.recall("a\nb", false);
    assert_eq!(e.cursor(), (1, 1));
    assert_eq!(e.handle_key(key(KeyCode::Down)), EditorAction::HistoryNext);
    // moving into the middle of the entry: Up/Down move within it
    e.recall("first\nsecond\nthird", true);
    e.handle_key(key(KeyCode::Right));
    assert_eq!(e.handle_key(key(KeyCode::Down)), EditorAction::None);
    assert_eq!(e.cursor().0, 1);
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
    assert_eq!(e.cursor(), (0, 0));
    // back at the start of the untouched entry: history again
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::HistoryPrev);
    // an edited entry is a draft: no history even at the edges
    e.recall("abc", false);
    typ(&mut e, "d");
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
    assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
    assert_eq!(e.raw_text(), "abcd");
}

#[test]
fn tabs_expand_on_insert_and_set_text() {
    let mut e = Editor::new();
    e.set_text("a\tb\n\tc");
    assert_eq!(e.raw_text(), "a   b\n    c");
    e.clear();
    typ(&mut e, "ab");
    e.insert_str("\tx\n\ty");
    // the first tab counts from the cursor's column: "ab" + 2 spaces to the stop at 4
    assert_eq!(e.raw_text(), "ab  x\n    y");
    // a typed Tab key is the app's (completion), not text
    assert_eq!(e.handle_key(key(KeyCode::Tab)), EditorAction::Unhandled);
    // CJK counts two columns toward the tab stop
    e.clear();
    e.insert_str("中\tz");
    assert_eq!(e.raw_text(), "中  z");
    // rendering aligns: every rendered row keeps the box width
    let th = Theme::default();
    let r = e.render(20, &th, &EditorRenderOpts::default());
    assert!(r.lines.iter().all(|l| l.width() == 20));
    assert!(!r.lines.iter().any(|l| l.plain().contains('\t')));
}

#[test]
fn cjk_word_motion() {
    // every ideograph is its own word (Unicode word bounds), punctuation splits words
    let mut e = Editor::new();
    typ(&mut e, "你好世界 foo.bar");
    e.handle_key(alt(KeyCode::Left));
    assert_eq!(e.cursor().1, "你好世界 foo.".len());
    e.handle_key(alt(KeyCode::Left));
    assert_eq!(e.cursor().1, "你好世界 foo".len());
    e.handle_key(alt(KeyCode::Left));
    assert_eq!(e.cursor().1, "你好世界 ".len());
    e.handle_key(alt(KeyCode::Left));
    assert_eq!(e.cursor().1, "你好世".len());
    e.handle_key(alt(KeyCode::Left));
    assert_eq!(e.cursor().1, "你好".len());
    e.handle_key(KeyEvent::new(KeyCode::Right, Modifiers::CONTROL));
    assert_eq!(e.cursor().1, "你好世".len());
    e.handle_key(ctrl('w'));
    assert_eq!(e.raw_text(), "你好界 foo.bar");
    // Japanese kana/kanji and Hangul: word motion never splits a grapheme
    e.set_text("日本語テキスト 한국어");
    for _ in 0..10 {
        e.handle_key(alt(KeyCode::Left));
        let (_, c) = e.cursor();
        assert!(e.raw_text().is_char_boundary(c));
    }
    assert_eq!(e.cursor().1, 0);
}

#[test]
fn kill_ring_accumulates_and_yank_pops() {
    let mut e = Editor::new();
    typ(&mut e, "one two three");
    // consecutive backward kills join into one entry
    e.handle_key(ctrl('w'));
    e.handle_key(ctrl('w'));
    assert_eq!(e.raw_text(), "one ");
    e.handle_key(ctrl('y'));
    assert_eq!(e.raw_text(), "one two three");
    // a movement breaks accumulation: a new entry
    e.handle_key(ctrl('a'));
    e.handle_key(alt(KeyCode::Char('d')));
    assert_eq!(e.raw_text(), " two three");
    e.handle_key(ctrl('e'));
    e.handle_key(ctrl('y'));
    assert_eq!(e.raw_text(), " two threeone");
    // Alt+Y right after a yank swaps in the older entry, and cycles
    e.handle_key(alt(KeyCode::Char('y')));
    assert_eq!(e.raw_text(), " two threetwo three");
    e.handle_key(alt(KeyCode::Char('y')));
    assert_eq!(e.raw_text(), " two threeone");
    // not after a yank: Alt+Y does nothing
    e.handle_key(key(KeyCode::Left));
    assert_eq!(e.handle_key(alt(KeyCode::Char('y'))), EditorAction::None);
    // forward kills append: Ctrl+K at line end joins the next line into the same entry
    let mut e = Editor::new();
    e.set_text("ab\ncd");
    e.handle_key(key(KeyCode::Up));
    e.handle_key(ctrl('a'));
    e.handle_key(ctrl('k'));
    e.handle_key(ctrl('k'));
    e.handle_key(ctrl('k'));
    assert_eq!(e.raw_text(), "");
    e.handle_key(ctrl('y'));
    assert_eq!(e.raw_text(), "ab\ncd");
}

#[test]
fn undo_by_words() {
    let mut e = Editor::new();
    typ(&mut e, "hello big world");
    e.handle_key(ctrl('z'));
    assert_eq!(e.raw_text(), "hello big");
    e.handle_key(ctrl('z'));
    assert_eq!(e.raw_text(), "hello");
    e.handle_key(ctrl('z'));
    assert_eq!(e.raw_text(), "");
    // a movement in between starts a new unit even mid-word
    typ(&mut e, "abc");
    e.handle_key(key(KeyCode::Left));
    typ(&mut e, "X");
    e.handle_key(ctrl('z'));
    assert_eq!(e.raw_text(), "abc");
}

/// Undo snapshots share unchanged lines: alternating keys in a 5000-line buffer (a snapshot per
/// key) keep one copy of each line plus one per edit, not one buffer per snapshot.
#[test]
fn undo_snapshots_share_unchanged_lines() {
    let text = (0..5000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let mut e = Editor::new();
    e.set_text(&text);
    for _ in 0..150 {
        e.handle_key(key(KeyCode::Char('1')));
        e.handle_key(key(KeyCode::Backspace));
    }
    assert_eq!(e.undo.len(), 200, "capped");
    let all = e.undo.iter().flat_map(|(l, ..)| l.iter()).chain(e.lines.iter());
    let distinct: std::collections::HashSet<*const String> = all.map(|l| std::ptr::from_ref::<String>(l)).collect();
    assert!(distinct.len() <= 5000 + 201, "{} line allocations", distinct.len());
    while e.handle_key(ctrl('z')) == EditorAction::Changed {}
    assert_eq!(e.raw_text(), text, "undo gives back the same states");
}

#[test]
fn paste_placeholders_expand_by_position() {
    let big = |tag: &str| (0..12).map(|i| format!("{tag}{i}\n")).collect::<String>();
    let mut e = Editor::new();
    // pasted content that contains the next placeholder's text is not expanded twice
    let tricky = format!("{}[Pasted text #2 +13 lines]", big("x"));
    e.handle_paste(&tricky);
    typ(&mut e, " ");
    e.handle_paste(&big("y"));
    let raw = e.raw_text();
    assert!(raw.starts_with("[Pasted text #1 +13 lines] [Pasted text #2 +13 lines]"), "{raw}");
    assert_eq!(e.text(), format!("{} {}", tricky, big("y")));
    // a duplicated placeholder (killed and yanked twice) expands both times
    e.handle_key(ctrl('w'));
    e.handle_key(ctrl('y'));
    e.handle_key(ctrl('y'));
    assert_eq!(e.text(), format!("{} {}{}", tricky, big("y"), big("y")));
    // deleting one placeholder (atomically) leaves the others intact
    e.handle_key(key(KeyCode::Backspace));
    assert_eq!(e.text(), format!("{} {}", tricky, big("y")));
    e.handle_key(ctrl('a'));
    e.handle_key(key(KeyCode::Delete));
    assert_eq!(e.raw_text(), " [Pasted text #2 +13 lines]");
    assert_eq!(e.text(), format!(" {}", big("y")));
    // numbers never repeat, so a new paste can't collide with a surviving placeholder
    e.handle_key(ctrl('e'));
    e.handle_paste(&big("z"));
    assert!(e.raw_text().ends_with("[Pasted text #3 +13 lines]"));
    assert_eq!(e.text(), format!(" {}{}", big("y"), big("z")));
    // a placeholder broken by hand is plain text
    let mut e = Editor::new();
    e.handle_paste(&big("q"));
    let raw = e.raw_text();
    e.set_text(&raw[..raw.len() - 1]);
    assert_eq!(e.text(), raw[..raw.len() - 1]);
    // inserting a literal lookalike before a real token and deleting the real token leaves no element
    let mut e = Editor::new();
    let id = e.insert_token("[Image #1]", "[Image #1]");
    e.handle_key(ctrl('a'));
    e.insert_str("prose [Image #1] ");
    let (txt, elems) = e.text_with_elements();
    assert_eq!(txt, "prose [Image #1] [Image #1]");
    assert_eq!(elems.len(), 1);
    assert_eq!(elems[0].id, id);
    assert_eq!(elems[0].range, "prose [Image #1] ".len()..txt.len());
    e.handle_key(ctrl('e'));
    e.handle_key(key(KeyCode::Backspace));
    assert_eq!(e.raw_text(), "prose [Image #1] ");
    assert!(e.text_with_elements().1.is_empty());
}

#[test]
fn wrap_cursor_cjk() {
    let mut e = Editor::new();
    // width 14 -> content width 14 - 4 - 2 = 8 cells = 4 CJK chars per row
    typ(&mut e, "一二三四五六");
    let th = Theme::default();
    let r = e.render(14, &th, &EditorRenderOpts { max_height: 0, ..Default::default() });
    assert_eq!(r.lines.len(), 4); // top, 2 content rows, bottom
    assert_eq!(r.cursor, (2, 2 + 2 + 4));
    assert!(r.lines.iter().all(|l| l.width() == 14));
    e.handle_key(key(KeyCode::Up));
    assert_eq!(e.cursor(), (0, "一二".len()));
    // full row: cursor wraps onto a fresh row
    e.set_text("一二三四");
    let r = e.render(14, &th, &EditorRenderOpts { max_height: 0, ..Default::default() });
    assert_eq!(r.cursor, (2, 4));
}

#[test]
fn render_scroll_and_placeholder() {
    let th = Theme::default();
    let mut e = Editor::new();
    let r = e.render(20, &th, &EditorRenderOpts { placeholder: "Ask anything".into(), ..Default::default() });
    assert!(r.lines[1].plain().contains("Ask anything"));
    assert_eq!(r.cursor, (1, 4));
    e.set_text("1\n2\n3\n4\n5");
    let r = e.render(20, &th, &EditorRenderOpts { max_height: 2, ..Default::default() });
    assert_eq!(r.lines.len(), 4);
    assert!(r.lines[2].plain().contains('5'));
    assert_eq!(r.cursor.0, 2);
}

#[test]
fn scrolled_editor_borders_show_hidden_rows() {
    let mut editor = Editor::new();
    editor.set_text("one\ntwo\nthree\nfour\nfive");
    let options = EditorRenderOpts { max_height: 2, ..Default::default() };
    let theme = Theme::default();
    let end = editor.render(30, &theme, &options);
    assert!(end.lines[0].plain().contains("↑ 3 more"), "{:?}", end.lines[0].plain());
    editor.set_cursor(0, 0);
    let start = editor.render(30, &theme, &options);
    assert!(start.lines.last().unwrap().plain().contains("↓ 3 more"), "{:?}", start.lines.last().unwrap().plain());
}

#[test]
fn paste_folding_roundtrip() {
    let mut e = Editor::new();
    typ(&mut e, "see ");
    let big: String = (0..20).map(|i| format!("line {i}\r\n")).collect();
    e.handle_paste(&big);
    typ(&mut e, " ok");
    let expected_content = big.replace("\r\n", "\n");
    assert!(e.raw_text().starts_with("see [Pasted text #1 +21 lines] ok"));
    assert_eq!(e.text(), format!("see {expected_content} ok"));
    // cursor can't land inside the token
    for _ in 0..4 {
        e.handle_key(key(KeyCode::Left));
    }
    assert_eq!(e.cursor().1, 4);
    e.handle_key(key(KeyCode::Right));
    assert_eq!(e.cursor().1, "see [Pasted text #1 +21 lines]".len());
    e.handle_key(key(KeyCode::Backspace));
    assert_eq!(e.raw_text(), "see  ok");
    assert_eq!(e.text(), "see  ok");
    // small paste inserted inline
    e.clear();
    e.handle_paste("a\tb\n\tc");
    assert_eq!(e.raw_text(), "a  b\n  c", "a pasted Tab is two spaces");
}

#[test]
fn a_long_single_line_paste_shows_its_character_count() {
    let mut editor = Editor::new();
    let text = "界".repeat(801);
    editor.handle_paste(&text);
    assert_eq!(editor.raw_text(), "[Pasted text #1 (801 chars)]");
    assert_eq!(editor.text(), text, "the submitted content remains the original paste");
}

#[test]
fn tokens_for_completion() {
    let mut e = Editor::new();
    typ(&mut e, "look at @src/ma");
    assert_eq!(e.current_token(), Some((8, "@src/ma".into())));
    e.replace_current_token("@src/main.rs ");
    assert_eq!(e.raw_text(), "look at @src/main.rs ");
    assert_eq!(e.current_token(), None);
    assert_eq!(e.handle_key(key(KeyCode::Escape)), EditorAction::Unhandled);
    assert_eq!(e.handle_key(key(KeyCode::Tab)), EditorAction::Unhandled);
    assert_eq!(e.handle_key(ctrl('t')), EditorAction::Unhandled);
    // a quoted reference keeps its spaces until the closing quote
    e.clear();
    typ(&mut e, "see @\"my dir/no");
    assert_eq!(e.current_token(), Some((4, "@\"my dir/no".into())));
    e.replace_current_token("@\"my dir/notes.md\" ");
    assert_eq!(e.raw_text(), "see @\"my dir/notes.md\" ");
    assert_eq!(e.current_token(), None);
    typ(&mut e, "x\"y z");
    assert_eq!(e.current_token(), Some((27, "z".into())), "a quote inside a word is no reference");
}

/// The input box draws with `theme.border` (`mare.o.border`); the default is the rounded box.
#[test]
fn the_box_follows_the_theme_border() {
    let mut e = Editor::new();
    e.set_text("hi");
    let mut th = Theme::default();
    let r = e.render(12, &th, &EditorRenderOpts::default());
    assert_eq!(r.lines[0].plain(), "╭──────────╮");
    assert_eq!(r.lines[2].plain(), "╰──────────╯");
    th.border = "double".into();
    let r = e.render(12, &th, &EditorRenderOpts::default());
    assert_eq!(r.lines[0].plain(), "╔══════════╗");
    assert!(r.lines[1].plain().starts_with('║') && r.lines[1].plain().ends_with('║'));
    assert_eq!(r.lines[2].plain(), "╚══════════╝");
}

#[test]
fn kill_yank_preserves_element_id_and_undo_or_yank_across_submit_keeps_paste_content() {
    let big: String = (0..12).map(|i| format!("line {i}\n")).collect();
    let mut e = Editor::new();
    let id = e.insert_token("[Image #1]", "[Image #1]");
    e.insert_str(" ");
    e.handle_paste(&big);
    // Kill the whole line (Ctrl+U) then yank it back twice (Ctrl+Y, Ctrl+Y): element id & paste content survive.
    e.handle_key(ctrl('u'));
    assert_eq!(e.raw_text(), "");
    assert!(e.text_with_elements().1.is_empty());
    e.handle_key(ctrl('y'));
    let (txt, elems) = e.text_with_elements();
    assert_eq!(txt, format!("[Image #1] {big}"));
    assert_eq!(elems.len(), 2);
    assert_eq!(elems[0].id, id, "yanked element must keep its original id");
    e.handle_key(ctrl('y'));
    let (txt2, elems2) = e.text_with_elements();
    assert_eq!(txt2, format!("[Image #1] {big}[Image #1] {big}"));
    assert_eq!(elems2.len(), 4);
    assert_eq!(elems2[0].id, id);
    assert_eq!(elems2[2].id, id);

    // Submit via Enter (clears editor), then Ctrl+_ (undo) restores placeholders and expands without panicking.
    let EditorAction::Submit(submitted) = e.handle_key(key(KeyCode::Enter)) else {
        panic!("expected submit");
    };
    assert_eq!(submitted, format!("[Image #1] {big}[Image #1] {big}"));
    assert_eq!(e.raw_text(), "");
    e.handle_key(ctrl('_'));
    assert_eq!(e.text(), format!("[Image #1] {big}[Image #1] {big}"));

    // Cross-submit yank: kill a paste, submit something else, then Ctrl+Y still expands the killed paste.
    let mut e2 = Editor::new();
    e2.handle_paste(&big);
    e2.handle_key(ctrl('u'));
    typ(&mut e2, "other");
    assert_eq!(e2.handle_key(key(KeyCode::Enter)), EditorAction::Submit("other".into()));
    e2.handle_key(ctrl('y'));
    assert_eq!(e2.text(), big);
}

/// Exhaustive branch equivalence for `handle_key` vs `DEFAULT_TEXT_KEYS`, `apply`, `enter`,
/// and `insert_char`, plus the two intentional refinements (`Ctrl+H` shell-step; `DeleteForward`
/// declining on empty input).
#[test]
fn handle_key_and_default_text_keys_cover_all_legacy_branches() {
    let ev = |code: KeyCode, m: Modifiers| KeyEvent::new(code, m);
    let release = KeyEvent { kind: KeyEventKind::Release, ..key(KeyCode::Char('a')) };

    // 1. Key release -> None
    let mut e = Editor::new();
    assert_eq!(e.handle_key(release), EditorAction::None);

    // 2. Enter variants: Shift+Enter, Alt+Enter, Alt+Shift+Enter, Ctrl+J, Enter backslash-continuation, plain Enter, Ctrl+Enter
    for m in [Modifiers::SHIFT, Modifiers::ALT, Modifiers::ALT | Modifiers::SHIFT] {
        let mut e = Editor::new();
        typ(&mut e, "a");
        assert_eq!(e.handle_key(ev(KeyCode::Enter, m)), EditorAction::Changed);
        assert_eq!(e.raw_text(), "a\n");
    }
    {
        let mut e = Editor::new();
        typ(&mut e, "a");
        assert_eq!(e.handle_key(ctrl('j')), EditorAction::Changed);
        assert_eq!(e.raw_text(), "a\n");
        e.handle_key(ev(KeyCode::Char('\\'), Modifiers::NONE));
        assert_eq!(e.handle_key(key(KeyCode::Enter)), EditorAction::Changed);
        assert_eq!(e.raw_text(), "a\n\n");
        assert_eq!(e.handle_key(key(KeyCode::Enter)), EditorAction::Submit("a\n\n".into()));
        assert_eq!(e.handle_key(ev(KeyCode::Enter, Modifiers::CONTROL)), EditorAction::Unhandled);
    }

    // 3. Word/char/line/vertical motions including Shift & Ctrl modifiers
    {
        let mut e = Editor::new();
        typ(&mut e, "one two three");
        assert_eq!(e.handle_key(ev(KeyCode::Left, Modifiers::CONTROL)), EditorAction::None);
        assert_eq!(e.cursor().1, "one two ".len());
        assert_eq!(e.handle_key(ev(KeyCode::Left, Modifiers::ALT)), EditorAction::None);
        assert_eq!(e.cursor().1, "one ".len());
        assert_eq!(e.handle_key(alt(KeyCode::Char('b'))), EditorAction::None);
        assert_eq!(e.cursor().1, 0);
        assert_eq!(e.handle_key(ev(KeyCode::Right, Modifiers::CONTROL)), EditorAction::None);
        assert_eq!(e.cursor().1, "one".len());
        assert_eq!(e.handle_key(ev(KeyCode::Right, Modifiers::ALT)), EditorAction::None);
        assert_eq!(e.cursor().1, "one two".len());
        assert_eq!(e.handle_key(alt(KeyCode::Char('f'))), EditorAction::None);
        assert_eq!(e.cursor().1, "one two three".len());

        // Plain, Ctrl+B/F, and Shift char movement / selection collapse
        assert_eq!(e.handle_key(key(KeyCode::Left)), EditorAction::None);
        assert_eq!(e.handle_key(ctrl('b')), EditorAction::None);
        assert_eq!(e.cursor().1, "one two thr".len());
        assert_eq!(e.handle_key(key(KeyCode::Right)), EditorAction::None);
        assert_eq!(e.handle_key(ctrl('f')), EditorAction::None);
        assert_eq!(e.cursor().1, "one two three".len());
        assert_eq!(e.handle_key(ev(KeyCode::Left, Modifiers::SHIFT)), EditorAction::None);
        assert_eq!(e.handle_key(ev(KeyCode::Left, Modifiers::SHIFT)), EditorAction::None);
        assert_eq!(e.selected_text().as_deref(), Some("ee"));
        assert_eq!(e.handle_key(key(KeyCode::Left)), EditorAction::None);
        assert_eq!(e.selection(), None);
        assert_eq!(e.cursor().1, "one two thr".len());
        assert_eq!(e.handle_key(ev(KeyCode::Right, Modifiers::SHIFT)), EditorAction::None);
        assert_eq!(e.selected_text().as_deref(), Some("e"));
        assert_eq!(e.handle_key(key(KeyCode::Right)), EditorAction::None);
        assert_eq!(e.cursor().1, "one two thre".len());
        assert_eq!(e.handle_key(key(KeyCode::Right)), EditorAction::None);
        assert_eq!(e.cursor().1, "one two three".len());

        // Home / End (plain, Shift, Ctrl) & Ctrl+A / Ctrl+E
        for hk in
            [key(KeyCode::Home), ev(KeyCode::Home, Modifiers::SHIFT), ev(KeyCode::Home, Modifiers::CONTROL), ctrl('a')]
        {
            e.col = 5;
            assert_eq!(e.handle_key(hk), EditorAction::None);
            assert_eq!(e.cursor().1, 0);
        }
        for ek in
            [key(KeyCode::End), ev(KeyCode::End, Modifiers::SHIFT), ev(KeyCode::End, Modifiers::CONTROL), ctrl('e')]
        {
            e.col = 0;
            assert_eq!(e.handle_key(ek), EditorAction::None);
            assert_eq!(e.cursor().1, "one two three".len());
        }
    }

    // 4. Vertical Up/Down (plain & Ctrl+P/N enter history on empty/edges; Shift+Up/Down stay in selection)
    {
        let mut e = Editor::new();
        for k in [key(KeyCode::Up), ctrl('p')] {
            assert_eq!(e.handle_key(k), EditorAction::HistoryPrev);
        }
        assert_eq!(e.handle_key(ev(KeyCode::Up, Modifiers::SHIFT)), EditorAction::None);
        for k in [key(KeyCode::Down), ctrl('n')] {
            assert_eq!(e.handle_key(k), EditorAction::HistoryNext);
        }
        assert_eq!(e.handle_key(ev(KeyCode::Down, Modifiers::SHIFT)), EditorAction::None);
        typ(&mut e, "top\nbot");
        assert_eq!(e.handle_key(ev(KeyCode::Up, Modifiers::SHIFT)), EditorAction::None);
        assert_eq!(e.cursor().0, 0);
        assert_eq!(e.handle_key(ev(KeyCode::Down, Modifiers::SHIFT)), EditorAction::None);
        assert_eq!(e.cursor().0, 1);
        assert_eq!(e.handle_key(key(KeyCode::Up)), EditorAction::None);
        assert_eq!(e.handle_key(ctrl('p')), EditorAction::None);
        assert_eq!(e.cursor().0, 0);
        assert_eq!(e.handle_key(ctrl('n')), EditorAction::None);
        assert_eq!(e.cursor().0, 1);
    }

    // 5. Backspace / Shift+Backspace / Ctrl+H (including shell-mode step back) & word deletes
    {
        let mut e = Editor::new();
        e.enable_shell_mode();
        assert_eq!(e.handle_key(key(KeyCode::Char('!'))), EditorAction::Changed);
        assert_eq!(e.handle_key(key(KeyCode::Char('!'))), EditorAction::Changed);
        assert_eq!(e.shell_level(), 2);
        assert_eq!(e.handle_key(ev(KeyCode::Backspace, Modifiers::SHIFT)), EditorAction::Changed);
        assert_eq!(e.shell_level(), 1);
        // Intentional fix: Ctrl+H shares DeleteBack shell-level step.
        assert_eq!(e.handle_key(ctrl('h')), EditorAction::Changed);
        assert_eq!(e.shell_level(), 0);

        // Shift+Backspace and Shift+Delete delete single graphemes
        let mut e = Editor::new();
        typ(&mut e, "abc");
        e.handle_key(key(KeyCode::Left));
        assert_eq!(e.handle_key(ev(KeyCode::Backspace, Modifiers::SHIFT)), EditorAction::Changed);
        assert_eq!(e.raw_text(), "ac");
        e.handle_key(key(KeyCode::Left));
        assert_eq!(e.handle_key(ev(KeyCode::Delete, Modifiers::SHIFT)), EditorAction::Changed);
        assert_eq!(e.raw_text(), "c");

        let mut e = Editor::new();
        typ(&mut e, "one two three four");
        assert_eq!(e.handle_key(ev(KeyCode::Backspace, Modifiers::CONTROL)), EditorAction::Changed);
        assert_eq!(e.handle_key(ev(KeyCode::Backspace, Modifiers::ALT)), EditorAction::Changed);
        assert_eq!(e.handle_key(ctrl('w')), EditorAction::Changed);
        assert_eq!(e.raw_text(), "one ");
        e.col = 0;
        assert_eq!(e.handle_key(ev(KeyCode::Delete, Modifiers::CONTROL)), EditorAction::Changed);
        assert_eq!(e.raw_text(), " ");
        typ(&mut e, "foo bar");
        e.col = 0;
        assert_eq!(e.handle_key(ev(KeyCode::Delete, Modifiers::ALT)), EditorAction::Changed);
        assert_eq!(e.handle_key(alt(KeyCode::Char('d'))), EditorAction::Changed);
        assert_eq!(e.raw_text(), " ");

        // Ctrl+D & DeleteForward on empty -> Unhandled (decline), non-empty -> deletes forward
        let mut empty = Editor::new();
        assert_eq!(empty.handle_key(ctrl('d')), EditorAction::Unhandled);
        assert_eq!(empty.handle_key(key(KeyCode::Delete)), EditorAction::Unhandled);
    }

    // 6. Kill/yank/undo & Turkish capital İ
    {
        let mut e = Editor::new();
        typ(&mut e, "alpha beta");
        e.col = 6;
        assert_eq!(e.handle_key(ctrl('k')), EditorAction::Changed);
        assert_eq!(e.raw_text(), "alpha ");
        e.handle_key(key(KeyCode::Left));
        e.col = 6;
        assert_eq!(e.handle_key(ctrl('u')), EditorAction::Changed);
        assert_eq!(e.raw_text(), "");
        assert_eq!(e.handle_key(ctrl('y')), EditorAction::Changed);
        assert_eq!(e.raw_text(), "alpha ");
        assert_eq!(e.handle_key(alt(KeyCode::Char('y'))), EditorAction::Changed);
        assert_eq!(e.raw_text(), "beta");
        assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
        assert_eq!(e.raw_text(), "");
        assert_eq!(e.handle_key(ev(KeyCode::Char('İ'), Modifiers::NONE)), EditorAction::Changed);
        assert_eq!(e.raw_text(), "İ");
    }
}

/// Grapheme/cross-line selection replacement, unshifted clear, and `MareSelection` render.
#[test]
fn selection_ops_grapheme_cross_line_highlight_and_replace() {
    let ev = |code: KeyCode, m: Modifiers| KeyEvent::new(code, m);
    assert!(TextOp::parse("text.select_left").is_some(), "text.select_left op must exist");
    assert!(TextOp::parse("text.select_right").is_some(), "text.select_right op must exist");
    assert!(TextOp::parse("text.select_word_left").is_some(), "text.select_word_left op must exist");
    assert!(TextOp::parse("text.select_word_right").is_some(), "text.select_word_right op must exist");
    assert!(TextOp::parse("text.select_line_start").is_some(), "text.select_line_start op must exist");
    assert!(TextOp::parse("text.select_line_end").is_some(), "text.select_line_end op must exist");
    assert!(TextOp::parse("text.select_up").is_some(), "text.select_up op must exist");
    assert!(TextOp::parse("text.select_down").is_some(), "text.select_down op must exist");
    assert!(TextOp::parse("text.select_all").is_some(), "text.select_all op must exist");
    assert!(TextOp::parse("text.redo").is_some(), "text.redo op must exist");

    // 1. Grapheme atomicity + selection highlight + replacement by typing & one-step undo/redo.
    let mut e = Editor::new();
    typ(&mut e, "中文👍🏽x");
    e.handle_key(ev(KeyCode::Left, Modifiers::SHIFT)); // select 'x'
    e.handle_key(ev(KeyCode::Left, Modifiers::SHIFT)); // extend across 👍🏽 as one grapheme
    let th = Theme::default();
    let sel_bg = th.style("MareSelection").bg;
    assert!(sel_bg.is_some());
    let rendered = e.render(24, &th, &EditorRenderOpts::default());
    let highlighted: String =
        rendered.lines[1].spans.iter().filter(|s| s.style.bg == sel_bg).map(|s| s.text.as_str()).collect();
    assert_eq!(highlighted, "👍🏽x", "rendered selection must highlight the grapheme-atomic span");

    // Typing replaces the whole selection as one undo unit.
    assert_eq!(e.handle_key(key(KeyCode::Char('Y'))), EditorAction::Changed);
    assert_eq!(e.raw_text(), "中文Y");
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e.raw_text(), "中文👍🏽x");
    assert_eq!(e.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)), EditorAction::Changed);
    assert_eq!(e.raw_text(), "中文Y");

    // 2. Unshifted movement clears selection without deleting text when typing afterwards.
    let mut e = Editor::new();
    typ(&mut e, "hello world");
    e.handle_key(ev(KeyCode::Left, Modifiers::CONTROL | Modifiers::SHIFT)); // select "world"
    e.handle_key(key(KeyCode::Right)); // unshifted move clears selection
    typ(&mut e, "!");
    assert_eq!(e.raw_text(), "hello world!");

    // Cross-line selection + Backspace/paste replacement + undo/redo + SelectAll.
    let mut e = Editor::new();
    typ(&mut e, "alpha\nbeta\ngamma");
    e.handle_key(ev(KeyCode::Up, Modifiers::SHIFT));
    e.handle_key(ev(KeyCode::Up, Modifiers::SHIFT));
    assert_eq!(e.selected_text().as_deref(), Some("\nbeta\ngamma"));
    assert_eq!(e.handle_key(key(KeyCode::Backspace)), EditorAction::Changed);
    assert_eq!(e.raw_text(), "alpha");
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e.raw_text(), "alpha\nbeta\ngamma");
    assert_eq!(e.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)), EditorAction::Changed);
    assert_eq!(e.raw_text(), "alpha");

    assert_eq!(e.apply(TextOp::SelectAll), EditorAction::None);
    assert_eq!(e.selected_text().as_deref(), Some("alpha"));
    assert_eq!(e.apply(TextOp::DeleteForward), EditorAction::Changed);
    assert_eq!(e.raw_text(), "");
}

/// Atomic paste/image tokens under selection, deletion, undo, and redo.
#[test]
fn selection_and_redo_keep_paste_and_image_tokens_atomic() {
    let ev = |code: KeyCode, m: Modifiers| KeyEvent::new(code, m);
    let big: String = (0..12).map(|i| format!("row {i}\n")).collect();
    let mut e = Editor::new();
    let img_id = e.insert_token("[Image #1]", "[Image #1]");
    typ(&mut e, " mid\nnext ");
    e.handle_paste(&big);
    let paste_id = e.text_with_elements().1[1].id;

    // One Shift+Left selects the entire [Pasted text #1] token atomically; Left collapses before it.
    e.handle_key(ev(KeyCode::Left, Modifiers::SHIFT));
    assert!(e.selected_text().as_deref().is_some_and(|s| s.starts_with("[Pasted text #1")));
    e.handle_key(key(KeyCode::Left));

    // Cross-line selection from before [Pasted text #1] up to start (covering [Image #1]) -> Shift+Home/Shift+Up/Shift+Home.
    e.handle_key(ev(KeyCode::Home, Modifiers::SHIFT));
    e.handle_key(ev(KeyCode::Up, Modifiers::SHIFT));
    e.handle_key(ev(KeyCode::Home, Modifiers::SHIFT));
    assert_eq!(e.handle_key(key(KeyCode::Delete)), EditorAction::Changed);
    assert!(e.raw_text().starts_with("[Pasted text #1"));
    assert_eq!(e.text_with_elements().1.len(), 1, "[Image #1] deleted atomically");
    assert!(e.referenced_element_ids().contains(&img_id), "undo must retain deleted image id");

    // Undo restores [Image #1]; Redo re-deletes it while redo stack retains paste_id/img_id cleanly.
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e.text_with_elements().1.len(), 2);
    assert!(e.referenced_element_ids().contains(&img_id));
    assert!(e.referenced_element_ids().contains(&paste_id));

    assert_eq!(e.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)), EditorAction::Changed);
    assert_eq!(e.text_with_elements().1.len(), 1);
    assert!(e.referenced_element_ids().contains(&img_id));

    // Undo right after insert_token puts [Image #1] ONLY on redo stack: referenced_element_ids must keep it!
    let mut e2 = Editor::new();
    let solo_id = e2.insert_token("[Image #1]", "[Image #1]");
    assert_eq!(e2.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e2.raw_text(), "");
    assert!(e2.referenced_element_ids().contains(&solo_id), "redo stack must keep undone attachment ids reachable");
    assert_eq!(e2.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)), EditorAction::Changed);
    assert_eq!(e2.raw_text(), "[Image #1]");
}

/// Symmetric redo stack, preserved across navigation, cleared by new edit.
#[test]
fn redo_preserves_across_navigation_and_clears_on_new_edit() {
    let ev = |code: KeyCode, m: Modifiers| KeyEvent::new(code, m);
    let mut e = Editor::new();
    typ(&mut e, "one");
    e.handle_key(key(KeyCode::Char(' ')));
    typ(&mut e, "two");
    // Space starts a new undo unit so first Ctrl+Z reverts `" two"` back to `"one"`.
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e.raw_text(), "one");
    // Pure navigation does not wipe redo.
    e.handle_key(key(KeyCode::Left));
    e.handle_key(ev(KeyCode::Right, Modifiers::SHIFT));
    assert_eq!(e.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)), EditorAction::Changed);
    assert_eq!(e.raw_text(), "one two");

    // Undo then new edit clears redo.
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    typ(&mut e, "three");
    assert_eq!(
        e.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)),
        EditorAction::None,
        "new edit after undo must clear redo stack"
    );
    assert_eq!(e.raw_text(), "onethree");
}

/// No-op deletes at boundaries must preserve redo.
#[test]
fn noop_deletes_at_boundaries_preserve_redo() {
    let ev = |code: KeyCode, m: Modifiers| KeyEvent::new(code, m);
    let mut e = Editor::new();
    typ(&mut e, "one");
    e.handle_key(key(KeyCode::Char(' ')));
    typ(&mut e, "two");
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e.raw_text(), "one");

    // DeleteForward at end of buffer is a no-op and must not clear redo.
    assert_eq!(e.handle_key(key(KeyCode::Delete)), EditorAction::None);
    // Move to (0, 0): Backspace and Ctrl+W at start are no-ops and must not clear redo.
    e.handle_key(key(KeyCode::Home));
    assert_eq!(e.handle_key(key(KeyCode::Backspace)), EditorAction::None);
    assert_eq!(e.handle_key(ctrl('w')), EditorAction::None);

    assert_eq!(e.handle_key(ev(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT)), EditorAction::Changed);
    assert_eq!(e.raw_text(), "one two");
}

/// Inline paste replaces selection, clears anchor, records undo.
#[test]
fn paste_inline_replaces_selection_clears_anchor_and_supports_undo() {
    let ev = |code: KeyCode, m: Modifiers| KeyEvent::new(code, m);
    let mut e = Editor::new();
    e.set_text("hello world");
    // Reverse selection: anchor (0, 11), cursor (0, 0).
    e.handle_key(ev(KeyCode::Home, Modifiers::SHIFT));
    assert_eq!(e.selected_text().as_deref(), Some("hello world"));

    e.paste_inline("a\nb");
    assert_eq!(e.raw_text(), "a\nb");
    assert!(e.selection().is_none(), "paste_inline must clear selection_anchor");
    // Pressing Left must not panic with stale anchor at (0, 11).
    e.handle_key(key(KeyCode::Left));
    assert_eq!(e.selected_text(), None);
    // Undo restores previous buffer cleanly.
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert_eq!(e.raw_text(), "hello world");
}

#[test]
fn clamp_pos_respects_grapheme_boundaries_and_atomic_tokens() {
    let mut editor = Editor::new();
    editor.set_text("xก้z\n界ﾞ!");
    for (row, col, expected) in [
        (0, 3, (0, 1)),
        (0, 7, (0, 7)),
        (0, usize::MAX, (0, 8)),
        (1, 4, (1, 0)),
        (1, 6, (1, 6)),
        (usize::MAX, usize::MAX, (1, 7)),
    ] {
        assert_eq!(editor.clamp_pos(row, col), expected, "row={row}, col={col}");
    }
    editor.set_text("prefix ");
    let start = editor.cursor().1;
    editor.insert_token("[Image #1]", "image");
    let end = editor.cursor().1;
    assert_eq!(editor.clamp_pos(0, start + 2), (0, start));
    assert_eq!(editor.clamp_pos(0, end), (0, end));
    assert_eq!(editor.clamp_pos(0, usize::MAX), (0, end));
}

#[test]
fn large_single_line_and_multiline_insert_tab_undo_and_cursor_motion() {
    let mut e = Editor::new();
    // 8,192-char single-line typing/insert + undo grouping.
    let big_line: String = (0..8192).map(|i| if i % 17 == 16 { ' ' } else { 'a' }).collect();
    for ch in big_line.chars() {
        e.insert_char(ch);
    }
    assert_eq!(e.raw_text(), big_line);
    assert_eq!(e.cursor(), (0, 8192));
    // Undoing rolls back word-by-word cleanly.
    assert_eq!(e.handle_key(ctrl('z')), EditorAction::Changed);
    assert!(e.raw_text().len() < 8192);

    // Tab expansion accuracy mid-line and multi-line.
    let mut tab_ed = Editor::new();
    tab_ed.insert_str("a\tb\ncd\t\te");
    assert_eq!(tab_ed.raw_text(), "a   b\ncd      e");
    tab_ed.set_text("x");
    tab_ed.insert_str("\ty");
    assert_eq!(tab_ed.raw_text(), "x   y");

    // 2,000-line multi-line insert in the middle of existing content with atomic elements.
    let mut multi = Editor::new();
    multi.insert_str("head ");
    multi.insert_token("[Image #1]", "img1");
    multi.insert_str(" mid ");
    multi.insert_token("[Image #2]", "img2");
    multi.insert_str(" tail");
    // Move cursor to right after " mid " (between elements).
    multi.set_cursor(0, "head [Image #1] mid ".len());
    let mut patch = String::new();
    for i in 0..2000 {
        if i > 0 {
            patch.push('\n');
        }
        let _ = std::fmt::Write::write_fmt(&mut patch, format_args!("ins-{i}"));
    }
    multi.insert_str(&patch);
    let (expanded, elems) = multi.text_with_elements();
    assert_eq!(elems.len(), 2);
    assert_eq!(elems[0].text, "img1");
    assert_eq!(elems[1].text, "img2");
    assert!(expanded.starts_with("head img1 mid ins-0\n"));
    assert!(expanded.ends_with("\nins-1999img2 tail"));
    assert_eq!(multi.cursor(), (1999, "ins-1999".len()));

    // Cursor-only vertical moves across 5,000 lines do not bump text_revision and match render.
    let rev_before = multi.text_revision();
    let opts = EditorRenderOpts { max_height: 8, ..Default::default() };
    let theme = Theme::default();
    let r0 = multi.render(60, &theme, &opts);
    assert_eq!(r0.lines.len(), 10);
    for _ in 0..5 {
        assert_eq!(multi.apply(TextOp::Up), EditorAction::None);
    }
    assert_eq!(multi.text_revision(), rev_before);
    let r1 = multi.render(60, &theme, &opts);
    assert_eq!(r1.lines.len(), 10);
    assert_eq!(multi.cursor().0, 1994);
}

#[test]
fn multiline_kill_and_yank_preserves_atomic_tokens_across_rows() {
    let mut e = Editor::new();
    e.insert_str("line0 ");
    e.insert_token("[Image #1]", "img-a");
    e.insert_str("\nline1 ");
    e.insert_token("[Image #2]", "img-b");
    e.insert_str(" end");
    // Accumulate consecutive Ctrl+K (`KillLineEnd`) across both rows and the newline between them.
    e.set_cursor(0, 0);
    assert_eq!(e.apply(TextOp::KillLineEnd), EditorAction::Changed);
    assert_eq!(e.apply(TextOp::KillLineEnd), EditorAction::Changed);
    assert_eq!(e.apply(TextOp::KillLineEnd), EditorAction::Changed);
    assert_eq!(e.raw_text(), "");

    // Yank back on a fresh prefix and verify both rows keep their atomic tokens intact.
    e.insert_str("pre ");
    assert_eq!(e.apply(TextOp::Yank), EditorAction::Changed);
    let (expanded, elems) = e.text_with_elements();
    assert_eq!(expanded, "pre line0 img-a\nline1 img-b end");
    assert_eq!(elems.len(), 2);
    assert_eq!(elems[0].text, "img-a");
    assert_eq!(elems[1].text, "img-b");
}

#[test]
fn masked_render_preserves_cursor_and_scroll() {
    let mut e = Editor::new();
    e.set_text("secret1234567890");
    e.set_cursor(0, 6);
    let theme = Theme::default();
    let opts = EditorRenderOpts { mask_char: Some('•'), ..Default::default() };
    let r = e.render(14, &theme, &opts);
    let plain: String = r.lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n");
    assert!(!plain.contains("secret"), "secret must be masked: {plain}");
    assert!(plain.contains("••••••••"), "mask bullets expected: {plain}");
    assert!(r.cursor.0 >= 1 && r.cursor.1 > 0);
}
