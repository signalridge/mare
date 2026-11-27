//! Pictures in answers and dropped image paths.

use super::*;

/// dropped image files (pasted as escaped paths, several at once) become `[Image #N]`
/// attachments that go out with the prompt; other dropped paths stay text.
#[tokio::test]
async fn dropped_image_paths_become_image_attachments() {
    // 1×1 transparent PNG
    const PNG: [u8; 67] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
        0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d,
        0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    local(async {
        let mut t = Term::new(json!([{ "text": "seen" }]), 80, 24).await;
        let cwd = t._env.cwd.clone();
        std::fs::write(cwd.join("my shot.png"), PNG).unwrap();
        std::fs::write(cwd.join("b.png"), PNG).unwrap();
        let a = cwd.join("my shot.png").display().to_string().replace(' ', "\\ ");
        let b = cwd.join("b.png").display().to_string();
        t.vt.paste(&format!("{a} '{b}' "));
        assert_eq!(t.vt.editor_text(), "[Image #1] [Image #2]");
        t.vt.paste(" /no/such/file.png");
        assert_eq!(t.vt.editor_text(), "[Image #1] [Image #2] /no/such/file.png", "not a file: text");
        tokio::time::sleep(Duration::from_millis(200)).await; // past the stray-Enter paste guard
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        let n: i64 = eval(
            &t.vt.host,
            "local n = 0; for _, m in ipairs(AG:list_messages().data) do for _, b in ipairs(type(m.content) == 'table' and m.content or {}) do if b.type == 'image' then n = n + 1 end end end; return n",
        )
        .await;
        assert_eq!(n, 2, "both images went out with the prompt");
    })
    .await
}

/// A 4×4 PNG (what `mare_tui::image`'s byte snapshots use).
const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 4, 8, 6, 0, 0, 0, 169, 241, 158,
    126, 0, 0, 0, 79, 73, 68, 65, 84, 120, 1, 1, 68, 0, 187, 255, 0, 0, 0, 128, 255, 60, 0, 128, 255, 120, 0, 128, 255,
    180, 0, 128, 255, 0, 0, 60, 128, 255, 60, 60, 128, 255, 120, 60, 128, 255, 180, 60, 128, 255, 0, 0, 120, 128, 255,
    60, 120, 128, 255, 120, 120, 128, 255, 180, 120, 128, 255, 0, 0, 180, 128, 255, 60, 180, 128, 255, 120, 180, 128,
    255, 180, 180, 128, 255, 57, 200, 35, 49, 117, 219, 213, 104, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// Rows of the screen between the line `from` and the line `to` (exclusive).
fn rows_between(screen: &str, from: &str, to: &str) -> Vec<String> {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let a = lines.iter().position(|l| l.ends_with(from)).unwrap_or_else(|| panic!("no {from:?}:\n{screen}"));
    let b = lines.iter().position(|l| l.ends_with(to)).unwrap_or_else(|| panic!("no {to:?}:\n{screen}"));
    lines[a + 1..b].iter().map(|l| l.trim().to_string()).collect()
}

/// a Markdown image in an answer takes its rows in the layout (Kitty: the
/// sequence rides on the last row, the screen text stays blank), and without a protocol it is
/// one dim text line. The picture is prepared on the preprocessing queue, not in the
/// frame: it still arrives, and the finished answer isn't committed with its placeholder.
#[tokio::test]
async fn an_answer_image_takes_its_rows_or_shows_a_placeholder() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let pic = dir.path().join("pic.png");
        std::fs::write(&pic, TINY_PNG).unwrap();
        // forward slashes: in Markdown `\.` is an escape, so a raw Windows path would lose its
        // backslashes (Windows accepts `/` in paths)
        let md = format!("before\n\n![pic]({})\n\nafter", pic.display().to_string().replace('\\', "/"));

        // no protocol: a one-line placeholder in its place
        let mut t = Term::new(json!([{ "text": md.clone() }]), 60, 20).await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        t.submit("img").await;
        t.idle().await;
        assert_eq!(rows_between(&t.screen(), "before", "after"), ["", "[image: pic.png 4×4]", ""]);
        drop(t);

        // Kitty with 2×2-pixel cells: the 4×4 picture takes 2 rows; the sequence is written
        let mut t = Term::new(json!([{ "text": md }]), 60, 20).await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(2, 2);
        t.vt.output();
        t.ended = t.vt.turns_ended();
        t.vt.type_str("img");
        t.key(KeyCode::Enter, Modifiers::NONE);
        // the picture is prepared off the UI thread: until it's ready its 2 rows are
        // reserved, so every frame has the same layout — nothing jumps when it arrives
        let mut bytes = Vec::new();
        let end = Instant::now() + WAIT;
        let placed = "\x1b_Ga=T,f=100,q=2,C=1,c=2,r=2,";
        loop {
            t.vt.frame().await;
            let out = t.vt.output();
            bytes.extend_from_slice(&out);
            t.parser.process(&out);
            let screen = t.screen();
            if screen.lines().any(|l| l.trim_end().ends_with("before"))
                && screen.lines().any(|l| l.trim_end().ends_with("after"))
            {
                assert_eq!(rows_between(&screen, "before", "after"), ["", "", "", ""], "the same rows in every frame");
            }
            let ended = t.vt.turns_ended() > t.ended && !t.vt.busy();
            if ended && screen.contains("after") && String::from_utf8_lossy(&bytes).contains(placed) {
                break;
            }
            assert!(Instant::now() < end, "turn never ended or the picture never arrived; screen:\n{screen}");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let screen = t.screen();
        assert_eq!(rows_between(&screen, "before", "after"), ["", "", "", ""], "blank, 2 image rows, blank");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\x1b_Ga=T,f=100,q=2,C=1,c=2,r=2,"), "kitty placement written");
        assert!(!screen.contains("[image:"), "no text fallback:\n{screen}");
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);
    })
    .await
}

/// a picture that takes a while to prepare has its final rows reserved from
/// the first frame (read from the file header): the text around it never moves, and it's drawn
/// in place when ready.
#[tokio::test]
async fn a_slow_picture_keeps_its_rows_while_it_is_prepared() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let pic = dir.path().join("big.png");
        // noise doesn't compress: decoding and scaling it takes a while
        let mut x: u32 = 0x9e37_79b9;
        let img = image::RgbImage::from_fn(1200, 1200, |_, _| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let [a, b, c, _] = x.to_le_bytes();
            image::Rgb([a, b, c])
        });
        img.save(&pic).unwrap();
        let md = format!("before\n\n![big]({})\n\nafter", pic.display().to_string().replace('\\', "/"));
        let mut t = Term::new(json!([{ "text": md }]), 60, 45).await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(8, 16);
        t.vt.output();
        t.ended = t.vt.turns_ended();
        t.vt.type_str("img");
        t.key(KeyCode::Enter, Modifiers::NONE);
        let mut bytes = Vec::new();
        let (mut rows, mut reserved_seen) = (None::<Vec<String>>, false);
        let end = Instant::now() + WAIT;
        loop {
            // the startup notice card would cover the answer
            t.vt.dismiss_notices();
            t.vt.frame().await;
            let out = t.vt.output();
            bytes.extend_from_slice(&out);
            t.parser.process(&out);
            let screen = t.screen();
            assert!(!screen.contains("[image:"), "no text placeholder while waiting:\n{screen}");
            let placed = String::from_utf8_lossy(&bytes).contains("\x1b_Ga=T,f=100");
            let has = |w: &str| screen.lines().any(|l| l.trim_end().ends_with(w));
            if has("before") && has("after") {
                let now = rows_between(&screen, "before", "after");
                assert!(now.iter().all(String::is_empty), "{screen}");
                match &rows {
                    Some(r) => assert_eq!(r, &now, "the rows never change:\n{screen}"),
                    None => rows = Some(now),
                }
                reserved_seen |= !placed;
            }
            if placed && t.vt.turns_ended() > t.ended && !t.vt.busy() && rows.is_some() {
                break;
            }
            assert!(Instant::now() < end, "the picture never arrived; screen:\n{screen}");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(reserved_seen, "a frame drew the reserved rows before the picture was ready");
        // 16 rows at most (Markdown images), blank lines around it
        assert_eq!(rows.unwrap().len(), 16 + 2);
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);
    })
    .await
}

/// an answer with more pictures than the encode cache holds (they keep evicting
/// each other, so they're never all ready at once) still moves to the scrollback once it has
/// waited the limit — the pictures not ready then are text lines.
#[tokio::test]
async fn an_answer_with_more_pictures_than_the_cache_is_committed() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let mut md = String::from("before\n\n");
        for n in 0..40 {
            let pic = dir.path().join(format!("p{n}.png"));
            // a picture of its own: the same PNG with a different tail after IEND
            let mut png = TINY_PNG.to_vec();
            png.extend_from_slice(format!("pic-{n}").as_bytes());
            std::fs::write(&pic, png).unwrap();
            md += &["![p](", &pic.display().to_string().replace('\\', "/"), ")\n\n"].concat();
        }
        md.push_str("after");
        let mut t = Term::new(json!([{ "text": md }]), 60, 20).await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(2, 2);
        mare_tui::image_hold::set_wait_limit(Duration::from_millis(300));
        t.submit("img").await;
        t.until_vt("the answer reaches the scrollback", |t| {
            t.vt.turns_ended() > t.ended && !t.vt.busy() && t.vt.live_blocks() == 0
        })
        .await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);
    })
    .await
}

/// Queued attachments survive cancelling a queue edit and interrupting the active turn, then
/// travel with the restored draft exactly once, in their original order.
#[tokio::test]
async fn interrupted_queued_images_are_restored_with_the_draft() {
    local(async {
        let mut t = Term::new(json!([{ "text": "z".repeat(4000), "delay_ms": 20 }, { "text": "seen" }]), 90, 30).await;
        let cwd = t._env.cwd.clone();
        std::fs::write(cwd.join("a.png"), TINY_PNG).unwrap();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255])).save(cwd.join("b.png")).unwrap();
        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        expose(&t.vt.host);
        exec(&t.vt.host, "Q_SNAP = {}; Q_TX, Q_RX = mare.task.channel(); C.cordis:get('policy'):voter('session/commit-ready', function(batch) if batch and batch.inbox then Q_SNAP = batch.inbox end end); C.cordis:on('queue/changed', function() Q_TX:send(true) end)").await;
        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        for (idx, file) in ["a.png", "b.png"].into_iter().enumerate() {
            t.vt.paste(&cwd.join(file).display().to_string());
            tokio::time::sleep(Duration::from_millis(200)).await;
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.step().await;
            let expected_len = idx + 1;
            let committed: bool = eval(
                &t.vt.host,
                &format!(
                    "return mare.task.timeout(10000, function() while #AG:list_queue().data < {expected_len} do Q_RX:recv() end end)"
                ),
            )
            .await;
            assert!(committed, "queued image #{expected_len} did not commit");
        }
        let encoded: String =
            eval(&t.vt.host, "assert(#AG:list_queue().data == 2); return mare.json.encode(Q_SNAP.follow_up)").await;
        let queued: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(queued.as_array().unwrap().len(), 2);
        let mut expected: Vec<Value> = queued.as_array().unwrap().iter().map(|e| e["images"][0].clone()).collect();
        assert!(expected.iter().all(|b| b["type"] == "image" && b["mime"] == "image/png"));
        assert_ne!(expected[0]["data"], expected[1]["data"]);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("queue edit loaded into editor", |t| t.vt.editor_text().contains("[Image #")).await;
        t.vt.type_str(" cancelled edit");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("queue edit cancelled", |t| t.vt.editor_text().is_empty()).await;
        assert!(t.vt.busy());
        assert_eq!(t.vt.editor_text(), "");
        t.vt.paste(&cwd.join("a.png").display().to_string());
        expected.push(expected[0].clone());
        t.vt.type_str("my draft");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("the interrupted turn", |t| !t.vt.busy()).await;
        t.until_vt("attachments restored", |t| {
            t.vt.editor_text().contains("my draft") && t.vt.editor_text().contains("[Image #")
        })
        .await;
        assert!(!t.vt.editor_text().contains("cancelled edit"));
        tokio::time::sleep(Duration::from_millis(200)).await; // past the draft image paste guard
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"REFUSE_QUEUED = true; C.cordis:get("policy"):voter("request/ready", function()
            if REFUSE_QUEUED then return {decision="deny",reason="queued draft refused"} end
        end)"#,
        )
        .await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        t.until("the refused combined draft", |s| s.contains("queued draft refused")).await;
        assert!(t.vt.editor_text().contains("my draft"));
        assert!(t.vt.editor_text().contains("[Image #"));
        exec(&t.vt.host, "REFUSE_QUEUED = false").await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        let encoded: String = eval(&t.vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        let messages: Value = serde_json::from_str(&encoded).unwrap();
        let rows = messages.as_array().unwrap();
        assert_eq!(rows.iter().filter(|m| m["turn_aborted"] == true).count(), 1, "one model interrupt marker");
        let users: Vec<_> = rows.iter().filter(|m| m["role"] == "user" && m["notification"] != true).collect();
        assert_eq!(users.len(), 2, "only the initial and restored input: {messages}");
        let actual: Vec<Value> =
            users[1]["content"].as_array().unwrap().iter().filter(|b| b["type"] == "image").cloned().collect();
        assert_eq!(actual, expected, "MIME, data, order and exactly-once delivery");
    })
    .await
}

/// A denied request owns its full draft until it can be retried, including image bytes.
#[tokio::test]
async fn refused_image_submission_restores_text_and_attachment_for_retry() {
    local(async {
        let mut t = Term::new(json!([{ "text": "seen after retry" }]), 90, 30).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare._veto = true
            C.cordis:get("policy"):voter("request/ready", function()
                if mare._veto then return { decision = "deny", reason = "image veto" } end
            end)"#,
        )
        .await;
        let path = t._env.cwd.join("retry.png");
        std::fs::write(&path, TINY_PNG).unwrap();
        t.vt.type_str("[Image #99] inspect ");
        t.vt.paste(&path.display().to_string());
        assert_eq!(t.vt.editor_text(), "[Image #99] inspect [Image #1]");
        tokio::time::sleep(Duration::from_millis(200)).await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        // A second draft and image arrive before the first request settles.
        t.vt.type_str("new draft ");
        t.vt.paste(&path.display().to_string());
        t.idle().await;
        t.until("the image refusal", |s| s.contains("not sent: image veto")).await;
        assert_eq!(t.vt.editor_text(), "[Image #99] inspect [Image #1]\n\nnew draft [Image #2]");
        let messages: String = eval(&t.vt.host, "return mare.json.encode(A:list_messages().data)").await;
        assert!(!messages.contains("inspect"), "a refused draft must not enter conversation: {messages}");
        exec(&t.vt.host, "mare._veto = false").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        t.until("the retry answer", |s| s.contains("seen after retry")).await;
        assert_eq!(t.vt.editor_text(), "");
        let count: i64 = eval(
            &t.vt.host,
            r#"local n = 0
            for _, m in ipairs(A:list_messages().data) do
                for _, b in ipairs(type(m.content) == "table" and m.content or {}) do
                    if b.type == "image" then n = n + 1; assert(b.data and #b.data > 0) end
                end
            end
            return n"#,
        )
        .await;
        assert_eq!(count, 2, "both original and new image bytes survive rejection and retry");
    })
    .await
}

/// Enter, Ctrl+S and Alt+Enter all retain draft ownership through a real journal failure.
/// Explicit policy gates cover the async window while a second draft/image is being typed.
#[tokio::test]
async fn failed_queued_image_submission_restores_old_and_new_drafts_for_every_send_key() {
    local(async {
        for (code,modifiers) in [
            (KeyCode::Enter,Modifiers::NONE),
            (KeyCode::Char('s'),Modifiers::CONTROL),
            (KeyCode::Enter,Modifiers::ALT),
        ] {
            let mut t=Term::new(json!([{"text":"accepted retry"}]),100,32).await;
            expose(&t.vt.host);
            exec(&t.vt.host,r#"
                A:append({role='user',content={{type='text',text='session seed'}}})
                local initial_tx,initial_rx=mare.task.channel()
                local initial_release,initial_wait=mare.task.channel()
                local queue_tx,queue_rx=mare.task.channel()
                local queue_release,queue_wait=mare.task.channel()
                INITIAL_RX,INITIAL_RELEASE,QUEUE_RX,QUEUE_RELEASE=initial_rx,initial_release,queue_rx,queue_release
                local first=true
                C.cordis:get("policy"):voter('request/ready',function()
                    if first then first=false;initial_tx:send(true);initial_wait:recv() end
                end)
                local queue_first=true
                C.cordis:get("policy"):voter('session/commit-ready',function()
                    if queue_first then queue_first=false;queue_tx:send(true);queue_wait:recv() end
                end)
            "#).await;
            t.submit("held initial turn").await;
            exec(&t.vt.host,"INITIAL_RX:recv()").await;
            let path:String=eval(&t.vt.host,"return S:info().path").await;
            let image=t._env.cwd.join("draft.png");
            std::fs::write(&image,TINY_PNG).unwrap();
            t.vt.type_str("[Image #99] old draft ");
            t.vt.paste(&image.display().to_string());
            t.key(KeyCode::End,Modifiers::NONE); // clear the paste guard through a real navigation key
            t.key(code,modifiers);
            exec(&t.vt.host,"QUEUE_RX:recv()").await;
            t.vt.type_str("new draft ");
            t.vt.paste(&image.display().to_string());
            let backup=std::path::PathBuf::from(format!("{path}.saved"));
            std::fs::rename(&path,&backup).unwrap();
            std::fs::write(&path,b"replacement journal").unwrap();
            exec(&t.vt.host,"QUEUE_RELEASE:send(true)").await;
            t.until_vt("both draft owners restored",|t|t.vt.editor_text()=="[Image #99] old draft [Image #1]\n\nnew draft [Image #2]").await;
            let queued:i64=eval(&t.vt.host,"return #A:list_queue().data").await;
            assert_eq!(queued,0,"failed queue write must not create hidden accepted input");
            std::fs::remove_file(&path).unwrap();
            std::fs::rename(&backup,&path).unwrap();
            t.key(KeyCode::Escape,Modifiers::NONE);
            exec(&t.vt.host,"INITIAL_RELEASE:send(true)").await;
            t.until_vt("initial work cancelled",|t|!t.vt.busy()).await;
            t.ended=t.vt.turns_ended();
            t.key(KeyCode::End,Modifiers::NONE);
            t.key(KeyCode::Enter,Modifiers::NONE);
            t.idle().await;
            let count:i64=eval(&t.vt.host,"local n=0;for _,m in ipairs(A:list_messages().data) do for _,b in ipairs(m.content or {}) do if b.type=='image' then n=n+1 end end end;return n").await;
            assert_eq!(count,2,"the original and newer attachment both survive refusal and retry");
        }
    }).await
}

#[tokio::test]
async fn deleting_the_actual_restored_image_marker_does_not_bind_to_lookalike_prose() {
    local(async {
        let mut t=Term::new(json!([{"text":"text only after marker deletion"}]),100,30).await;
        expose(&t.vt.host);
        exec(&t.vt.host,r#"
            REFUSE_MARKER=true
            C.cordis:get("policy"):voter('request/ready',function() if REFUSE_MARKER then return {decision='deny',reason='marker refusal'} end end)
        "#).await;
        let image=t._env.cwd.join("marker.png");
        std::fs::write(&image,TINY_PNG).unwrap();
        t.vt.type_str("ordinary [Image #99] inspect ");
        t.vt.paste(&image.display().to_string());
        t.key(KeyCode::End,Modifiers::NONE);
        t.ended=t.vt.turns_ended();
        t.key(KeyCode::Enter,Modifiers::NONE);
        t.idle().await;
        assert_eq!(t.vt.editor_text(),"ordinary [Image #99] inspect [Image #1]");
        t.key(KeyCode::End,Modifiers::NONE);
        t.key(KeyCode::Backspace,Modifiers::NONE);
        assert_eq!(t.vt.editor_text(),"ordinary [Image #99] inspect ","the real attachment marker is one atomic editor element");
        exec(&t.vt.host,"REFUSE_MARKER=false").await;
        t.ended=t.vt.turns_ended();
        t.key(KeyCode::Enter,Modifiers::NONE);
        t.idle().await;
        let count:i64=eval(&t.vt.host,"local n=0;for _,m in ipairs(A:list_messages().data) do for _,b in ipairs(m.content or {}) do if b.type=='image' then n=n+1 end end end;return n").await;
        assert_eq!(count,0,"ordinary image-like prose must never inherit a deleted attachment");
    }).await
}

#[tokio::test]
async fn restored_image_uses_a_fresh_marker_when_the_editor_counter_is_still_zero() {
    local(async {
        let mut t=Term::new(json!([{"text":"text after removing restored image"}]),100,30).await;
        expose(&t.vt.host);
        t.vt.type_str("ordinary [Image #1] [Image #2]");
        exec(&t.vt.host,r#"
            mare.ui.set_editor('[Image #1]',{
                prepend=true,
                images={{type='image',mime='image/png',data='AQIDBA=='}},
                image_markers={{text='[Image #1]',offset=0}},
            })
        "#).await;
        t.until_vt("fresh restored marker",|t|t.vt.editor_text()=="[Image #3]\n\nordinary [Image #1] [Image #2]").await;
        // set_text leaves the cursor at the last row. Return to the restored attachment and
        // delete its one atomic element; both literal lookalikes remain ordinary prose.
        t.key(KeyCode::Up,Modifiers::NONE);
        t.key(KeyCode::Up,Modifiers::NONE);
        t.key(KeyCode::Home,Modifiers::NONE);
        t.key(KeyCode::Delete,Modifiers::NONE);
        assert_eq!(t.vt.editor_text(),"\n\nordinary [Image #1] [Image #2]");
        t.ended=t.vt.turns_ended();
        t.key(KeyCode::Enter,Modifiers::NONE);
        t.idle().await;
        let count:i64=eval(&t.vt.host,"local n=0;for _,m in ipairs(A:list_messages().data) do for _,b in ipairs(m.content or {}) do if b.type=='image' then n=n+1 end end end;return n").await;
        assert_eq!(count,0,"restored attachment identity must never bind either existing literal marker");
    }).await
}

#[tokio::test]
async fn deleting_real_image_element_after_inserting_same_literal_sends_no_image() {
    local(async {
        let mut t = Term::new(json!([{"text":"text only after same-literal insertion"}]), 100, 30).await;
        expose(&t.vt.host);
        let image = t._env.cwd.join("same-literal.png");
        std::fs::write(&image, TINY_PNG).unwrap();
        t.vt.paste(&image.display().to_string());
        assert_eq!(t.vt.editor_text(), "[Image #1]");
        // Insert ordinary text containing the exact same literal "[Image #1]" before the real element.
        t.key(KeyCode::Home, Modifiers::NONE);
        t.vt.paste("literal [Image #1] ");
        assert_eq!(t.vt.editor_text(), "literal [Image #1] [Image #1]");
        // Delete the real attachment element at the end; the literal prose remains.
        t.key(KeyCode::End, Modifiers::NONE);
        t.key(KeyCode::Backspace, Modifiers::NONE);
        assert_eq!(
            t.vt.editor_text(),
            "literal [Image #1] ",
            "deleting the real element leaves the lookalike prose untouched"
        );
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        let (count, sent_text): (i64, String) = eval(
            &t.vt.host,
            r#"
            local n=0
            local txt=""
            for _,m in ipairs(A:list_messages().data) do
                for _,b in ipairs(m.content or {}) do
                    if b.type=='image' then n=n+1 end
                    if b.type=='text' and m.role=='user' and not m.context_disclosure then txt=b.text end
                end
            end
            return n,txt
        "#,
        )
        .await;
        assert_eq!(count, 0, "deleting the real image element must not bind to an earlier literal [Image #1]");
        assert_eq!(sent_text, "literal [Image #1] ");
    })
    .await
}

#[tokio::test]
async fn kill_and_yank_preserves_image_attachment_across_submit_and_undo_does_not_panic() {
    local(async {
        let mut t =
            Term::new(json!([{"text":"saw image"},{"text":"after undo"},{"text":"after second undo"}]), 100, 30).await;
        expose(&t.vt.host);
        let image = t._env.cwd.join("kill-yank.png");
        std::fs::write(&image, TINY_PNG).unwrap();
        t.vt.paste(&image.display().to_string());
        t.vt.type_str(" describe");
        assert_eq!(t.vt.editor_text(), "[Image #1] describe");

        // Kill to start of line (Ctrl+U), then yank twice (Ctrl+Y, Ctrl+Y) and undo the second yank
        // so the surviving element comes from a yank.
        t.key(KeyCode::Char('u'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "");
        t.key(KeyCode::Char('y'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "[Image #1] describe");
        t.key(KeyCode::Char('y'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "[Image #1] describe[Image #1] describe");
        t.key(KeyCode::Char('_'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "[Image #1] describe");

        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        let encoded: String = eval(&t.vt.host, "return mare.json.encode(A:list_messages().data)").await;
        let msgs: Value = serde_json::from_str(&encoded).unwrap();
        let user_images = |idx: usize| -> Vec<Value> {
            msgs.as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user" && !m["notification"].as_bool().unwrap_or(false))
                .nth(idx)
                .unwrap()["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "image")
                .cloned()
                .collect()
        };
        let first_images = user_images(0);
        assert_eq!(first_images.len(), 1, "yanked image element must submit its image attachment: {encoded}");
        let expected_data = first_images[0]["data"].as_str().unwrap().to_owned();
        assert!(!expected_data.is_empty());
        assert_eq!(first_images[0]["mime"], "image/png");

        // Post-submit Ctrl+_ (undo) restores the element AND keeps its image data for the 2nd and 3rd submits.
        for turn in 1..=2 {
            t.key(KeyCode::Char('_'), Modifiers::CONTROL);
            assert_eq!(t.vt.editor_text(), "[Image #1] describe");
            t.ended = t.vt.turns_ended();
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.idle().await;

            let encoded: String = eval(&t.vt.host, "return mare.json.encode(A:list_messages().data)").await;
            let msgs: Value = serde_json::from_str(&encoded).unwrap();
            let imgs: Vec<Value> = msgs
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "user" && !m["notification"].as_bool().unwrap_or(false))
                .nth(turn)
                .unwrap()["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "image")
                .cloned()
                .collect();
            assert_eq!(imgs.len(), 1, "post-submit undo turn {turn} must still send full image data: {encoded}");
            assert_eq!(imgs[0]["mime"], "image/png");
            assert_eq!(imgs[0]["data"], expected_data);
        }
    })
    .await
}

#[tokio::test]
async fn repeated_yank_and_cross_submit_yank_preserve_full_image_data() {
    local(async {
        let mut t =
            Term::new(json!([{"text":"plain"},{"text":"yanked after submit"},{"text":"yanked twice"}]), 100, 30).await;
        expose(&t.vt.host);
        let image = t._env.cwd.join("repeat-yank.png");
        std::fs::write(&image, TINY_PNG).unwrap();
        t.vt.paste(&image.display().to_string());
        t.vt.type_str(" pic");
        assert_eq!(t.vt.editor_text(), "[Image #1] pic");

        // Kill the image prompt, submit a plain text prompt, then yank across submit.
        t.key(KeyCode::Char('u'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "");
        t.vt.type_str("plain");
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        t.key(KeyCode::Char('y'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "[Image #1] pic");
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        // Yank twice in the next prompt and submit both copies.
        t.key(KeyCode::Char('y'), Modifiers::CONTROL);
        t.key(KeyCode::Char('y'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "[Image #1] pic[Image #1] pic");
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        let encoded: String = eval(&t.vt.host, "return mare.json.encode(A:list_messages().data)").await;
        let msgs: Value = serde_json::from_str(&encoded).unwrap();
        let user_msgs: Vec<&Value> = msgs
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user" && !m["notification"].as_bool().unwrap_or(false))
            .collect();
        assert_eq!(user_msgs.len(), 3, "{encoded}");

        let imgs_for = |m: &Value| -> Vec<Value> {
            m["content"].as_array().unwrap().iter().filter(|b| b["type"] == "image").cloned().collect()
        };
        assert_eq!(imgs_for(user_msgs[0]).len(), 0, "plain prompt has no images");
        let second = imgs_for(user_msgs[1]);
        assert_eq!(second.len(), 1, "cross-submit yank must keep image data: {encoded}");
        let expected_data = second[0]["data"].as_str().unwrap();
        assert!(!expected_data.is_empty());
        assert_eq!(second[0]["mime"], "image/png");

        let third = imgs_for(user_msgs[2]);
        assert_eq!(third.len(), 2, "repeated yank must send both image attachments: {encoded}");
        assert_eq!(third[0]["data"], expected_data);
        assert_eq!(third[1]["data"], expected_data);
    })
    .await
}

#[tokio::test]
async fn scrollback_reflow_with_images_and_text_preserves_history_after_resize() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let pic = dir.path().join("reflow_pic.png");
        std::fs::write(&pic, TINY_PNG).unwrap();
        let path = pic.display().to_string().replace('\\', "/");
        let md_img = format!("img-before\n\n![pic]({path})\n\nimg-after");
        let mut t = Term::new(
            json!([
                { "text": "first plain text answer with enough words to rewrap across widths" },
                { "text": md_img }
            ]),
            80,
            20,
        )
        .await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(2, 2);
        t.submit("turn1").await;
        t.idle().await;
        t.submit("turn2").await;
        t.idle().await;
        t.resize(50, 20).await;
        t.until_vt("reflowed history keeps both answers", |vt| {
            let hist = vt.history();
            hist.contains("first plain text answer") && hist.contains("img-before") && hist.contains("img-after")
        })
        .await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);
    })
    .await
}

/// Without an image protocol, pasted user images and `read`-tool images still route through
/// the core `mare.ui.image` primitive and render `[image: <label> W×H]` (never bare `[image]`).
#[tokio::test]
async fn images_plugin_uses_core_placeholder_with_dimensions_without_protocol() {
    local(async {
        let mut t = Term::new(
            json!([
                { "tool_calls": [{ "id": "r1", "name": "read", "arguments": { "path": "shot.png" } }] },
                { "text": "done reading" }
            ]),
            80,
            24,
        )
        .await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        let pic = t._env.cwd.join("shot.png");
        std::fs::write(&pic, TINY_PNG).unwrap();
        t.vt.paste(&pic.display().to_string());
        t.vt.type_str(" inspect");
        t.until("pasted image and text rendered in editor", |s| s.contains("inspect")).await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        let hist = t.history();
        assert!(
            hist.contains("[image: shot.png 4×4]"),
            "read tool image must render via core placeholder with dimensions:\n{hist}"
        );
        assert!(
            hist.matches("4×4]").count() >= 2,
            "both user attachment and read tool image must use core placeholder with dimensions:\n{hist}"
        );
        assert!(
            !hist.contains("[image]\n") && !hist.ends_with("[image]"),
            "transcript must not emit bare literal [image]:\n{hist}"
        );
    })
    .await
}

/// Resizing with updated terminal pixel dimensions (e.g. font zoom or moving across DPI displays)
/// must update `image::cell_px()` and re-layout/re-encode committed images even when cols/rows stay fixed.
#[tokio::test]
async fn window_resize_with_pixel_size_updates_cell_px_and_recomputes_image_rows() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let pic = dir.path().join("dpi.png");
        let img = image::RgbaImage::from_pixel(64, 64, image::Rgba([40, 120, 220, 255]));
        img.save(&pic).unwrap();
        let md = format!("before\n\n![dpi]({})\n\nafter", pic.display().to_string().replace('\\', "/"));

        let mut t = Term::new(json!([{ "text": md }]), 80, 24).await;
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(8, 16);
        t.submit("dpi").await;
        t.until_vt("initial image committed at 8x16 cell_px", |t| {
            t.vt.turns_ended() > t.ended && !t.vt.busy() && t.vt.live_blocks() == 0
        })
        .await;
        let _ = t.vt.output();

        // Simulate 2x font/DPI zoom at identical 80x24 cell grid: 16x32 px per cell.
        // A 64x64 image must shrink from 8x4 cells to 4x2 cells (`c=4,r=2`).
        t.vt.resize_with_pixels(80, 24, Some(1280), Some(768));
        t.vt.frame().await;
        let out = String::from_utf8_lossy(&t.vt.output()).into_owned();

        assert_eq!(mare_tui::image::cell_px(), (16, 32), "WindowResized with pixel dimensions must update CELL_PX");
        assert!(
            out.contains("c=4,r=2,"),
            "image must re-layout and re-encode at new 16x32 cell_px (expected c=4,r=2); out={out:?}"
        );

        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);
    })
    .await
}

/// When a finished block waits on an unready image after the turn is idle (no working indicator,
/// no notices, no further key/UI events), `event_loop::step_loop` must wake up on
/// `image_hold.deadline()` after `wait_limit()` and commit the block with a text placeholder.
#[tokio::test]
async fn image_hold_deadline_wakes_production_loop_and_commits_after_wait_limit() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(2, 2);

        let (resolver, mut requests) = mare_shared::image_resource::Resolver::register();
        let rid = resolver.id();
        exec(
            &t.vt.host,
            &format!(
                r#"
                local id = mare.ui.push("assistant", {{
                    node = {{
                        type = "image",
                        resource = {{ resolver_id = {rid}, key = "stuck-pic", width = 16, height = 16 }},
                        max_cols = 40,
                        max_rows = 10,
                    }},
                }})
                mare.ui.finish(id)
                "#
            ),
        )
        .await;

        tokio::time::pause();
        t.vt.reset_loop_state();
        // First tick renders the finished block, queues the pending resource request, and starts Hold's 5s timer.
        t.vt.run_production_loop_for(Duration::from_millis(100)).await;
        assert_eq!(t.vt.live_blocks(), 1, "finished block with pending image must stay live before wait_limit");
        assert!(!t.vt.animating(), "no animation should be active while idle");

        // Advance paused tokio time past the 5s image_hold wait_limit with ZERO keys or UI events.
        let committed = t.vt.run_production_loop_until(Duration::from_secs(7), |vt| vt.live_blocks() == 0).await;
        let out = String::from_utf8_lossy(&t.vt.output()).into_owned();

        resolver.close();
        while let Ok(req) = requests.try_recv() {
            req.complete(Err("test ended".into()));
        }
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);

        assert!(committed, "step_loop must wake at image_hold deadline and commit degraded block; out={out:?}");
        assert!(out.contains("[image:"), "degraded commit must render text placeholder; out={out:?}");
    })
    .await
}
