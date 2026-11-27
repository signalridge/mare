use super::*;

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
}

#[test]
fn detection_follows_env_then_probe() {
    assert_eq!(detect(&env(&[("KITTY_WINDOW_ID", "1")]), None, &[]), Protocol::Kitty);
    // tmux / Zellij off even inside kitty; herdr/rex and TERM_PROGRAM win over leaked iTerm variables
    assert_eq!(detect(&env(&[("KITTY_WINDOW_ID", "1"), ("TMUX", "x")]), None, &[]), Protocol::None);
    assert_eq!(detect(&env(&[("ZELLIJ", "0")]), None, &[4]), Protocol::None);
    assert_eq!(detect(&env(&[("TERM_PROGRAM", "herdr"), ("ITERM_SESSION_ID", "w")]), None, &[]), Protocol::Kitty);
    assert_eq!(detect(&env(&[("TERM_PROGRAM", "rex"), ("ITERM_SESSION_ID", "w")]), None, &[]), Protocol::Kitty);
    assert_eq!(detect(&env(&[("TERM_PROGRAM", "kitty"), ("ITERM_SESSION_ID", "w")]), None, &[]), Protocol::Kitty);
    assert_eq!(detect(&env(&[("TERM_PROGRAM", "iTerm.app")]), None, &[]), Protocol::Iterm2);
    // unknown terminal: XTVERSION, then DA1 attribute 4 (sixel)
    assert_eq!(detect(&env(&[]), Some("WezTerm 20240203"), &[]), Protocol::Kitty);
    assert_eq!(detect(&env(&[]), Some("foot(1.16)"), &[62, 4, 22]), Protocol::Sixel);
    assert_eq!(detect(&env(&[]), None, &[62, 22]), Protocol::None);
}

#[test]
fn fit_keeps_aspect_and_limits() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_cell_px(9, 18);
    // 90×90 px at 9×18 cells: 10 cols × 5 rows
    assert_eq!(fit(90, 90, 80, 20), (10, 5));
    // wide image limited by columns, tall image by rows
    assert_eq!(fit(9000, 900, 40, 20).0, 40);
    assert_eq!(fit(90, 9000, 40, 10).1, 10);
}

fn tiny() -> Vec<u8> {
    let mut img = image::RgbaImage::new(4, 4);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([(x * 60) as u8, (y * 60) as u8, 128, 255]);
    }
    png_bytes(&DynamicImage::ImageRgba8(img)).unwrap()
}

#[test]
fn kitty_chunks_and_ids() {
    let png = tiny();
    let one = encode_kitty(&png, 2, 1, 7);
    assert_eq!(one.len(), 1);
    assert!(one[0].starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=2,r=1,i=7,p=1;") && one[0].ends_with("\x1b\\"));
    let big = vec![0u8; 10_000];
    let chunks = encode_kitty(&big, 3, 2, 9);
    assert!(chunks.len() > 1);
    assert!(chunks[0].contains(",m=1;") && chunks.last().unwrap().starts_with("\x1b_Gm=0;"));
    assert_eq!(image_id(&png), image_id(&png));
}

#[test]
fn placeholder_without_protocol_and_rows_with_one() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.png");
    std::fs::write(&p, tiny()).unwrap();
    let src = Source::Path(p.to_string_lossy().to_string());
    set_protocol(Protocol::None);
    let ph = render(&src, 10, 40, 40, &t);
    assert_eq!(ph.len(), 1);
    assert_eq!(ph[0].plain(), "[image: t.png 4×4]");
    let bytes_ph = render(&Source::Bytes(tiny().into()), 10, 40, 40, &t);
    assert_eq!(bytes_ph.len(), 1);
    assert!(
        bytes_ph[0].plain().starts_with("[image: ") && bytes_ph[0].plain().ends_with(" 4×4]"),
        "bytes placeholder should include dimensions: {:?}",
        bytes_ph[0].plain()
    );
    set_cell_px(2, 2); // a 4×4 image → 2×2 cells
    set_protocol(Protocol::Kitty);
    let lines = {
        let _b = blocking();
        render(&src, 10, 40, 40, &t)
    };
    assert_eq!(lines.len(), 2);
    let raw = lines[1].raw.as_deref().unwrap();
    assert!(raw.starts_with("\r\x1b[1A\x1b_G") && raw.ends_with("\x1b\\\x1b[1B"), "{raw:?}");
    assert!(super::super::out::image_allowed(&raw.as_bytes()[5..raw.len() - 4]));
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

/// Byte snapshots of what each protocol writes for a fixed 4×4 picture in 2×2 cells: cursor
/// moves, the sequence itself, and the move back.
#[test]
fn protocol_bytes_snapshots() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_cell_px(2, 2);
    let src = Source::Bytes(tiny().into());
    for proto in [Protocol::Kitty, Protocol::Iterm2, Protocol::Sixel] {
        set_protocol(proto);
        let lines = {
            let _b = blocking();
            render(&src, 10, 40, 40, &t)
        };
        assert_eq!(lines.len(), 2, "{proto:?}: 2 rows reserved");
        assert!(lines[0].raw.is_none() && lines[0].plain().is_empty());
        let raw = lines[1].raw.as_deref().unwrap();
        insta::assert_snapshot!(format!("bytes_{}", proto.name()), format!("{raw:?}"));
    }
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

#[test]
fn kitty_ids_are_read_back_from_placements_only() {
    let png = tiny();
    let seqs = encode_kitty(&png, 2, 1, 42).concat();
    assert_eq!(kitty_ids(&format!("\r\x1b[1A{seqs}\x1b[1B")).collect::<Vec<_>>(), [42]);
    assert_eq!(kitty_ids(&kitty_delete(42)).count(), 0);
    let chunked = encode_kitty(&vec![0u8; 10_000], 3, 2, 9).concat();
    assert_eq!(kitty_ids(&chunked).collect::<Vec<_>>(), [9]);
}

/// Wait (bounded) until the queue has prepared `src` at this size: draw frames the way the UI
/// loop does after [`ready`] until the placeholder is gone.
fn render_when_ready(src: &Source, max_rows: usize, max_cols: usize) -> Vec<Line> {
    let t = Theme::default();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let before = waited();
        let lines = render(src, max_rows, max_cols, max_cols, &t);
        if waited() == before {
            return lines;
        }
        assert!(std::time::Instant::now() < end, "never prepared");
        std::thread::yield_now();
    }
}

// ---- a controllable worker: pictures tagged `gated` wait in `prepare` until the gate opens

static GATE: Mutex<bool> = Mutex::new(false);
static GATE_CV: std::sync::Condvar = std::sync::Condvar::new();

/// Called by `prepare` after decoding (tests only).
pub(super) fn gate(src: &Source) {
    if matches!(src, Source::Bytes(b) if b.ends_with(b"gated"))
        || matches!(src, Source::Path(p) if p.ends_with("-gated.png"))
    {
        let g = GATE.lock().unwrap_or_else(|e| e.into_inner());
        drop(GATE_CV.wait_while(g, |closed| *closed).unwrap_or_else(|e| e.into_inner()));
    }
}

/// Closes the gate; dropping it opens it again.
struct Closed;
fn close_gate() -> Closed {
    *GATE.lock().unwrap() = true;
    Closed
}
impl Drop for Closed {
    fn drop(&mut self) {
        *GATE.lock().unwrap_or_else(|e| e.into_inner()) = false;
        GATE_CV.notify_all();
    }
}

/// A `w`×`h` PNG of its own (`tag` follows IEND, so no other test has it cached).
fn pic(w: u32, h: u32, tag: &str) -> Source {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x as u8).wrapping_mul(9), (y as u8).wrapping_mul(7), 99, 255])
    });
    let mut png = png_bytes(&DynamicImage::ImageRgba8(img)).unwrap();
    png.extend_from_slice(tag.as_bytes());
    Source::Bytes(png.into())
}

/// While a picture is being prepared its final rows are reserved, blank (from the header's
/// size, before any decode); the prepared picture takes exactly those rows, so nothing on
/// screen moves when it arrives.
#[test]
fn the_reserved_rows_are_the_final_rows() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_protocol(Protocol::Kitty);
    let cases = [(4, 4, (2, 2), 10, 40), (90, 30, (9, 18), 20, 8), (40, 200, (3, 7), 6, 30), (500, 77, (8, 16), 9, 12)];
    for (n, (w, h, (cw, ch), max_rows, max_cols)) in cases.into_iter().enumerate() {
        set_cell_px(cw, ch);
        let src = pic(w, h, &format!("reserve-{n}-gated"));
        let reserved = {
            let _closed = close_gate();
            let before = waited();
            let r = render(&src, max_rows, max_cols, max_cols, &t);
            assert_eq!(waited(), before + 1, "case {n}: still being prepared");
            r
        };
        assert!(reserved.iter().all(|l| l.raw.is_none() && l.plain().is_empty()), "case {n}: blank");
        let fin = render_when_ready(&src, max_rows, max_cols);
        assert_eq!(reserved.len(), fin.len(), "case {n}: {w}×{h} at {cw}×{ch}");
        assert!(fin.last().unwrap().raw.is_some());
    }
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

/// One text line instead of reserved rows when the header can't be read, the room is too
/// narrow, or there's no protocol — none of them waits for anything.
#[test]
fn unreadable_narrow_or_no_protocol_is_one_text_line() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_cell_px(2, 2);
    set_protocol(Protocol::Kitty);
    let before = unsettled();
    let garbage = Source::Bytes(b"no header here".to_vec().into());
    let l = render(&garbage, 10, 40, 40, &t);
    assert_eq!((l.len(), l[0].plain().as_str()), (1, "[image: 14 bytes]"));
    let l = render(&pic(40, 40, "narrow"), 10, MIN_COLS - 1, 40, &t);
    assert!(l.len() == 1 && l[0].plain().starts_with("[image: "));
    set_protocol(Protocol::None);
    let l = render(&pic(40, 40, "none"), 10, 40, 40, &t);
    assert!(l.len() == 1 && l[0].plain().starts_with("[image: "));
    assert_eq!(unsettled(), before);
    set_cell_px(9, 18);
}

/// The draw path doesn't decode: once the queue has stored a picture, the next
/// draw has its rows; the same picture at a new cell size is queued again, never the old
/// encoding; the scrollback rebuild waits in place.
#[test]
fn a_picture_is_requeued_at_a_new_cell_size() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_cell_px(2, 2);
    set_protocol(Protocol::Kitty);
    let src = pic(4, 4, "resize");
    let lines = render_when_ready(&src, 10, 40);
    assert_eq!(lines.len(), 2, "4×4 px at 2×2 cells");
    assert!(lines[1].raw.as_deref().unwrap().contains("c=2,r=2,"));
    // a cell-size change: a new key, prepared again at the new size
    set_cell_px(4, 4);
    let lines = render_when_ready(&src, 10, 40);
    assert_eq!(lines.len(), 1, "4×4 px at 4×4 cells");
    assert!(lines[0].raw.as_deref().unwrap().contains("c=1,r=1,"));
    set_cell_px(1, 1);
    let before = waited();
    let lines = {
        let _b = blocking();
        render(&src, 10, 40, 40, &t)
    };
    assert_eq!((waited(), lines.len()), (before, 4));
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

/// Committing: a block waits for its pictures up to the limit, then is drawn
/// with text for the ones not ready and committed; a picture that fails is text at once.
#[test]
fn a_failed_or_late_picture_is_text_and_the_block_commits() {
    use super::super::image_hold::{Hold, set_wait_limit};
    use std::time::{Duration, Instant};
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_cell_px(2, 2);
    set_protocol(Protocol::Kitty);
    // late: the worker is held; under the limit the block stays live with its rows reserved
    let late = pic(4, 4, "late-gated");
    let mut hold = Hold::default();
    let closed = close_gate();
    set_wait_limit(Duration::from_secs(3600));
    let t0 = Instant::now();
    let (l, done) = hold.draw(1, t0, || render(&late, 10, 40, 40, &t));
    assert!(!done && l.len() == 2 && l.iter().all(|l| l.plain().is_empty()));
    let (_, done) = hold.draw(1, t0 + Duration::from_secs(10), || render(&late, 10, 40, 40, &t));
    assert!(!done, "within the limit");
    // past the limit: text, and it may commit
    set_wait_limit(Duration::from_millis(50));
    let (l, done) = hold.draw(1, t0 + Duration::from_secs(10), || render(&late, 10, 40, 40, &t));
    assert!(done, "past the limit");
    assert_eq!(l.len(), 1);
    assert!(l[0].plain().starts_with("[image: "));
    // the wait is per block: another block starts its own
    let (_, done) = hold.draw(2, t0 + Duration::from_secs(10), || render(&late, 10, 40, 40, &t));
    assert!(!done);
    drop(closed);
    // it was still prepared in the background
    assert_eq!(render_when_ready(&late, 10, 40).len(), 2);
    // failed: the header reads, the pixels don't decode — text with the reason, no wait
    let broken = {
        let Source::Bytes(b) = pic(4, 4, "") else { unreachable!() };
        let mut b = b.to_vec();
        let n = b.len();
        for x in &mut b[n - 20..n - 12] {
            *x ^= 0xff;
        }
        Source::Bytes(b.into())
    };
    set_wait_limit(Duration::from_secs(3600));
    let end = Instant::now() + Duration::from_secs(60);
    let l = loop {
        let (l, done) = hold.draw(3, Instant::now(), || render(&broken, 10, 40, 40, &t));
        if done {
            break l;
        }
        assert!(Instant::now() < end, "never failed");
        std::thread::yield_now();
    };
    assert_eq!(l.len(), 1);
    assert!(l[0].plain().starts_with("[image: ") && l[0].plain().contains(" — "), "{:?}", l[0].plain());
    set_wait_limit(Duration::from_secs(5));
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

/// More pictures in one block than the encode cache holds keep evicting each other, so the
/// block never has them all at once: the limit still lets it commit.
#[test]
fn a_block_with_more_pictures_than_the_cache_commits() {
    use super::super::image_hold::{Hold, set_wait_limit};
    use std::time::{Duration, Instant};
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_cell_px(2, 2);
    set_protocol(Protocol::Kitty);
    let srcs: Vec<Source> = (0..CACHE_MAX.get() + 8).map(|n| pic(4, 4, &format!("many-{n}"))).collect();
    set_wait_limit(Duration::from_millis(200));
    let mut hold = Hold::default();
    let end = Instant::now() + Duration::from_secs(60);
    let lines = loop {
        let (l, done) =
            hold.draw(7, Instant::now(), || srcs.iter().flat_map(|s| render(s, 10, 40, 40, &t)).collect::<Vec<_>>());
        if done {
            break l;
        }
        assert!(Instant::now() < end, "never committed");
        std::thread::yield_now();
    };
    // every picture is there: drawn, or one text line
    let text = lines.iter().filter(|l| l.plain().starts_with("[image: ")).count();
    let drawn = lines.iter().filter(|l| l.raw.is_some()).count();
    assert_eq!(text + drawn, srcs.len());
    set_wait_limit(Duration::from_secs(5));
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

/// Many pictures drawn in one frame are all queued, none dropped, and all of them arrive.
#[test]
fn a_burst_of_pictures_all_arrive() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = Theme::default();
    set_cell_px(2, 2);
    set_protocol(Protocol::Kitty);
    let srcs: Vec<Source> = (0..8).map(|n| pic(4, 4, &format!("burst-{n}"))).collect();
    for s in &srcs {
        render(s, 10, 40, 40, &t);
    }
    for s in &srcs {
        assert_eq!(render_when_ready(s, 10, 40).len(), 2);
    }
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

#[test]
fn streaming_markdown_does_not_cache_a_pending_picture_or_failure() {
    use crate::markdown::StreamingMarkdown;
    use std::time::{Duration, Instant};

    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("mare-stream-image-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let theme = Theme::default();
    set_protocol(Protocol::Kitty);
    set_cell_px(2, 2);
    for broken in [false, true] {
        let mut bytes = tiny();
        if broken {
            let n = bytes.len();
            for byte in &mut bytes[n - 20..n - 12] {
                *byte ^= 0xff;
            }
        }
        let path = dir.join(if broken { "bad-gated.png" } else { "ok-gated.png" });
        std::fs::write(&path, bytes).unwrap();
        let mut md = StreamingMarkdown::new();
        md.set_text(&format!("![picture]({})", path.display().to_string().replace('\\', "/")));
        let initial_generation = generation();
        let closed = close_gate();
        for _ in 0..2 {
            let before = unsettled();
            let lines = md.render(40, &theme);
            assert_eq!(unsettled(), before + 1, "every pending frame must render again");
            assert!(lines.iter().all(|line| line.raw.is_none()));
        }
        drop(closed);
        let deadline = Instant::now() + Duration::from_secs(60);
        let ready = loop {
            let before = unsettled();
            let lines = md.render(40, &theme);
            if unsettled() == before {
                break lines;
            }
            assert!(Instant::now() < deadline, "picture did not complete");
            std::thread::yield_now();
        };
        assert_eq!(generation(), initial_generation, "job completion does not bump image generation");
        if broken {
            assert!(ready.iter().any(|line| line.plain().contains("[image:") && line.plain().contains(" — ")));
        } else {
            assert!(ready.iter().any(|line| line.raw.is_some()), "prepared image placement is drawn");
        }
        assert_eq!(md.render(40, &theme), ready, "completed result stays stable");
    }
    std::fs::remove_dir_all(dir).unwrap();
    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}

#[test]
fn identical_pixels_keep_independent_image_occurrences() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _blocking = blocking();
    let prior = protocol();
    set_protocol(Protocol::Kitty);
    let source = Source::Bytes(tiny().into());
    let theme = Theme::default();
    let a = render_with_id(&source, 8, 40, 80, &theme, 0x01000001);
    let b = render_with_id(&source, 8, 40, 80, &theme, 0x01000002);
    set_protocol(prior);
    let a = a.iter().find_map(|line| line.raw.as_deref()).unwrap();
    let b = b.iter().find_map(|line| line.raw.as_deref()).unwrap();
    assert_eq!(kitty_ids(a).collect::<Vec<_>>(), [0x01000001]);
    assert_eq!(kitty_ids(b).collect::<Vec<_>>(), [0x01000002]);
    assert_eq!(a.split_once(';').unwrap().1, b.split_once(';').unwrap().1);
}

#[test]
fn resource_layout_and_large_document_window_do_not_load_offscreen_images() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_protocol(Protocol::Kitty);
    set_cell_px(8, 16);
    let (resolver, mut requests) = mare_shared::image_resource::Resolver::register();
    let reference = mare_shared::image_resource::Reference::new(resolver.id(), "layout-only".into(), 32, 16).unwrap();
    {
        let _measure = measure_only();
        assert_eq!(render(&Source::Resource(reference), 16, 80, 80, &Theme::default()).len(), 1);
    }
    assert!(requests.try_recv().is_err(), "layout measurement queued storage IO");
    let mut nodes = vec![];
    for index in 0..1000 {
        nodes.push(mare_shared::ui::Node::Image {
            image_id: 10000 + index,
            resource: Some(
                mare_shared::image_resource::Reference::new(resolver.id(), format!("source-{index}"), 32, 16).unwrap(),
            ),
            fallback: None,
            path: None,
            bytes: None,
            max_rows: 16,
            max_cols: 80,
        });
        nodes.push(mare_shared::ui::Node::Gap(1));
    }
    let _window = window(1998, 2000);
    let lines = crate::render::render_node(&mare_shared::ui::Node::Lines(nodes), 80, &Theme::default(), 0);
    assert_eq!(lines.len(), 2000);
    assert!(lines.iter().all(|line| line.raw.is_none()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let _enter = rt.enter();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let request = rt
        .block_on(async { tokio::time::timeout_at(deadline, requests.recv()).await })
        .expect("visible image was not queued")
        .expect("resolver request channel closed");
    assert_eq!(request.reference.key.as_ref(), "source-999");
    assert!(requests.try_recv().is_err(), "offscreen images were queued");
    let source = Source::Resource(request.reference.clone());
    resolver.close();
    request.complete(Err("test completed".into()));
    // the worker stores the result before `ready_notify`, whose permit is kept until awaited
    while cached(&(source.clone(), None, 80, 16, Protocol::Kitty, (8, 16))).is_none() {
        rt.block_on(async { tokio::time::timeout_at(deadline, super::ready()).await })
            .expect("image worker did not settle");
    }
    set_protocol(Protocol::None);
}

#[test]
fn sixel_recomputes_rows_and_restores_cursor_safely() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_protocol(Protocol::Sixel);
    set_cell_px(8, 16);
    let mut img = image::RgbaImage::new(200, 200);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba([(x ^ y) as u8, (x * 3) as u8, (y * 5) as u8, 255]);
    }
    let png = png_bytes(&DynamicImage::ImageRgba8(img)).unwrap();
    let enc = prepare(&(Source::Bytes(png.into()), None, 10, 8, Protocol::Sixel, (8, 16))).unwrap();
    assert!(enc.rows >= 1 && enc.rows <= 8);
    // Single-row render without trailing graphics newline ends with `\r`, not `\r\x1b[1A`.
    let out =
        finish_encoded(&Encoded { rows: 1, seqs: vec!["\x1bP9;1q#0\x1b\\".into()].into() }, Protocol::Sixel, None);
    let raw = out.last().and_then(|l| l.raw.as_deref()).unwrap();
    assert!(raw.ends_with('\r') && !raw.ends_with("\r\x1b[1A"), "{raw:?}");
    let out_nl =
        finish_encoded(&Encoded { rows: 1, seqs: vec!["\x1bP9;1q#0-\x1b\\".into()].into() }, Protocol::Sixel, None);
    let raw_nl = out_nl.last().and_then(|l| l.raw.as_deref()).unwrap();
    assert!(raw_nl.ends_with("\r\x1b[1A"), "{raw_nl:?}");
    set_protocol(Protocol::None);
}

#[test]
fn path_image_cache_invalidates_on_overwrite_and_recovers_from_partial_write() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let pic = dir.path().join("chart.png");
    let src = Source::Path(pic.to_string_lossy().into_owned());
    let theme = Theme::default();
    set_protocol(Protocol::Kitty);
    set_cell_px(2, 2);

    // 1. Valid PNG header with corrupt pixel payload: header succeeds (4x4), full decode fails (Err).
    let mut corrupt_png =
        png_bytes(&DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 200, 30, 255]))))
            .unwrap();
    let n = corrupt_png.len();
    for b in &mut corrupt_png[n - 20..n - 12] {
        *b ^= 0xff;
    }
    std::fs::write(&pic, &corrupt_png).unwrap();
    assert_eq!(super::super::image_header::dimensions(&src), Some((4, 4)));
    {
        let _blocking = blocking();
        let lines = render(&src, 16, 40, 40, &theme);
        assert!(lines.iter().all(|l| l.raw.is_none()), "corrupt partial file must render placeholder, not graphics");
    }

    // 2. Overwriting the same path with a valid 4x4 PNG must recover (not stay stuck on cached Err).
    let png_4x4 =
        png_bytes(&DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 200, 30, 255]))))
            .unwrap();
    std::fs::write(&pic, &png_4x4).unwrap();
    assert_eq!(super::super::image_header::dimensions(&src), Some((4, 4)));
    let raw_4x4 = {
        let _blocking = blocking();
        let lines = render(&src, 16, 40, 40, &theme);
        lines.iter().find_map(|l| l.raw.as_deref().map(str::to_string)).expect("valid 4x4 PNG must render Kitty image")
    };
    assert!(raw_4x4.contains("c=2,r=2,"), "expected 2x2 cells for 4x4 at 2x2 cell_px: {raw_4x4:?}");

    // 3. Overwriting the same path with a 4x16 PNG must update both header dimensions and encoded payload.
    let png_4x16 =
        png_bytes(&DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 16, image::Rgba([220, 40, 180, 255]))))
            .unwrap();
    std::fs::write(&pic, &png_4x16).unwrap();
    assert_eq!(super::super::image_header::dimensions(&src), Some((4, 16)));
    let raw_4x16 = {
        let _blocking = blocking();
        let lines = render(&src, 16, 40, 40, &theme);
        assert_eq!(lines.len(), 8, "4x16 at 2x2 cell_px must reserve 8 rows");
        lines
            .iter()
            .find_map(|l| l.raw.as_deref().map(str::to_string))
            .expect("overwritten 4x16 PNG must render updated Kitty image")
    };
    assert!(raw_4x16.contains("c=2,r=8,"), "expected 2x8 cells for 4x16 at 2x2 cell_px: {raw_4x16:?}");
    assert_ne!(raw_4x16, raw_4x4, "overwritten image must produce a new payload");

    set_protocol(Protocol::None);
    set_cell_px(9, 18);
}
