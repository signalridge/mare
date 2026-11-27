use super::width::wcwidth_sum;
use super::*;

#[test]
fn no_color_keeps_only_bold_underline_and_reverse() {
    let st = Style::new().fg(Color::Rgb(1, 2, 3)).bg(Color::Ansi(4)).bold().underline().italic().dim().reverse();
    let sgr = style_sgr(st, ColorMode::NoColor);
    assert_eq!(sgr, "\x1b[0m\x1b[1m\x1b[4m\x1b[7m");
    let l = Line::styled("hi", st).with_fill(Color::Rgb(9, 9, 9));
    let a = line_to_ansi(&l, 6, ColorMode::NoColor);
    assert!(!a.contains("38;") && !a.contains("48;") && a.ends_with("    \x1b[0m"), "{a:?}");
    assert!(no_color_env(&|k| (k == "NO_COLOR").then(|| "1".into())));
    assert!(!no_color_env(&|k| (k == "NO_COLOR").then(String::new)), "empty means unset");
}

#[test]
fn url_tokens() {
    for u in [
        "https://example.com/a/b?c=d#e",
        "(https://github.com/signalridge/mare).",
        "example.com/path",
        "www.example.com",
        "localhost:3000/api",
        "192.168.1.1:8080/health",
        "myapp://open",
    ] {
        assert!(is_url_like(u), "{u}");
    }
    for t in ["src/main.rs", "foo/bar", "example.com", "http://", "a.b:99999/x", "hello"] {
        assert!(!is_url_like(t), "{t}");
    }
}

#[test]
fn a_url_is_never_broken_mid_url() {
    let url = "https://example.com/a/very/long/path/that/does/not/fit/in/twenty/cells";
    // a line that is only a URL stays one row (wider than the width)
    let rows = wrap_line(&Line::raw(url), 20);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].plain(), url);
    // prose around it still wraps; the URL keeps a row of its own
    let rows = wrap_line(&Line::raw(format!("see {url} for details")), 20);
    let plain: Vec<String> = rows.iter().map(Line::plain).collect();
    assert_eq!(plain, vec!["see".to_string(), url.to_string(), "for details".to_string()]);
    // a long word that isn't a URL is still hard-broken
    assert_eq!(wrap_line(&Line::raw("x".repeat(30)), 20).len(), 2);
}

#[test]
fn display_path_tilde_and_middle_elision() {
    assert_eq!(display_path("/home/u/src/repo", "/home/u", 40), "~/src/repo");
    assert_eq!(display_path("/home/u", "/home/u/", 40), "~");
    assert_eq!(display_path("/home/user2/x", "/home/u", 40), "/home/user2/x", "not a prefix by component");
    let p = "/home/u/scratchpad/claude-501/uxlab/repo";
    assert_eq!(display_path(p, "/home/u", 14), "…/uxlab/repo");
    assert_eq!(display_path(p, "/home/u", 12), "…/uxlab/repo");
    assert_eq!(display_path(p, "/home/u", 11), "…/repo");
    assert_eq!(display_path(p, "/home/u", 0), "~/scratchpad/claude-501/uxlab/repo", "0 = no limit");
    // one component too wide: keep its end
    assert_eq!(display_path("/a/very-long-directory-name", "", 8), "…ry-name");
    // CJK counts two cells per character
    assert_eq!(display_path("/home/u/项目/代码库", "/home/u", 9), "…/代码库");
    assert!(str_width(&display_path("/home/u/项目/代码库", "/home/u", 5)) <= 5);
    // Windows paths
    assert_eq!(display_path(r"C:\Users\u\src\proj", r"C:\Users\u", 40), r"~\src\proj");
    assert_eq!(display_path(r"D:\work\deep\proj", "", 10), r"…\proj");
}

#[cfg(unix)]
#[test]
fn display_path_collapses_symlinked_home_and_canonical_paths() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let real_home = tmp.path().join("real_home");
    let sub_repo = real_home.join("sub").join("repo");
    let sibling_dir = tmp.path().join("real_home2").join("other");
    std::fs::create_dir_all(&sub_repo).unwrap();
    std::fs::create_dir_all(&sibling_dir).unwrap();

    let link_home = tmp.path().join("link_home");
    let link_sub = tmp.path().join("link_sub");
    symlink(&real_home, &link_home).unwrap();
    symlink(real_home.join("sub"), &link_sub).unwrap();

    let canon_home = dunce::canonicalize(&real_home).unwrap();
    let canon_repo = dunce::canonicalize(&sub_repo).unwrap();
    let canon_sibling = dunce::canonicalize(&sibling_dir).unwrap();
    let link_home_str = link_home.to_string_lossy();
    let canon_home_str = canon_home.to_string_lossy();

    // Canonical path under real home with symlinked HOME (e.g. macOS /tmp -> /private/tmp).
    assert_eq!(display_path(&canon_repo.to_string_lossy(), &link_home_str, 0), "~/sub/repo");
    assert_eq!(display_path(&canon_home_str, &link_home_str, 40), "~");
    // Non-existent descendant inside canonical home still collapses against symlinked HOME.
    assert_eq!(
        display_path(&canon_home.join("future").join("file.rs").to_string_lossy(), &link_home_str, 0),
        "~/future/file.rs"
    );
    // Raw symlinked HOME path hits the direct prefix fast path.
    assert_eq!(display_path(&link_home.join("sub").join("repo").to_string_lossy(), &link_home_str, 0), "~/sub/repo");
    // Path-side symlinks outside HOME are never resolved on the render hot path.
    let link_sub_repo = link_sub.join("repo");
    assert_eq!(
        display_path(&link_sub_repo.to_string_lossy(), &canon_home_str, 0),
        link_sub_repo.to_string_lossy().as_ref()
    );
    // Component boundaries and relative paths are never mis-collapsed.
    assert_eq!(
        display_path(&canon_sibling.to_string_lossy(), &link_home_str, 0),
        canon_sibling.to_string_lossy().as_ref()
    );
    assert_eq!(display_path("sub/repo", &link_home_str, 40), "sub/repo");
    // Width elision still applies after tilde collapse.
    assert_eq!(display_path(&canon_repo.to_string_lossy(), &link_home_str, 6), "…/repo");
}

#[cfg(unix)]
#[test]
fn display_path_caches_canonical_home_and_skips_fs_for_non_home_paths() {
    use std::os::unix::fs::symlink;
    use std::path::Path;

    let tmp = tempfile::tempdir().unwrap();
    let real_home = tmp.path().join("real_home");
    let outside = tmp.path().join("outside");
    let link_home = tmp.path().join("link_home");
    let loop_a = tmp.path().join("loop_a");
    let loop_b = tmp.path().join("loop_b");
    std::fs::create_dir_all(real_home.join("proj")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&real_home, &link_home).unwrap();
    symlink(&loop_b, &loop_a).unwrap();
    symlink(&loop_a, &loop_b).unwrap();

    let canon_home = dunce::canonicalize(&real_home).unwrap();
    let link_home_str = link_home.to_string_lossy();
    let outside_str = outside.to_string_lossy();
    let loop_str = loop_a.to_string_lossy();

    let mut cache = super::cells::HomeCanonCache::new();
    let fs_calls = std::cell::Cell::new(0usize);
    let mut collapse = |path: &str| {
        super::cells::collapse_home_with(path, &link_home_str, |h| {
            cache.get_or_resolve(h, |p| {
                assert_eq!(p, Path::new(link_home_str.as_ref()), "only HOME may be canonicalized");
                fs_calls.set(fs_calls.get() + 1);
                dunce::canonicalize(p).ok().map(|c| c.to_string_lossy().trim_end_matches(['/', '\\']).to_owned())
            })
        })
    };

    // Direct raw-HOME hit does not touch the filesystem at all.
    assert_eq!(collapse(&link_home.join("proj").to_string_lossy()), "~/proj");
    assert_eq!(fs_calls.get(), 0);

    // First miss canonicalizes HOME once and caches it.
    assert_eq!(collapse(&outside_str), outside_str.as_ref());
    assert_eq!(fs_calls.get(), 1);

    // Subsequent non-HOME paths (existing, missing, cyclic symlink) and canonical HOME paths
    // must trigger zero filesystem calls.
    for i in 0..200 {
        let non_home = format!("{outside_str}/missing-{i}/file.rs");
        assert_eq!(collapse(&non_home), non_home);
    }
    assert_eq!(collapse(&loop_str), loop_str.as_ref());
    assert_eq!(collapse(&canon_home.join("proj").to_string_lossy()), "~/proj");
    assert_eq!(fs_calls.get(), 1);

    // Public API matches the same behavior through the process-wide OnceLock cache.
    assert_eq!(display_path(&loop_str, &link_home_str, 0), loop_str.as_ref());
    assert_eq!(display_path(&canon_home.join("proj").to_string_lossy(), &link_home_str, 0), "~/proj");
}

#[test]
fn grapheme_widths_follow_terminal_rules() {
    assert_eq!(str_width("abc"), 3);
    assert_eq!(str_width("中文"), 4);
    assert_eq!(str_width("👍"), 2, "emoji");
    assert_eq!(str_width("👍🏽"), 2, "emoji + skin tone is one 2-cell cluster");
    assert_eq!(str_width("👨‍👩‍👧"), 2, "ZWJ family");
    assert_eq!(str_width("❤️"), 2, "VS16 makes it emoji presentation");
    assert_eq!(str_width("🇯🇵"), 2, "a flag");
    assert_eq!(str_width("🇯"), 2, "a lone regional indicator during streaming stays 2");
    assert_eq!(str_width("e\u{301}"), 1, "combining acute: zero width");
    assert_eq!(str_width("\u{200B}"), 0, "zero-width space");
    assert_eq!(str_width("คำ"), 2, "Thai AM vowel takes a cell");
    assert_eq!(str_width("क्षि"), 3, "Devanagari: the consonant after virama and the spacing vowel sign add cells");
}

#[test]
fn wrap_cjk() {
    let l = Line::raw("中文中文中文");
    let w = wrap_line(&l, 5);
    assert!(w.iter().all(|l| l.width() <= 5));
    assert_eq!(w.iter().map(|l| l.plain()).collect::<String>(), "中文中文中文");
}

#[test]
fn wrap_words() {
    let l = Line::raw("hello world foo");
    let w = wrap_line(&l, 11);
    assert_eq!(w[0].plain(), "hello world");
    assert_eq!(w[1].plain(), "foo");
}

#[test]
fn truncate() {
    assert_eq!(truncate_str("hello world", 6, "…"), "hello…");
    assert_eq!(truncate_str("中文中文", 5, "…"), "中文…");
}

#[test]
fn osc8_links() {
    let l = Line::new(vec![Span::raw("see "), Span::styled("docs", Style::new().link("https://x.dev"))]);
    let a = line_to_ansi(&l, 80, ColorMode::TrueColor);
    assert!(a.contains("\x1b]8;;https://x.dev\x1b\\"), "{a:?}");
    assert!(a.contains("docs\x1b]8;;\x1b\\"), "{a:?}");
}

#[test]
fn width_overrides_apply_and_reset() {
    assert_eq!(str_width("⚡"), 2);
    set_width_overrides(vec![(0x26A1, 0x26A1, 1)]);
    assert_eq!(str_width("⚡x"), 2);
    set_width_overrides(vec![]);
    assert_eq!(str_width("⚡"), 2);
}

#[test]
fn wcwidth_counts_every_code_point() {
    // (the global switch is left alone: other tests in this process measure clusters)
    assert_eq!(wcwidth_sum("👨‍👩‍👧"), 6, "a ZWJ family is three 2-cell emoji");
    assert_eq!(wcwidth_sum("❤️"), 1, "VS16 adds nothing to a 1-cell symbol");
    assert_eq!(wcwidth_sum("👍🏽"), 4, "the skin tone modifier is its own 2 cells");
    assert_eq!(wcwidth_sum("e\u{301}"), 1);
    assert_eq!(wcwidth_sum("中"), 2);
    assert_eq!(WidthMethod::parse(" WCWidth "), Some(WidthMethod::Wcwidth));
    assert_eq!(WidthMethod::parse("grapheme"), Some(WidthMethod::Unicode));
    assert_eq!(WidthMethod::parse("auto"), None);
}

#[test]
fn private_use_nerd_font_icons_have_cell_width_under_unicode_and_wcwidth() {
    assert_eq!(width_method(), WidthMethod::Unicode);
    for icon in ["\u{e7a8}", "\u{f489}", "\u{f0335}"] {
        assert_eq!(grapheme_width(icon), 1, "PUA icon {icon:?} under Unicode mode");
        assert_eq!(wcwidth_sum(icon), 1, "PUA icon {icon:?} under wcwidth");
    }
    assert_eq!(str_width(" \u{e7a8} Rust "), 8);
}

#[test]
fn sanitize_ansi() {
    assert_eq!(sanitize("\x1b[31mred\x1b[0m\r\n"), "red\n");
}

#[test]
fn image_prefix_offsets_accumulate_without_touching_payload() {
    let payload = "\r\x1b_Ga=T,i=17,p=1;PNGDATA\x1b\x5c";
    let image = Line { spans: vec![], fill_bg: None, raw: Some(payload.into()) };
    assert_eq!(image.clone().prefixed(vec![]).raw.as_deref(), Some(payload));
    let nested = image.prefixed(vec![Span::raw("中")]).prefixed(vec![Span::raw("  ")]);
    assert_eq!(nested.raw.as_deref(), Some("\r\x1b[2C\x1b[2C\x1b_Ga=T,i=17,p=1;PNGDATA\x1b\x5c"));
}

fn env_lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| pairs.iter().find(|(ek, _)| *ek == k).map(|(_, v)| (*v).to_string())
}

#[test]
fn color_mode_resolve_hierarchy_and_ansi16_encoding() {
    // Explicit option overrides everything including NO_COLOR and probe.
    assert_eq!(
        ColorMode::resolve(Some("16"), &env_lookup(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]), Some(true)),
        ColorMode::Ansi16
    );
    // "auto" defers to NO_COLOR > MARE_COLOR > COLORTERM > probe > terminal defaults.
    assert_eq!(
        ColorMode::resolve(Some("auto"), &env_lookup(&[("NO_COLOR", "1"), ("MARE_COLOR", "256")]), Some(true)),
        ColorMode::NoColor
    );
    assert_eq!(
        ColorMode::resolve(None, &env_lookup(&[("MARE_COLOR", "16"), ("COLORTERM", "truecolor")]), Some(true)),
        ColorMode::Ansi16
    );
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("COLORTERM", "truecolor")]), Some(false)), ColorMode::TrueColor);
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("TERM", "xterm-256color")]), Some(true)), ColorMode::TrueColor);
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("TERM", "xterm-256color")]), Some(false)), ColorMode::Ansi256);
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("TERM", "vt100")]), None), ColorMode::Ansi16);
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("TERM_PROGRAM", "Apple_Terminal")]), None), ColorMode::Ansi256);

    // Ansi16 emits only standard 16-color SGR codes (no 38;2; / 38;5; / 48;2; / 48;5).
    let st = Style::new().fg(Color::Rgb(220, 40, 40)).bg(Color::Ansi(236));
    let sgr = style_sgr(st, ColorMode::Ansi16);
    assert!(!sgr.contains("38;") && !sgr.contains("48;"), "Ansi16 must not emit extended SGR: {sgr:?}");
}

#[test]
fn color_mode_resolve_preserves_windows_terminal_and_term_program_truecolor() {
    // Windows Terminal sets WT_SESSION even when TERM/COLORTERM/TERM_PROGRAM are empty.
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("WT_SESSION", "wt-1")]), None), ColorMode::TrueColor);
    assert_eq!(
        ColorMode::resolve(None, &env_lookup(&[("WT_SESSION", "wt-1"), ("TERM", "")]), None),
        ColorMode::TrueColor
    );

    // Modern terminals over SSH/tmux with TERM=xterm-256color (no COLORTERM) keep TrueColor.
    for term_prog in ["iTerm.app", "WezTerm", "ghostty", "vscode"] {
        assert_eq!(
            ColorMode::resolve(None, &env_lookup(&[("TERM_PROGRAM", term_prog), ("TERM", "xterm-256color")]), None),
            ColorMode::TrueColor,
            "TERM_PROGRAM={term_prog} with TERM=xterm-256color must resolve to TrueColor"
        );
    }
    for env_key in
        ["KITTY_WINDOW_ID", "GHOSTTY_RESOURCES_DIR", "WEZTERM_PANE", "WEZTERM_EXECUTABLE", "ITERM_SESSION_ID"]
    {
        assert_eq!(
            ColorMode::resolve(None, &env_lookup(&[(env_key, "1"), ("TERM", "xterm-256color")]), None),
            ColorMode::TrueColor,
            "{env_key} with TERM=xterm-256color must resolve to TrueColor"
        );
    }
    for truecolor_term in ["xterm-kitty", "alacritty"] {
        assert_eq!(
            ColorMode::resolve(None, &env_lookup(&[("TERM", truecolor_term)]), None),
            ColorMode::TrueColor,
            "TERM={truecolor_term} must resolve to TrueColor"
        );
    }
    assert_eq!(
        ColorMode::resolve(None, &env_lookup(&[("TERM_PROGRAM", "Apple_Terminal"), ("TERM", "xterm-256color")]), None),
        ColorMode::Ansi256
    );
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("TERM", "xterm-256color")]), None), ColorMode::Ansi256);
    for ansi16_term in ["", "dumb", "linux", "vt100", "ansi", "xterm"] {
        assert_eq!(
            ColorMode::resolve(None, &env_lookup(&[("TERM", ansi16_term)]), None),
            ColorMode::Ansi16,
            "TERM={ansi16_term:?} must resolve to Ansi16"
        );
    }
    assert_eq!(
        ColorMode::resolve(None, &env_lookup(&[("WT_SESSION", "wt-1"), ("NO_COLOR", "1")]), None),
        ColorMode::NoColor
    );
    assert_eq!(ColorMode::resolve(Some("16"), &env_lookup(&[("WT_SESSION", "wt-1")]), Some(true)), ColorMode::Ansi16);
}

#[test]
fn resolve_icons_follows_canonical_hierarchy() {
    assert_eq!(resolve_icons(Some("ascii"), &env_lookup(&[("MARE_NERD_FONT", "1")]), Some(true)), "ascii");
    assert_eq!(resolve_icons(Some("auto"), &env_lookup(&[("NERD_FONT", "1")]), Some(false)), "nerd");
    assert_eq!(resolve_icons(None, &env_lookup(&[("TERM", "linux"), ("LANG", "C")]), Some(true)), "ascii");
    assert_eq!(resolve_icons(None, &env_lookup(&[]), Some(false)), "ascii");
    assert_eq!(resolve_icons(None, &env_lookup(&[]), Some(true)), "unicode");
    assert_eq!(
        resolve_icons(None, &env_lookup(&[("TERM_PROGRAM", "Apple_Terminal"), ("TERM_PROGRAM_VERSION", "440")]), None),
        "ascii"
    );
    assert_eq!(
        resolve_icons(None, &env_lookup(&[("TERM_PROGRAM", "Apple_Terminal"), ("TERM_PROGRAM_VERSION", "465")]), None),
        "unicode"
    );
    assert_eq!(resolve_icons(Some("nerd"), &env_lookup(&[]), Some(false)), "nerd");
    assert_eq!(resolve_icons(Some("unicode"), &env_lookup(&[]), Some(false)), "unicode");
}

#[test]
fn cells_color_parse_and_hyperlink_edge_cases() {
    // truncate_str when max <= ellipsis width.
    assert_eq!(truncate_str("abcdef", 1, "…"), "…");
    assert_eq!(truncate_str("abcdef", 2, "..."), "..");

    // Wide CJK grapheme straddling slice boundaries gets replaced with spaces.
    let sliced = slice_cells(&Line::raw("a中b"), 2, 4);
    assert_eq!(sliced.plain(), " b");

    // ColorMode::parse no_color/none and Ansi256 fallback without TERM_PROGRAM.
    assert_eq!(ColorMode::parse("none"), Some(ColorMode::NoColor));
    assert_eq!(ColorMode::parse("no_color"), Some(ColorMode::NoColor));
    assert_eq!(ColorMode::resolve(None, &env_lookup(&[("TERM", "screen-256color")]), None), ColorMode::Ansi256);

    // Color::parse named ANSI colors + rgb/ansi256 -> ansi16 downgrade paths.
    for (name, code) in [
        ("black", 0),
        ("red", 1),
        ("green", 2),
        ("yellow", 3),
        ("blue", 4),
        ("magenta", 5),
        ("cyan", 6),
        ("white", 7),
        ("gray", 8),
        ("bright_red", 9),
        ("bright_green", 10),
        ("bright_yellow", 11),
        ("bright_blue", 12),
        ("bright_magenta", 13),
        ("bright_cyan", 14),
        ("bright_white", 15),
    ] {
        assert_eq!(Color::parse(name), Some(Color::Ansi(code)), "name={name}");
    }
    assert_eq!(Color::parse("not-a-color"), None);
    let mut ansi16_out = String::new();
    for c in [
        Color::Ansi(5),
        Color::Ansi(235),
        Color::Ansi(245),
        Color::Ansi(252),
        Color::Ansi(196),
        Color::Rgb(200, 10, 10),
    ] {
        let line = Line::new(vec![Span::styled("x", Style::new().fg(c))]);
        ansi16_out.push_str(&line_to_ansi(&line, 1, ColorMode::Ansi16));
    }
    assert!(!ansi16_out.is_empty());

    // link_id deduplicates URLs and link_url(0) returns None.
    assert_eq!(super::model::link_url(0), None);
    let id1 = link_id("https://example.com/a");
    let id2 = link_id("https://example.com/a");
    assert_eq!(id1, id2);
    assert_eq!(super::model::link_url(id1).as_deref(), Some("https://example.com/a"));
}
