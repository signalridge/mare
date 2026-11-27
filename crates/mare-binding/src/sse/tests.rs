//! Decoder tests.
//!
//! - `[sse-stream]`: ported from sse-stream 0.3.0 (MIT OR Apache-2.0,
//!   <https://github.com/4t145/sse-stream>): `tests/test_bytes_parse/{fields,fragments,input,
//!   lifecycle}.rs`, `tests/test_data_capacity.rs`, `tests/test_utf8.rs` and the decode half of
//!   `src/body/tests.rs`; they check the raw blocks ([`Blocks`]).
//! - `[eventsource-parser]`: translated from eventsource-parser 4.1.3 (MIT,
//!   <https://github.com/rexxars/eventsource-parser>, commit
//!   f46b5575f72682527ed936763983bdda0e1283f5) `test/parse.test.ts` / `test/fixtures.ts`; they
//!   check dispatched events ([`dispatch`]: the last event ID buffer carries on, a block without
//!   data dispatches nothing).
//! - unmarked: mare's caps, comment callback and linear-time regression.

use std::convert::Infallible;

use futures_util::{FutureExt, StreamExt};

use super::*;

#[derive(Debug, Default)]
struct Out {
    blocks: Vec<Sse>,
    events: Vec<Event>,
    comments: Vec<String>,
    /// the terminal error, displayed
    error: Option<String>,
    /// the last `retry:` seen
    retry: Option<u64>,
}

fn run_caps(chunks: &[&[u8]], caps: Caps) -> Out {
    let comments = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = comments.clone();
    let on_comment = Box::new(move |t: &[u8]| sink.lock().unwrap().push(String::from_utf8_lossy(t).into_owned()));
    let input: Vec<Result<Bytes, Infallible>> = chunks.iter().map(|c| Ok(Bytes::copy_from_slice(c))).collect();
    let mut s = Blocks::new(futures_util::stream::iter(input), caps, on_comment);
    let mut out = Out::default();
    let mut last_id = String::new();
    while let Some(item) = s.next().now_or_never().expect("ready input") {
        match item {
            Ok(b) => {
                out.retry = b.retry.or(out.retry);
                out.blocks.push(b.clone());
                out.events.extend(dispatch(&mut last_id, b));
            }
            Err(e) => out.error = Some(e.to_string()),
        }
    }
    assert!(s.is_terminated());
    out.comments = std::mem::take(&mut *comments.lock().unwrap());
    out
}

fn run(chunks: &[&[u8]]) -> Out {
    run_caps(chunks, Caps::default())
}

fn data(out: &Out) -> Vec<&str> {
    out.events.iter().map(|e| e.data.as_str()).collect()
}

/// The same input split at every width (1 byte .. whole), so every boundary is exercised.
fn every_width(input: &[u8]) -> impl Iterator<Item = (usize, Out)> + '_ {
    (1..=input.len().max(1)).map(move |w| (w, run(&input.chunks(w).collect::<Vec<_>>())))
}

fn ev(event: &str, data: &str, id: &str) -> Event {
    Event { event: event.into(), data: data.into(), id: id.into() }
}

fn data_only(s: &str) -> Sse {
    Sse::default().data(s)
}

// ---- line endings -------------------------------------------------------------------------

#[test]
fn crlf_lf_and_lone_cr_end_lines() {
    // [sse-stream] test_line_break_crlf / _cr_only / _mixed / test_multiple_consecutive_cr
    assert_eq!(data(&run(&[b"data: a\r\ndata: b\r\n\r\n"])), ["a\nb"]);
    assert_eq!(data(&run(&[b"data: a\rdata: b\r\r"])), ["a\nb"]);
    assert_eq!(data(&run(&[b"data: one\r\ndata: two\rdata: three\n\r\n"])), ["one\ntwo\nthree"]);
    assert_eq!(data(&run(&[b"data: a\r\r\r"])), ["a"]);
    // [eventsource-parser] stream using carriage returns / line feeds / both
    for nl in ["\r", "\n", "\r\n"] {
        let s = format!("data: dog{nl}data: bark{nl}{nl}data: cat{nl}data: meow{nl}{nl}");
        for (w, out) in every_width(s.as_bytes()) {
            assert_eq!(data(&out), ["dog\nbark", "cat\nmeow"], "{nl:?} width {w}");
        }
    }
}

#[test]
fn crlf_split_across_chunks_is_one_line_ending() {
    // [sse-stream] test_cr_lf_split_across_chunks, test_empty_chunk_between_split_crlf,
    // test_cr_then_non_lf_across_chunks, test_cr_then_cr_across_chunks, test_dispatch_boundary_split_at_cr
    assert_eq!(data(&run(&[b"data: hello\r", b"\ndata: world\n\n"])), ["hello\nworld"]);
    assert_eq!(data(&run(&[b"data: hello\r", b"", b"\ndata: world\n\n"])), ["hello\nworld"]);
    assert_eq!(data(&run(&[b"data: a\r", b"data: b\n\n"])), ["a\nb"]);
    assert_eq!(data(&run(&[b"data: a\r", b"\rdata: b\n\n"])), ["a", "b"]);
    assert_eq!(data(&run(&[b"data: x\r\n\r", b"\n"])), ["x"]);
    // [eventsource-parser] stream with cr separating chunks of same event (issue #17)
    assert_eq!(data(&run(&[b"data: A\r\n", b"data: B\r", b"\n", b"data: C\r\n", b"\n"])), ["A\nB\nC"]);
    // [eventsource-parser] a blank cr line ending a chunk dispatches the pending event exactly once
    assert_eq!(run(&[b"data: one\r", b"\r", b"\n"]).events, [ev("message", "one", "")]);
}

// ---- BOM ----------------------------------------------------------------------------------

#[test]
fn a_leading_bom_is_stripped_once() {
    // [sse-stream] test_bom_header_at_start, test_bom_split_across_chunks
    assert_eq!(data(&run(&[b"\xEF\xBB\xBFdata: hello\n\n"])), ["hello"]);
    assert_eq!(data(&run(&[b"\xEF", b"\xBB\xBFdata: hello\n\n"])), ["hello"]);
    // [eventsource-parser] stream of leading bom / empty chunks do not make a later BOM leading:
    // a BOM anywhere else is part of the field name, so the line is an unknown field
    let s = b"\xEF\xBB\xBFdata: bomful 1\n\ndata: bomless 2\n\n\xEF\xBB\xBFdata: ignored\n\ndata: last\n\n";
    for (w, out) in every_width(s) {
        assert_eq!(data(&out), ["bomful 1", "bomless 2", "last"], "width {w}");
    }
    assert_eq!(
        data(&run(&[b"data: first\n\n", b"", b"\xEF\xBB\xBFdata: ignored\n\ndata: last\n\n"])),
        ["first", "last"]
    );
}

// ---- fields -------------------------------------------------------------------------------

#[test]
fn a_line_without_a_colon_is_a_field_with_an_empty_value() {
    // [eventsource-parser] stream with oddly shaped data field
    assert_eq!(data(&run(&[b"data:\n\ndata\ndata\n\ndata:test\n\n"])), ["", "\n", "test"]);
    // [sse-stream] field_rules_are_independent_of_fragmentation (`retry` / `id` / `event` bare)
    let out = run(&[b"id: 1\nretry: 5\nretry\nid\nevent\ndata: x\n\n"]);
    assert_eq!(out.events, [ev("message", "x", "")], "bare id empties the id buffer");
    assert_eq!(out.retry, Some(5), "bare retry is not all digits: ignored");
}

#[test]
fn data_lines_join_with_lf() {
    // [eventsource-parser] stream of multi-line data events
    let out = run(&[
        b"event: stock\n",
        b"data: YHOO\n",
        b"data: +2\n",
        b"data: 10\n\n",
        b"event: stock\n",
        b"data: GOOG\n",
        b"data: -8\n",
        b"data: 1881\n\n",
    ]);
    assert_eq!(out.events, [ev("stock", "YHOO\n+2\n10", ""), ev("stock", "GOOG\n-8\n1881", "")]);
    // [sse-stream] test_two_empty_data_lines_join_with_newline
    assert_eq!(data(&run(&[b"data:\ndata:\n\n"])), ["\n"]);
}

#[test]
fn empty_data_dispatches_an_empty_event_and_no_data_dispatches_nothing() {
    // [sse-stream] test_empty_data_field; [eventsource-parser] reports an `id` from a block
    // without data — the id is buffered, no event fires
    assert_eq!(run(&[b"data:\n\n"]).events, [ev("message", "", "")]);
    let out = run(&[b"id: 42\n\nevent: x\n\nretry: 9\n\n"]);
    assert!(out.events.is_empty(), "{out:?}");
    assert_eq!(run(&[b"id: 42\n\ndata: later\n\n"]).events, [ev("message", "later", "42")]);
}

#[test]
fn unknown_fields_are_ignored() {
    // [eventsource-parser] stream with unknown fields in the stream
    let s = b"data:abc\n data\ndata\nfoobar:xxx\njustsometext\n:thisisacommentyay\ndata:123\n\nevent: done\ndata: \xE2\x9C\x94\n\n";
    for (w, out) in every_width(s) {
        assert_eq!(out.events, [ev("message", "abc\n\n123", ""), ev("done", "✔", "")], "width {w}");
    }
    // [eventsource-parser] stream with empty `event` field
    assert_eq!(run(&[b"event:\ndata: Hello 1\n\n"]).events, [ev("message", "Hello 1", "")]);
}

#[test]
fn the_last_duplicate_field_wins() {
    // [sse-stream] field_rules_are_independent_of_fragmentation
    let input = concat!(
        "event: old\nevent: new\n",
        "id: old\nid: new\nid: bad\0id\n",
        "retry: 12\nretry: 34\nretry: +56\nretry: 78 \n",
        "retry: 18446744073709551616\nretry\n",
        "data\ndata: tail\n\n",
        "data: next\n\n",
    );
    for (w, out) in every_width(input.as_bytes()) {
        assert_eq!(out.events, [ev("new", "\ntail", "new"), ev("message", "next", "new")], "width {w}");
        assert_eq!(out.retry, Some(34), "width {w}");
    }
}

#[test]
fn an_id_with_nul_is_ignored_and_the_last_id_carries_on() {
    // [eventsource-parser] stream with `id` field containing a NULL character
    assert_eq!(run(&[b"id: 123\nid: bad\0id\ndata: hello\n\n"]).events, [ev("message", "hello", "123")]);
    // [sse-stream] test_id_with_null_byte_ignored
    assert_eq!(run(&[b"id: ab\x00cd\ndata: x\n\n"]).events, [ev("message", "x", "")]);
    // [eventsource-parser] stream of identified messages: each block sets its own id
    let out =
        run(&[b"retry: 50\nid: 1337\nevent: tick\ndata: 1337\n\nretry: 50\nid: 1338\nevent: tick\ndata: 1338\n\n"]);
    assert_eq!(out.events, [ev("tick", "1337", "1337"), ev("tick", "1338", "1338")]);
    assert_eq!(out.retry, Some(50));
    // the buffer survives events without an id; `id:` with an empty value clears it
    let out = run(&[b"id: 7\ndata: a\n\ndata: b\n\nid:\ndata: c\n\n"]);
    assert_eq!(data(&out), ["a", "b", "c"]);
    assert_eq!(out.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["7", "7", ""]);
}

#[test]
fn retry_takes_ascii_digits_only() {
    // [eventsource-parser] stream with partially incorrect / incorrect / empty retry fields
    assert_eq!(run(&[b"retry:1000\nretry:2000x\n\ndata:x\n\n"]).retry, Some(1000));
    assert_eq!(run(&[b"\nretry: 500\n\ndata: first\n\nretry: 50x\n\ndata: second\n\n"]).retry, Some(500));
    assert_eq!(run(&[b"retry: 500\n\nretry:\n\n"]).retry, Some(500));
    // [sse-stream] leading zeros parse, overflow / sign / trailing space are ignored
    assert_eq!(run(&[b"retry: 0007\n\n"]).retry, Some(7));
    assert_eq!(run(&[b"retry: 18446744073709551616\nretry: -1\nretry: +5\nretry: 5 \n\n"]).retry, None);
}

// ---- comments -----------------------------------------------------------------------------

#[test]
fn a_comments_only_stream_dispatches_nothing_and_reports_each_comment() {
    // [sse-stream] test_fragmented_comment_lines
    let out = run(&[b": ke", b"ep", b"alive\n", b"\n"]);
    assert!(out.events.is_empty());
    assert_eq!(out.comments, [""], "a comment in pieces is skipped, not buffered");
    assert_eq!(data(&run(&[b": comm", b"ent\ndata: before\n\n"])), ["before"]);
    assert_eq!(data(&run(&[b"data: after\n: comm", b"ent\n\n"])), ["after"]);
    // [eventsource-parser] stream of "heartbeat" comments, unnamed events
    let mut chunks: Vec<Vec<u8>> = vec![];
    for c in 'A'..='E' {
        chunks.push(b": \xE2\x99\xA5\n".to_vec());
        chunks.push(format!("data: {c}\n\n").into_bytes());
    }
    chunks.push(b"event: done\ndata: x\n\n".to_vec());
    let out = run(&chunks.iter().map(Vec::as_slice).collect::<Vec<_>>());
    assert_eq!(data(&out), ["A", "B", "C", "D", "E", "x"]);
    assert_eq!(out.comments, [" ♥"; 5]);
}

#[test]
fn odd_comments_between_and_inside_events() {
    // [eventsource-parser] stream with varying / even more odd uses of comments
    let long = "x".repeat(2 * 1024 + 1);
    let colons = ":".repeat(300);
    let out = run(&[
        b": Hello\n\n",
        colons.as_bytes(),
        b"\n",
        b"data: First\n\n",
        ": Первый".as_bytes(),
        ": 第二".as_bytes(),
        b"\n",
        b"data: Second\n\n",
    ]);
    assert_eq!(data(&out), ["First", "Second"]);
    let out = run(&[
        b"data:1\r\r:\0\n:\r\ndata:2\n\n:",
        long.as_bytes(),
        b"\rdata:3\n\n:data:fail\r:",
        long.as_bytes(),
        b"\ndata:4\n\n",
        b"data:5",
    ]);
    assert_eq!(data(&out), ["1", "2", "3", "4"], "data:5 has no blank line");
}

// ---- end of stream and UTF-8 --------------------------------------------------------------

#[test]
fn an_unfinished_event_at_the_end_is_discarded() {
    // [sse-stream] test_incomplete_trailing_event_discarded
    assert_eq!(data(&run(&[b"data: complete\n\ndata: incomplete\n"])), ["complete"]);
    assert_eq!(data(&run(&[b"data: complete\n\ndata: no line end"])), ["complete"]);
}

#[test]
fn multi_byte_characters_split_across_chunks_reassemble() {
    // [sse-stream] test_event_boundaries_preserve_fields_across_every_split,
    // test_fragmented_data_line_split_mid_utf8_char, test_fragmented_data_line_five_byte_chunks
    let input = concat!(
        "data: 首条🙂\n\n",
        "event: update\nid: stream/7\nretry: 1000\n",
        "data: first\ndata:\ndata: last\n\n",
        "data: before-id\nid: stream/8\n\n",
        ": keepalive\r\n\r\n",
        "data: crlf\r\n\r\n",
        "data:\n\n",
        "data: unfinished"
    )
    .as_bytes();
    let want = [
        ev("message", "首条🙂", ""),
        ev("update", "first\n\nlast", "stream/7"),
        ev("message", "before-id", "stream/8"),
        ev("message", "crlf", "stream/8"),
        ev("message", "", "stream/8"),
    ];
    let want_blocks = [
        data_only("首条🙂"),
        Sse::default().event("update").id("stream/7").retry(1000).data("first\n\nlast"),
        Sse::default().data("before-id").id("stream/8"),
        data_only("crlf"),
        data_only(""),
    ];
    for split in 0..=input.len() {
        let out = run(&[&input[..split], b"", &input[split..]]);
        assert_eq!(out.events, want, "split {split}");
        assert_eq!(out.blocks, want_blocks, "split {split}");
        assert_eq!(out.retry, Some(1000));
    }
    assert_eq!(data(&run(&[b"data: \xe4", b"\xbd\xa0 ok\n\n"])), ["你 ok"]);
    let s = "data: 你好世界🦀\n\n";
    assert_eq!(data(&run(&s.as_bytes().chunks(5).collect::<Vec<_>>())), ["你好世界🦀"]);
}

#[test]
fn invalid_utf8_in_a_field_is_an_error() {
    // [sse-stream] test_invalid_utf8_in_fragmented_data_line
    assert_eq!(
        run(&[b"data: \xff", b"\xfe\n\n"]).error,
        Some("utf8 parse error: invalid utf-8 sequence of 1 bytes from index 0".into())
    );
    // [sse-stream] ignored_fields_do_not_create_events: an invalid comment is just a comment
    assert_eq!(data(&run(&[b": \xff\r\n\r\ndata: after\n\n"])), ["after"]);
}

// ---- caps and linear time -----------------------------------------------------------------

#[test]
fn lines_and_events_over_their_caps_are_errors() {
    let caps = Caps { line: 16, event: 24 };
    // at the caps: 16-byte lines; the blank line resets the event's count
    let out = run_caps(&[b"data: 0123456789\r\ndata: 01\r\n\r\ndata: 0123456789\n\n"], caps);
    assert_eq!((out.events.len(), out.error), (2, None));
    // a line over the cap, complete or still arriving (never buffered past the cap)
    assert_eq!(run_caps(&[b"data: 0123456789A\n\n"], caps).error, Some("an SSE line is longer than 16 bytes".into()));
    let chunks: Vec<&[u8]> = vec![b"data: 01234"; 1000];
    assert_eq!(run_caps(&chunks, caps).error, Some("an SSE line is longer than 16 bytes".into()));
    // [eventsource-parser] maxBufferSize: triggers on data buffer overflow (no blank line)
    let out = run_caps(&[b"data: 0123456789\n", b"data: 0123456789\n", b"data: 012\n\n"], caps);
    assert_eq!(out.error, Some("an SSE event is larger than 24 bytes".into()));
    assert!(out.events.is_empty());
    // [eventsource-parser] maxBufferSize: preserves events dispatched before an overflow
    let out = run_caps(&[b"data: first\n\ndata: 0123456789\ndata: 0123456789\ndata: 012\n\ndata: later\n\n"], caps);
    assert_eq!((data(&out), out.error.clone()), (vec!["first"], Some("an SSE event is larger than 24 bytes".into())));
    // comments never accumulate toward the event cap
    let chunks: Vec<&[u8]> = vec![b": ping\n"; 100];
    assert_eq!(run_caps(&chunks, caps).error, None);
}

/// eventsource-stream#11: re-parsing the whole buffer per chunk made a long line quadratic.
/// [eventsource-parser] stream with huge data chunks (a multi-MiB event in small chunks).
#[test]
fn a_long_data_line_in_small_chunks_decodes_in_linear_time() {
    // Work is counted, not timed: every byte handed to a line-end scan. Re-parsing the buffer
    // per chunk (the quadratic bug) makes 4x the data cost ~16x the scanned bytes, linear
    // decoding exactly 4x, and no machine load changes either number.
    fn scanned(mib: usize) -> (u64, usize) {
        let mut body = b"data: ".to_vec();
        body.resize(6 + mib * 1024 * 1024, b'x');
        body.extend_from_slice(b"\n\n");
        let chunks: Vec<&[u8]> = body.chunks(256).collect();
        super::SCANNED.with(|c| c.set(0));
        let out = run(&chunks);
        let n = super::SCANNED.with(|c| c.get());
        assert_eq!(out.error, None);
        assert_eq!(out.events.iter().map(|e| e.data.len()).collect::<Vec<_>>(), [mib * 1024 * 1024]);
        (n, body.len())
    }
    let (small, small_len) = scanned(1);
    let (large, large_len) = scanned(4);
    assert!(small >= small_len as u64, "the counter sees the scans: {small} for {small_len} bytes");
    // a small constant number of passes over the input (cap counter + parser), whatever the size
    assert!(large <= 3 * large_len as u64, "4 MiB scanned {large} bytes for {large_len} bytes of input");
    let ratio = large as f64 / small as f64;
    assert!(ratio <= 4.5, "1 MiB scanned {small}, 4 MiB scanned {large}: ratio {ratio:.2}");
}

#[tokio::test]
async fn blocks_before_an_over_cap_line_come_first_then_the_error_ends_the_stream() {
    let chunks: Vec<Result<&'static [u8], Infallible>> =
        vec![Ok(b"data: a\n\n: hi\ndata: 0123456789ABCDEFG\n"), Ok(b"data: never\n\n")];
    let comments = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = comments.clone();
    let on_comment = Box::new(move |_: &[u8]| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let mut s = Blocks::new(futures_util::stream::iter(chunks), Caps { line: 16, event: 64 }, on_comment);
    assert_eq!(s.next().await.unwrap().unwrap(), data_only("a"));
    assert!(matches!(s.next().await, Some(Err(Error::LineTooLong(16)))));
    assert!(s.next().await.is_none());
    assert_eq!(comments.load(std::sync::atomic::Ordering::SeqCst), 1, "the comment before the cut");
}

/// Heartbeats reach `on_comment` whole or in pieces (a comment split across chunks is skipped,
/// not buffered, and reported with an empty text): either way the idle watchdog is fed.
#[test]
fn every_comment_line_reaches_the_callback() {
    let out = run(&[b": ping\n\n", b": pi", b"ng\n", b"\xEF\xBB\xBF: bom\n", b"data: x\n: in\n\n"]);
    assert_eq!(data(&out), ["x"]);
    assert_eq!(out.comments, [" ping", "", " in"], "the BOM is not leading here: an unknown field");
    let out = run(&[b"\xEF\xBB\xBF: leading\n\n"]);
    assert_eq!(out.comments, [" leading"]);
}

// ---- [sse-stream] raw blocks ----------------------------------------------------------------

#[test]
fn raw_field_rules_are_independent_of_fragmentation() {
    // [sse-stream] fields.rs field_rules_are_independent_of_fragmentation
    let input = concat!(
        "event: old\nevent: new\n",
        "id: old\nid: new\nid: bad\0id\n",
        "retry: 12\nretry: 34\nretry: +56\nretry: 78 \n",
        "retry: 18446744073709551616\nretry\n",
        "data\ndata: tail\n\n",
        "id\nevent\n\n",
        "retry: 0007\n\n",
        "data: next\n\n",
    );
    let expected = vec![
        Sse::default().event("new").id("new").retry(34).data("\ntail"),
        Sse::default().id("").event(""),
        Sse::default().retry(7),
        Sse::default().data("next"),
    ];
    for (w, out) in every_width(input.as_bytes()) {
        assert_eq!(out.blocks, expected, "width={w}");
    }
    // [sse-stream] fields.rs ignored_fields_do_not_create_events
    assert!(run(&[b"retry: nope\nid: \0\n\n"]).blocks.is_empty());
    for (w, out) in every_width(b": \xff\r\n\r\ndata: after\n\n") {
        assert_eq!(out.blocks, [data_only("after")], "width={w}");
    }
}

#[test]
fn raw_data_prefix_and_field_continuations_split_anywhere() {
    // [sse-stream] fragments.rs test_data_prefix_split_at_every_byte
    for chunks in [
        [&b"d"[..], b"ata: hello\n\n"],
        [b"da", b"ta: hello\n\n"],
        [b"dat", b"a: hello\n\n"],
        [b"data", b": hello\n\n"],
        [b"data:", b" hello\n\n"],
    ] {
        assert_eq!(run(&chunks).blocks, [data_only("hello")]);
    }
    // [sse-stream] test_only_one_leading_space_stripped, test_field_continuations_are_not_new_lines
    assert_eq!(data(&run(&[b"data:  hello\n\n"])), [" hello"]);
    assert_eq!(data(&run(&[b"data:no-space\n\n"])), ["no-space"]);
    let input = b"event: data: update\nid: : stream/7\n: data: ignored\ndata: payload\n\n";
    for split in 0..=input.len() {
        let out = run(&[&input[..split], b"", &input[split..]]);
        assert_eq!(out.events, [ev("data: update", "payload", ": stream/7")], "split={split}");
        assert_eq!(
            out.blocks,
            [Sse::default().event("data: update").id(": stream/7").data("payload")],
            "split={split}"
        );
    }
    // test_split_prefix_ending_at_colon_preserves_space_handling
    for tail in ["hello", " hello", ""] {
        let input = format!("data: {tail}\n\n");
        let b = input.as_bytes();
        for split in 1..5 {
            assert_eq!(
                run(&[&b[..split], b"", &b[split..5], b"", &b[5..]]).blocks,
                [data_only(tail)],
                "{tail:?} {split}"
            );
        }
    }
    // test_fragmented_data_line_empty_first_fragment / _does_not_leak_into_next_event
    assert_eq!(run(&[b"data:", b"payload\n\n"]).blocks, [data_only("payload")]);
    assert_eq!(
        run(&[b"data: first-", b"part2\n\nda", b"ta: second\n\n"]).blocks,
        [data_only("first-part2"), data_only("second")]
    );
    // test_multiple_events_split_chunks
    assert_eq!(
        run(&[b"event: a\ndata: 1\n", b"\nevent: b\nda", b"ta: 2\n\n"]).blocks,
        [Sse::default().event("a").data("1"), Sse::default().event("b").data("2")]
    );
}

#[test]
fn raw_multi_segment_buffers_are_read_whole() {
    use bytes::Buf as _;
    let one = |data: bytes::buf::Chain<Bytes, Bytes>| -> Vec<Sse> {
        let input = futures_util::stream::iter([Ok::<_, Infallible>(data)]);
        let mut s = Blocks::new(input, Caps::default(), Box::new(|_: &[u8]| {}));
        std::iter::from_fn(|| s.next().now_or_never().unwrap()).map(Result::unwrap).collect()
    };
    // [sse-stream] input.rs test_multi_segment_buf_from_byte_stream_not_truncated
    assert_eq!(one(Bytes::from_static(b"data: hel").chain(Bytes::from_static(b"lo\n\n"))), [data_only("hello")]);
    // test_crlf_split_across_segments_of_one_frame / test_bom_split_across_segments_of_one_frame
    assert_eq!(
        one(Bytes::from_static(b"data: hello\r").chain(Bytes::from_static(b"\ndata: world\n\n"))),
        [data_only("hello\nworld")]
    );
    assert_eq!(
        one(Bytes::from_static(b"\xEF\xBB").chain(Bytes::from_static(b"\xBFdata: hello\n\n"))),
        [data_only("hello")]
    );
}

#[tokio::test]
async fn raw_events_from_one_buffer_then_a_body_error() {
    // [sse-stream] input.rs test_multiple_events_retained_from_one_byte_buffer
    let input = futures_util::stream::iter([
        Ok(Bytes::from_static(b"data: one\n\ndata: two\n\n")),
        Err(std::io::Error::other("end-of-test error")),
    ]);
    let mut s = Blocks::new(input, Caps::default(), Box::new(|_: &[u8]| {}));
    assert_eq!(s.next().await.unwrap().unwrap(), data_only("one"));
    assert_eq!(s.next().await.unwrap().unwrap(), data_only("two"));
    assert!(matches!(s.next().await, Some(Err(Error::Body(_)))));
    assert!(s.next().await.is_none());
    // test_event_split_across_many_immediately_ready_fragments
    let mut chunks: Vec<&[u8]> = vec![b"data: "];
    chunks.extend(std::iter::repeat_n(&b"x"[..], 10_000));
    chunks.push(b"\n\n");
    assert_eq!(run(&chunks).blocks.iter().map(|b| b.data.as_deref().map(str::len)).collect::<Vec<_>>(), [Some(10_000)]);
}

#[tokio::test]
async fn raw_lifecycle_errors_and_eof_end_the_stream() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    // [sse-stream] lifecycle.rs parse_errors_end_both_decoders_for_every_chunk_width
    let input = b"data: before\ndata: \xff\n\ndata: after\n\n";
    for (w, out) in every_width(input) {
        assert!(out.blocks.is_empty(), "width={w}");
        assert!(out.error.is_some_and(|e| e.starts_with("utf8 parse error")), "width={w}");
    }
    // invalid_first_data_line_never_creates_an_empty_event
    for chunks in [vec![&b"data: \xff\n"[..], b"\n"], vec![&b"data: \xff"[..], b"\n", b"\n"]] {
        let out = run(&chunks);
        assert!(out.blocks.is_empty() && out.error.is_some());
    }
    // completed_events_precede_error_and_input_is_never_polled_again
    let polls = std::sync::Arc::new(AtomicUsize::new(0));
    let count = polls.clone();
    let input = futures_util::stream::poll_fn(move |_| {
        Poll::Ready(Some(match count.fetch_add(1, Ordering::Relaxed) {
            0 => Ok(Bytes::from_static(b"data: first\n\ndata: incomplete")),
            1 => Err(std::io::Error::other("transport failed")),
            _ => panic!("input polled after terminal error"),
        }))
    });
    let mut s = Blocks::new(input, Caps::default(), Box::new(|_: &[u8]| {}));
    assert_eq!(s.next().await.unwrap().unwrap(), data_only("first"));
    let err = s.next().await.unwrap().unwrap_err();
    assert_eq!(std::error::Error::source(&err).unwrap().to_string(), "transport failed");
    assert!(s.next().await.is_none());
    assert_eq!(polls.load(Ordering::Relaxed), 2);
    // partial_utf8_survives_pending_and_wakeup
    let mut chunks = [b"data: \xe4".as_slice(), b"\xbd\xa0\r", b"\n\r", b"\n"].into_iter();
    let mut pending = false;
    let input = futures_util::stream::poll_fn(move |cx| {
        pending = !pending;
        if pending {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(chunks.next().map(Ok::<_, Infallible>))
        }
    });
    let got: Vec<_> = Blocks::new(input, Caps::default(), Box::new(|_: &[u8]| {})).collect().await;
    assert_eq!(got.into_iter().map(Result::unwrap).collect::<Vec<_>>(), [data_only("你")]);
    // eof_discards_partial_data_and_fuses_non_fused_input
    let mut step = 0;
    let input = futures_util::stream::poll_fn(move |_| {
        step += 1;
        Poll::Ready(match step {
            1 => Some(Ok::<_, Infallible>(b"data: partial".as_slice())),
            2 => None,
            _ => panic!("input polled after EOF"),
        })
    });
    let mut s = Blocks::new(input, Caps::default(), Box::new(|_: &[u8]| {}));
    assert!(!s.is_terminated());
    assert!(s.next().await.is_none());
    assert!(s.is_terminated());
    assert!(s.next().await.is_none());
}

#[test]
fn raw_small_events_after_large_ones_have_compact_data() {
    // [sse-stream] test_data_capacity.rs small_events_after_large_events_have_compact_owned_data
    let large = "中文🙂é".repeat(4000);
    let mut input = format!("data: {large}\n\n");
    input.push_str("id: control\n\ndata:\n\n");
    for _ in 0..32 {
        input.push_str("data: x\ndata: y\n\n");
    }
    input += &["data: ", &large, "\r\n\r\ndata: tail\n\n"].concat();
    for width in [1, 4, 255, 1460, input.len()] {
        let b = run(&input.as_bytes().chunks(width).collect::<Vec<_>>()).blocks;
        assert_eq!(b.len(), 37);
        assert_eq!(b[0].data.as_deref(), Some(large.as_str()));
        assert_eq!(b[1], Sse::default().id("control"));
        assert_eq!(b[2].data.as_deref(), Some(""));
        assert_eq!(b[35].data.as_deref(), Some(large.as_str()));
        assert_eq!(b[36].data.as_deref(), Some("tail"));
        for e in &b[3..35] {
            assert_eq!(e.data.as_deref(), Some("x\ny"));
        }
        for i in (2..35).chain(std::iter::once(36)) {
            let cap = b[i].data.as_ref().unwrap().capacity();
            assert!(cap <= 64, "small output kept a large allocation: width={width}, index={i}, capacity={cap}");
        }
    }
    // varied_sizes_keep_complete_payloads
    let values: Vec<_> =
        [32_768, 4096, 8191, 8192, 8193, 512, 16_384, 80, 40_000].iter().map(|&n| "x".repeat(n)).collect();
    let input: String = values.iter().map(|v| format!("data: {v}\ndata: tail\n\n")).collect();
    for width in [10, 1460, input.len()] {
        let b = run(&input.as_bytes().chunks(width).collect::<Vec<_>>()).blocks;
        let want: Vec<_> = values.iter().map(|v| data_only(&format!("{v}\ntail"))).collect();
        assert_eq!(b, want, "width={width}");
    }
}

#[test]
fn raw_unicode_fields_and_utf8_errors() {
    // [sse-stream] test_utf8.rs unicode_fields_across_validation_and_input_boundaries
    for padding in [0, 63, 64, 255, 256, 4096] {
        let value = format!("{}中文🙂é", "x".repeat(padding));
        let input = format!("\u{feff}event: {value}\r\nid: {value}\r\ndata: {value}\r\ndata: tail\r\n\r\n");
        let want = Sse::default().event(value.clone()).id(value.clone()).data(format!("{value}\ntail"));
        for width in [1, 4, 63, 64, 255, 256, 1460, input.len()] {
            assert_eq!(
                run(&input.as_bytes().chunks(width).collect::<Vec<_>>()).blocks,
                std::slice::from_ref(&want),
                "{padding} {width}"
            );
        }
    }
    // invalid_utf8_keeps_standard_library_error_details (fewer offsets / widths)
    let invalid: &[&[u8]] = &[b"\xff", b"\x80", b"\xc0\xaf", b"\xed\xa0\x80", b"\xf4\x90\x80\x80", b"\xe4\xb8"];
    for field in ["data", "event", "id", "retry"] {
        for offset in [0, 255, 4095] {
            for bad in invalid {
                let mut value = vec![b'x'; 8192];
                value.splice(offset..offset + bad.len(), bad.iter().copied());
                let expected = std::str::from_utf8(&value).unwrap_err();
                let mut input = format!("{field}: ").into_bytes();
                input.extend_from_slice(&value);
                input.extend_from_slice(b"\r\n\r\n");
                for width in [1, 64, 1460, input.len()] {
                    let input = futures_util::stream::iter(input.chunks(width).map(Ok::<_, Infallible>));
                    let mut s = Blocks::new(input, Caps::default(), Box::new(|_: &[u8]| {}));
                    let Some(Err(Error::Utf8Parse(e))) = s.next().now_or_never().unwrap() else {
                        panic!("expected a UTF-8 error: {field} {offset} {width}");
                    };
                    assert_eq!((e.valid_up_to(), e.error_len()), (expected.valid_up_to(), expected.error_len()));
                }
            }
        }
    }
    // long_retry_whitespace_is_ignored
    let input = format!("retry: {}18446744073709551615{}\ndata: after\n\n", " ".repeat(256), " ".repeat(256));
    for width in [1, 255, input.len()] {
        assert_eq!(run(&input.as_bytes().chunks(width).collect::<Vec<_>>()).blocks, [data_only("after")]);
    }
}

#[test]
fn raw_multiline_keep_alive_comments_cannot_inject_events() {
    // [sse-stream] src/body/tests.rs multiline_keep_alive_comments_cannot_inject_events (the
    // frame its encoder writes for the comment "hello\r\ndata: injected\n\n")
    let out = run(&[b": hello\n: data: injected\n: \n: \n\n"]);
    assert!(out.blocks.is_empty());
    assert_eq!(out.comments, [" hello", " data: injected", " ", " "]);
}

/// Default SSE caps enforce the 16 MiB line and event bounds.
#[test]
fn default_caps_enforce_16_mib_line_and_event_limits() {
    let def = Caps::default();
    assert_eq!(def.line, 16 * 1024 * 1024);
    assert_eq!(def.event, 16 * 1024 * 1024);

    // One line just over 16 MiB fails with LineTooLong(DEFAULT_CAP).
    let mut over_line = b"data: ".to_vec();
    over_line.resize(DEFAULT_CAP + 1, b'a');
    over_line.extend_from_slice(b"\n\n");
    let out_line = run(&[&over_line]);
    assert_eq!(out_line.error, Some(format!("an SSE line is longer than {DEFAULT_CAP} bytes")));

    // Multi-line event whose lines stay under 16 MiB each, but whose total exceeds 16 MiB.
    let mut chunk = b"data: ".to_vec();
    chunk.resize(4 * 1024 * 1024, b'b');
    chunk.push(b'\n');
    let chunks: Vec<&[u8]> = vec![&chunk, &chunk, &chunk, &chunk, b"data: extra\n\n"];
    let out_event = run(&chunks);
    assert_eq!(out_event.error, Some(format!("an SSE event is larger than {DEFAULT_CAP} bytes")));
}
