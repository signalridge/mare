use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::common::*;

const PRESENTER_LUA: &str = include_str!("../../fixtures/presentation_fixture.lua");

struct ImageRecord {
    size: (u32, u32),
    identity: (u64, u64),
    offset: usize,
    png: Vec<u8>,
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn sum_cuf_offsets(prefix: &[u8]) -> usize {
    let mut total = 0;
    let mut i = 0;
    while i + 2 < prefix.len() {
        if prefix[i] == 0x1b && prefix[i + 1] == b'[' {
            let mut j = i + 2;
            while j < prefix.len() && prefix[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2
                && j < prefix.len()
                && prefix[j] == b'C'
                && let Ok(s) = std::str::from_utf8(&prefix[i + 2..j])
                && let Ok(v) = s.parse::<usize>()
            {
                total += v;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    total
}

fn image_records(raw: &[u8]) -> Vec<ImageRecord> {
    let mut payloads = Vec::new();
    let mut pending: Option<(Vec<u8>, (u64, u64), usize)> = None;
    let mut pos = 0;
    while let Some(rel) = find_subslice(&raw[pos..], b"\x1b_G") {
        let start = pos + rel;
        let body_start = start + 3;
        let Some(end_rel) = find_subslice(&raw[body_start..], b"\x1b\\") else {
            break;
        };
        let chunk = &raw[body_start..body_start + end_rel];
        pos = body_start + end_rel + 2;

        let Some(semi) = chunk.iter().position(|&b| b == b';') else {
            continue;
        };
        let options = &chunk[..semi];
        let data = &chunk[semi + 1..];

        let mut is_transmit = false;
        let mut has_m = false;
        let mut more = false;
        let mut id = None;
        let mut placement = 0u64;
        for field in options.split(|&b| b == b',') {
            if field == b"a=T" {
                is_transmit = true;
            } else if field == b"m=1" {
                has_m = true;
                more = true;
            } else if field == b"m=0" {
                has_m = true;
                more = false;
            } else if let Some(val) = field.strip_prefix(b"i=") {
                id = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok());
            } else if let Some(val) = field.strip_prefix(b"p=") {
                placement = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            }
        }

        if is_transmit {
            let line_start = raw[..start].iter().rposition(|&b| b == b'\r').map_or(0, |i| i + 1);
            let offset = sum_cuf_offsets(&raw[line_start..start]);
            let ident = (id.expect("kitty image id"), placement);
            pending = Some((data.to_vec(), ident, offset));
        } else if has_m && let Some((buf, _, _)) = &mut pending {
            buf.extend_from_slice(data);
        } else {
            continue;
        }

        if !more && let Some((b64, identity, offset)) = pending.take() {
            let png = base64::engine::general_purpose::STANDARD.decode(&b64).expect("valid base64 PNG");
            let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).expect("valid PNG image");
            payloads.push(ImageRecord { size: (img.width(), img.height()), identity, offset, png });
        }
    }
    payloads
}

struct PresentationPty {
    pty: PtySession,
    session_report: PathBuf,
    new_report: PathBuf,
    reflow_report: PathBuf,
    cancel_started: PathBuf,
}

fn start_presentation_pty(script: Value, extra_args: &[&str], images: bool, wait_agent_end: bool) -> PresentationPty {
    let answered = Arc::new(AtomicBool::new(false));
    let answered_cb = answered.clone();
    let version: &'static [u8] = if images { b"kitty(0.40.0)" } else { b"plain-test-terminal(1.0)" };

    let mut cmd = MareCmd::new()
        .args(std::iter::once("--yolo").chain(extra_args.iter().copied()))
        .faux(script)
        .plugin("presentation-fixture", PRESENTER_LUA)
        .env("TERM_PROGRAM", "mare-test-pty");
    let session_report = cmd.home_path().join("session.json");
    let cancel_started = cmd.home_path().join("cancel-started");
    cmd = cmd.env("PRESENTATION_SESSION_REPORT", &session_report).env("PRESENTATION_CANCEL_STARTED", &cancel_started);
    if wait_agent_end {
        cmd = cmd.env("PRESENTATION_WAIT_AGENT_END", "1");
    }
    let (cmd, caps_report) = cmd.report("ui/caps");
    let (cmd, ready_report) = cmd.report("ui/ready");
    let (cmd, new_report) = cmd.report("session/new");
    let (cmd, reflow_report) = cmd.report("ui/reflow");
    let cmd = cmd.respond(move |buf| {
        if !answered_cb.load(Ordering::Relaxed)
            && find_subslice(buf, b"\x1b[c").is_some()
            && find_subslice(buf, b"\x1b[>q").is_some()
        {
            answered_cb.store(true, Ordering::Relaxed);
            let mut reply = Vec::new();
            reply.extend_from_slice(b"\x1bP>|");
            reply.extend_from_slice(version);
            reply.extend_from_slice(b"\x1b\\\x1b[6;16;8t\x1b[3;1R\x1b[?62;22c");
            Some(reply)
        } else {
            None
        }
    });
    let p = cmd.pty(100, 30);
    let caps = p.wait_file(&caps_report);
    assert!(answered.load(Ordering::Relaxed), "native XTVERSION probe was not answered");
    let expected = if images { "kitty" } else { "none" };
    assert_eq!(caps["images"], expected, "caps: {caps}");
    p.wait_file(&ready_report);
    PresentationPty { pty: p, session_report, new_report, reflow_report, cancel_started }
}

#[test]
fn late_updates_replay_and_narrow_resize() {
    let script = json!([{
        "text": "before\n\n> ```drawing\n> slow\n> ```\n\n- list\n\n  > ```drawing\n  > slow\n  > ```\n\nafter"
    }]);
    let PresentationPty { pty: mut p, session_report, .. } = start_presentation_pty(script, &[], true, true);
    p.send(b"render a diagram\r");
    p.wait_raw("2 kitty images", |raw| {
        image_records(raw).iter().map(|r| r.identity).collect::<BTreeSet<_>>().len() >= 2
    });
    let records = image_records(&p.bytes());
    let offsets: BTreeSet<usize> = records.iter().map(|r| r.offset).collect();
    assert!(offsets.contains(&4) && offsets.contains(&6), "expected offsets 4 and 6, got {offsets:?}");
    let pngs: BTreeSet<&[u8]> = records.iter().map(|r| r.png.as_slice()).collect();
    assert_eq!(pngs.len(), 1, "same PNG data must remain unchanged across occurrences");
    let session = p.wait_file(&session_report);
    let raw = p.bytes();
    assert!(
        find_subslice(&raw, b"before").is_some() && find_subslice(&raw, b"after").is_some(),
        "missing before/after in PTY output:\n{}",
        p.screen()
    );
    let session_path = session["path"].as_str().expect("session path").to_string();
    p.send(b"\x04");
    p.wait_exit(15);

    let answered = Arc::new(AtomicBool::new(false));
    let answered_cb = answered.clone();
    let cmd = MareCmd::new()
        .args(["--yolo", "--resume", &session_path])
        .plugin("presentation-fixture", PRESENTER_LUA)
        .env("TERM_PROGRAM", "mare-test-pty");
    let (cmd, caps_report) = cmd.report("ui/caps");
    let (cmd, ready_report) = cmd.report("ui/ready");
    let mut resumed = cmd
        .respond(move |buf| {
            if !answered_cb.load(Ordering::Relaxed)
                && find_subslice(buf, b"\x1b[c").is_some()
                && find_subslice(buf, b"\x1b[>q").is_some()
            {
                answered_cb.store(true, Ordering::Relaxed);
                Some(b"\x1bP>|kitty(0.40.0)\x1b\\\x1b[6;16;8t\x1b[3;1R\x1b[?62;22c".to_vec())
            } else {
                None
            }
        })
        .pty(100, 30);
    resumed.wait_file(&caps_report);
    resumed.wait_screen("resume prompt or ready", |s| {
        s.contains("recorded in another directory") || ready_report.exists()
    });
    if resumed.screen().contains("recorded in another directory") {
        resumed.send(b"\x1b[B\r");
    }
    resumed.wait_file(&ready_report);
    resumed.wait_raw("replayed kitty images", |raw| !image_records(raw).is_empty());
    let mark = resumed.bytes().len();
    resumed.resize(40, 30);
    resumed.wait_raw("narrow resize graphics", |raw| {
        raw.len() > mark
            && find_subslice(&raw[mark..], b"viewport:40").is_some()
            && !image_records(&raw[mark..]).is_empty()
    });
    assert!(resumed.running());
    resumed.send(b"\x04");
    resumed.wait_exit(15);
}

// Adapted from DeepSeek Harness `python/sdk/tests/test_client.py:506`
#[test]
fn cancelled_turn_and_no_image_terminal() {
    let body = format!("```drawing\nslow-cancel\n```\n\nCANCEL-TAIL {}", "pending ".repeat(300));
    let script = json!([
        { "text": body, "delay_ms": 30 },
        { "text": "```drawing\nnew generation\n```" }
    ]);
    let PresentationPty { pty: mut p, cancel_started, reflow_report, .. } =
        start_presentation_pty(script, &["--no-session"], true, false);
    p.send(b"cancel diagram\r");
    p.wait_raw("CANCEL-TAIL", |raw| find_subslice(raw, b"CANCEL-TAIL").is_some());
    let first_cancel = p.wait_file(&cancel_started);
    assert_eq!(first_cancel["seq"], 1);
    // Force a same-block, same-generation re-render at a new revision while render 1 is in-flight.
    let _ = std::fs::remove_file(&cancel_started);
    p.resize(97, 30);
    let second_cancel = p.wait_file(&cancel_started);
    assert_eq!(second_cancel["seq"], 2);
    assert_ne!(second_cancel["request_id"], first_cancel["request_id"]);
    assert_ne!(second_cancel["revision"], first_cancel["revision"]);
    assert!(
        find_subslice(&p.bytes(), b"CANCEL-BARRIER-2").is_none(),
        "superseded render A must not arm CANCEL-BARRIER-2 while render B is still in-flight"
    );
    p.send(b"\x03");
    p.wait_text("interrupted", |s| s.to_ascii_lowercase().contains("interrupted"));
    // Timing-independent terminal output barrier: `slow-cancel` arms `CANCEL-BARRIER-<seq>`
    // only when `ui/presentation-render-done` fires with matching request_id/revision/
    // generation/block_id and `accepted == false`, and all pending cancelled renders settle.
    // Then trigger a resize reflow (`ui/reflow`) so any debounced `BlockUpdate` replay and
    // background image preparation run before we proceed, and again after the next turn's
    // image so cached images are flushed before `wait_exit` drains PTY.
    p.wait_raw("CANCEL-BARRIER-2", |raw| find_subslice(raw, b"CANCEL-BARRIER-2").is_some());
    let _ = std::fs::remove_file(&reflow_report);
    p.resize(99, 30);
    p.wait_file(&reflow_report);
    assert!(image_records(&p.bytes()).is_empty(), "cancelled occurrence produced late image bytes");
    p.send(b"next\r");
    p.wait_raw("next turn image", |raw| !image_records(raw).is_empty());
    let mark = p.bytes().len();
    let _ = std::fs::remove_file(&reflow_report);
    p.resize(98, 30);
    p.wait_file(&reflow_report);
    p.wait_raw("rescaled next turn image", |raw| !image_records(&raw[mark..]).is_empty());
    p.send(b"\x03\x03");
    p.wait_exit(15);
    let recs = image_records(&p.bytes());
    assert!(!recs.is_empty(), "expected next turn image");
    for rec in recs {
        assert!(rec.size.0 >= rec.size.1, "stale cancelled variant leaked: {:?}", rec.size);
    }

    let script = json!([{ "text": "```drawing\nslow\ncomplete source\n```" }]);
    let PresentationPty { pty: mut plain, session_report, .. } =
        start_presentation_pty(script, &["--no-session"], false, false);
    plain.send(b"source\r");
    plain.wait_raw("complete source", |raw| find_subslice(raw, b"complete source").is_some());
    plain.wait_file(&session_report);
    assert!(image_records(&plain.bytes()).is_empty());
    let sc = plain.screen();
    assert!(sc.contains("slow") && sc.contains("complete source"), "screen:\n{sc}");
    plain.send(b"\x03\x03");
    plain.wait_exit(15);
}

#[test]
fn partial_stream_and_old_generation_rejected() {
    let body = format!("lead\n\n```drawing\nslow\n```\n\n{}PARTIAL-DONE", "tail ".repeat(150));
    let PresentationPty { pty: mut p, session_report, .. } =
        start_presentation_pty(json!([{ "text": body, "delay_ms": 8 }]), &["--no-session"], true, false);
    p.send(b"stream\r");
    p.wait_raw("early stream image", |raw| !image_records(raw).is_empty());
    assert!(!p.screen().contains("PARTIAL-DONE"), "image must replace a partially committed stream");
    p.wait_raw("PARTIAL-DONE", |raw| find_subslice(raw, b"PARTIAL-DONE").is_some());
    p.wait_file(&session_report);
    assert!(p.screen().contains("PARTIAL-DONE"));
    let ids: BTreeSet<_> = image_records(&p.bytes()).into_iter().map(|r| r.identity).collect();
    assert_eq!(ids.len(), 1, "identity must survive stream updates");
    p.send(b"\x03\x03");
    p.wait_exit(15);

    let script = json!([
        { "text": "```drawing\nslow-cancel\n```\n\nAFTER-GEN" },
        { "text": "```drawing\nnew scope\n```" }
    ]);
    let PresentationPty { pty: mut p, new_report, reflow_report, cancel_started, .. } =
        start_presentation_pty(script, &["--no-session"], true, false);
    p.send(b"old scope\r");
    p.wait_raw("AFTER-GEN", |raw| find_subslice(raw, b"AFTER-GEN").is_some());
    p.wait_file(&cancel_started);
    let _ = std::fs::remove_file(&new_report);
    p.send(b"/new");
    p.wait_screen("/new", |s| s.contains("/new"));
    p.send(b"\r");
    p.wait_file(&new_report);
    // Same `UiCmd` FIFO barrier + resize reflow + PTY exit drain as `cancelled_turn_and_no_image_terminal`.
    p.wait_raw("CANCEL-BARRIER-1", |raw| find_subslice(raw, b"CANCEL-BARRIER-1").is_some());
    let _ = std::fs::remove_file(&reflow_report);
    p.resize(99, 30);
    p.wait_file(&reflow_report);
    assert!(image_records(&p.bytes()).is_empty(), "old generation emitted a late graph");
    p.send(b"new scope\r");
    p.wait_raw("new scope image", |raw| !image_records(raw).is_empty());
    let mark = p.bytes().len();
    let _ = std::fs::remove_file(&reflow_report);
    p.resize(98, 30);
    p.wait_file(&reflow_report);
    p.wait_raw("rescaled new scope image", |raw| !image_records(&raw[mark..]).is_empty());
    p.send(b"\x03\x03");
    p.wait_exit(15);
    let recs = image_records(&p.bytes());
    assert!(!recs.is_empty(), "expected new scope image");
    for rec in recs {
        assert!(rec.size.0 >= rec.size.1, "stale old generation variant leaked: {:?}", rec.size);
    }
}
