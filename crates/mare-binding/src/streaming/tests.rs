use super::*;
use std::cell::Cell;

#[test]
fn bytes_are_accumulated_in_order_and_clear_and_take_reset_them() {
    let mut buffer = TextBuffer::new();
    assert!(buffer.is_empty());
    let unicode = "a日本🙂z".as_bytes();
    for b in unicode {
        buffer.append(std::slice::from_ref(b));
    }
    buffer.append(&[0, 0xff]);
    assert_eq!(buffer.len(), unicode.len() + 2);
    assert_eq!(buffer.as_bytes(), [unicode, &[0, 0xff]].concat());
    assert_eq!(buffer.take(), [unicode, &[0, 0xff]].concat());
    assert!(buffer.is_empty());
    buffer.append(b"discard");
    buffer.clear();
    buffer.append(b"next");
    assert_eq!(buffer.as_bytes(), b"next");
    let mut long = TextBuffer::new();
    for _ in 0..8192 {
        long.append(b"abcdefghijklmnop");
    }
    assert_eq!(long.len(), 131_072);
    assert_eq!(long.as_bytes(), b"abcdefghijklmnop".repeat(8192));
}

#[test]
fn tail_bounds_lines_and_bytes_without_splitting_utf8() {
    let mut buf = TextBuffer::new();
    assert_eq!(buf.tail(5, 1024), b"");
    buf.append(b"line1\nline2\nline3\nline4\n");
    assert_eq!(buf.tail(0, 1024), b"");
    assert_eq!(buf.tail(5, 0), b"");
    assert_eq!(buf.tail(2, 1024), b"line3\nline4\n");
    assert_eq!(buf.tail(10, 1024), b"line1\nline2\nline3\nline4\n");

    let mut no_trailing_nl = TextBuffer::new();
    no_trailing_nl.append(b"a\nb\nc");
    assert_eq!(no_trailing_nl.tail(2, 1024), b"b\nc");

    // Multi-byte UTF-8 ("日" is 3 bytes: E6 97 A5): slicing mid-character advances to next char.
    let mut utf8 = TextBuffer::new();
    utf8.append("prefix-日本🙂tail".as_bytes());
    let sliced = utf8.tail(10, 10);
    assert!(std::str::from_utf8(sliced).is_ok(), "tail must not split UTF-8: {sliced:?}");
    assert!(sliced.ends_with(b"tail"));
}

#[test]
fn thresholds_key_changes_and_empty_flush_follow_event_driven_semantics() {
    let mut c = DeltaCoalescer::<&str>::new();
    let calls = Cell::new(0);
    assert!(
        c.flush(|| {
            calls.set(calls.get() + 1);
            99.0
        })
        .is_none()
    );
    assert_eq!(calls.get(), 0, "no pending event must not sample the clock");
    c.append(DeltaKind::Text, None, b"a").unwrap();
    assert!(!c.due(49.999));
    assert!(c.due(50.0));
    assert!(c.due(51.0));
    let first = c.flush(|| 50.5).unwrap();
    assert_eq!(first, Delta { kind: DeltaKind::Text, id: None, bytes: b"a".to_vec() });
    c.append(DeltaKind::Text, None, b"").unwrap();
    c.append(DeltaKind::Text, None, b"b").unwrap();
    let changed = c.append(DeltaKind::ToolCall, Some("x"), b"should-not-append").unwrap_err();
    assert_eq!(changed.id, Some("x"));
    assert!(!c.due(100.499));
    assert!(c.due(100.5));
    let second = c.flush(|| 101.0).unwrap();
    assert_eq!(second.bytes, b"b");
    c.append(DeltaKind::ToolCall, changed.id, b"c").unwrap();
    assert!(!c.due(150.999));
    assert!(c.due(151.0));
    let final_delta = c.flush(|| 151.2).unwrap();
    assert_eq!(final_delta, Delta { kind: DeltaKind::ToolCall, id: Some("x"), bytes: b"c".to_vec() });
    assert!(!c.has_pending());
}

#[derive(Clone, Debug)]
struct Input {
    time: f64,
    kind: &'static str,
    id: Option<&'static str>,
    data: &'static str,
}

type Output = (String, Option<String>, Option<Vec<u8>>);

fn delta_kind(s: &str) -> Option<DeltaKind> {
    match s {
        "text_delta" => Some(DeltaKind::Text),
        "reasoning_delta" => Some(DeltaKind::Reasoning),
        "tool_call_delta" => Some(DeltaKind::ToolCall),
        _ => None,
    }
}

fn kind_name(kind: DeltaKind) -> &'static str {
    match kind {
        DeltaKind::Text => "text_delta",
        DeltaKind::Reasoning => "reasoning_delta",
        DeltaKind::ToolCall => "tool_call_delta",
    }
}

fn push_flush(c: &mut DeltaCoalescer<String>, clock: &mut impl FnMut() -> f64, out: &mut Vec<Output>) {
    if let Some(d) = c.flush(clock) {
        out.push((kind_name(d.kind).into(), d.id, Some(d.bytes)));
    }
}

/// The Lua reference below mirrors the existing agent sink; this driver uses only the new API.
fn run_rust(inputs: &[Input]) -> (Vec<Output>, usize) {
    let mut c = DeltaCoalescer::<String>::new();
    let mut out = Vec::new();
    let calls = Cell::new(0usize);
    let mut time = 0.0;
    for input in inputs {
        time = input.time;
        let tick = Cell::new(0usize);
        let mut clock = || {
            let n = tick.get();
            tick.set(n + 1);
            calls.set(calls.get() + 1);
            time + n as f64 * 0.125
        };
        if input.kind == "pause" {
            continue;
        }
        if let Some(kind) = delta_kind(input.kind) {
            let id = input.id.map(str::to_string);
            if let Err(changed) = c.append(kind, id, input.data.as_bytes()) {
                push_flush(&mut c, &mut clock, &mut out);
                c.append(kind, changed.id, input.data.as_bytes()).unwrap();
            }
            if c.due(clock()) {
                push_flush(&mut c, &mut clock, &mut out);
            }
        } else {
            push_flush(&mut c, &mut clock, &mut out);
            out.push((input.kind.into(), input.id.map(str::to_string), Some(input.data.as_bytes().to_vec())));
        }
    }
    let mut clock = || {
        calls.set(calls.get() + 1);
        time + 1.0
    };
    push_flush(&mut c, &mut clock, &mut out);
    (out, calls.get())
}

fn run_lua_reference(inputs: &[Input]) -> (Vec<Output>, usize) {
    let lua = mlua::Lua::new();
    let events = lua.create_table().unwrap();
    for (i, input) in inputs.iter().enumerate() {
        let e = lua.create_table().unwrap();
        e.set("time", input.time).unwrap();
        e.set("type", input.kind).unwrap();
        e.set("id", input.id).unwrap();
        e.set("delta", input.data).unwrap();
        events.set(i + 1, e).unwrap();
    }
    let reference = lua
        .load(
            r#"
return function(events)
    local pend, last_emit = nil, 0
    local out, now, tick, calls = {}, 0, 0, 0
    local function clock()
        local v = now + tick * 0.125
        tick, calls = tick + 1, calls + 1
        return v
    end
    local function emit(e) out[#out + 1] = e end
    local function flush()
        if pend then
            local ev = pend
            pend = nil
            last_emit = clock()
            emit(ev)
        end
    end
    for _, ev in ipairs(events) do
        now, tick = ev.time, 0
        local t = ev.type
        if t == "pause" then
            -- advancing time alone does not flush
        elseif t == "text_delta" or t == "reasoning_delta" or t == "tool_call_delta" then
            if pend and (pend.type ~= t or pend.id ~= ev.id) then flush() end
            if not pend then pend = { type = t, id = ev.id, delta = "" } end
            pend.delta = pend.delta .. (ev.delta or "")
            if clock() - last_emit >= 50 then flush() end
        else
            flush()
            emit(ev)
        end
    end
    now, tick = now + 1, 0
    flush()
    return out, calls
end
"#,
        )
        .eval::<mlua::Function>()
        .unwrap();
    let (rows, calls): (mlua::Table, usize) = reference.call(events).unwrap();
    let out = rows
        .sequence_values::<mlua::Table>()
        .map(|e| {
            let e = e.unwrap();
            (
                e.get("type").unwrap(),
                e.get("id").unwrap(),
                e.get::<Option<mlua::LuaString>>("delta").unwrap().map(|d| d.as_bytes().to_vec()),
            )
        })
        .collect();
    (out, calls)
}

#[test]
fn ordered_events_match_the_actual_lua_coalescing_algorithm() {
    let cases = [
        Input { time: 100.0, kind: "text_delta", id: None, data: "a" },
        Input { time: 101.0, kind: "text_delta", id: None, data: "b" },
        Input { time: 102.0, kind: "text_delta", id: None, data: "c" },
        Input { time: 103.0, kind: "tool_call_start", id: Some("x"), data: "start" },
        Input { time: 104.0, kind: "tool_call_delta", id: Some("x"), data: "{" },
        Input { time: 105.0, kind: "tool_call_delta", id: Some("y"), data: "{" },
        Input { time: 106.0, kind: "error", id: None, data: "failure" },
        Input { time: 107.0, kind: "reasoning_delta", id: None, data: "h" },
        Input { time: 400.0, kind: "pause", id: None, data: "" },
        Input { time: 401.0, kind: "text_delta", id: None, data: "end" },
    ];
    let (actual, calls) = run_rust(&cases);
    assert_eq!((actual.clone(), calls), run_lua_reference(&cases));
    let expected = vec![
        ("text_delta", None, "a"),
        ("text_delta", None, "bc"),
        ("tool_call_start", Some("x"), "start"),
        ("tool_call_delta", Some("x"), "{"),
        ("tool_call_delta", Some("y"), "{"),
        ("error", None, "failure"),
        ("reasoning_delta", None, "h"),
        ("text_delta", None, "end"),
    ]
    .into_iter()
    .map(|(t, id, d)| (t.to_string(), id.map(str::to_string), Some(d.as_bytes().to_vec())))
    .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn repeated_empty_delta_nil_and_interleaved_ids_match_lua() {
    let mut inputs = Vec::new();
    for i in 0..2000 {
        let kind = match i % 13 {
            0 => "tool_call_start",
            1..=4 => "text_delta",
            5..=8 => "tool_call_delta",
            9..=11 => "reasoning_delta",
            _ => "pause",
        };
        let id = match i % 3 {
            0 => None,
            1 => Some("a"),
            _ => Some("b"),
        };
        let data = if i % 5 == 0 { "" } else { "x日" };
        inputs.push(Input { time: i as f64 * 1.125, kind, id, data });
    }
    assert_eq!(run_rust(&inputs), run_lua_reference(&inputs));
}

#[test]
fn independent_buffers_do_not_mix_pending_deltas() {
    let (mut a, mut b) = (DeltaCoalescer::<String>::new(), DeltaCoalescer::<String>::new());
    a.append(DeltaKind::Text, None, b"a").unwrap();
    b.append(DeltaKind::Text, None, b"b").unwrap();
    assert_eq!(a.flush(|| 1.0).unwrap().bytes, b"a");
    assert_eq!(b.flush(|| 2.0).unwrap().bytes, b"b");
}

#[test]
fn pending_data_precedes_control_events_and_final_drain_happens_once() {
    for marker in ["end", "error", "cancel", "retry"] {
        let events = [
            Input { time: 1.0, kind: "text_delta", id: None, data: "partial" },
            Input { time: 2.0, kind: marker, id: None, data: "control" },
        ];
        let (actual, calls) = run_rust(&events);
        assert_eq!((actual.clone(), calls), run_lua_reference(&events));
        assert_eq!(
            actual,
            vec![
                ("text_delta".into(), None, Some(b"partial".to_vec())),
                (marker.into(), None, Some(b"control".to_vec())),
            ]
        );
    }
    let final_only = [Input { time: 1.0, kind: "text_delta", id: None, data: "tail" }];
    let (actual, calls) = run_rust(&final_only);
    assert_eq!((actual.clone(), calls), run_lua_reference(&final_only));
    assert_eq!(actual, vec![("text_delta".into(), None, Some(b"tail".to_vec()))]);
}
