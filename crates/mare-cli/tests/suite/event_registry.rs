//! Event-registry sync checks:
//! - Every literal kernel/bus emit site in Lua (`crates/mare-plugin/preset/**`) and Rust
//!   (`crates/*/src/**`) must be declared in `protocol_events!` or built-in service contracts.
//! - Every `protocol_events!` declaration must have at least one scanned emit site (or a
//!   `host_emitted` Rust site verified on disk).
//! - Dynamic emit sites are enumerated explicitly with reasons.
//! - Generated `docs/{en,ja,zh}/reference/events.md` documents every registered public event.

use crate::common::ChildCommandExt;
use mare_protocol::event_registry::PROTOCOL_EVENTS;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn walk_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk_files(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

fn read_first_arg(bytes: &[u8], open_paren: usize) -> Option<String> {
    let mut i = open_paren + 1;
    let len = bytes.len();
    while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut brace = 0usize;
    let mut in_quote: Option<u8> = None;
    let mut escaped = false;
    while i < len {
        let b = bytes[i];
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_quote = Some(b),
            b'(' => paren += 1,
            b')' => {
                if paren == 0 && bracket == 0 && brace == 0 {
                    break;
                }
                paren = paren.saturating_sub(1);
            }
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => brace = brace.saturating_sub(1),
            b',' if paren == 0 && bracket == 0 && brace == 0 => break,
            _ => {}
        }
        i += 1;
    }
    String::from_utf8(bytes[start..i].to_vec()).ok().map(|s| s.trim().to_string())
}

fn parse_string_literal(arg: &str, allow_single_quote: bool) -> Option<String> {
    let bytes = arg.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let q = bytes[0];
    if !(q == b'"' || (allow_single_quote && q == b'\'')) || bytes[bytes.len() - 1] != q {
        return None;
    }
    let mut i = 1;
    let mut out = String::new();
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 2 >= bytes.len() {
                return None;
            }
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if bytes[i] == q {
            // Quote closed early (e.g. `"agent/" .. rec.kind`), so not a pure literal.
            return None;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Some(out)
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[derive(Default)]
struct ScanResult {
    literals: BTreeMap<String, BTreeSet<String>>,
    dynamic: Vec<(String, String)>,
}

fn scan_lua_bus_sites(root: &Path, out: &mut ScanResult) {
    let preset = root.join("crates/mare-plugin/preset");
    let mut files = Vec::new();
    walk_files(&preset, "lua", &mut files);
    files.sort();
    let methods = ["emit_checked", "emit", "bail", "waterfall_checked", "waterfall"];
    for file in files {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if rel.contains("/tests/") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let bytes = text.as_bytes();
        let mut idx = 0;
        while let Some(pos) = text[idx..].find("ctx.bus") {
            let abs = idx + pos;
            idx = abs + "ctx.bus".len();
            // Skip Lua line comments.
            let line_start = text[..abs].rfind('\n').map_or(0, |p| p + 1);
            if text[line_start..abs].contains("--") {
                continue;
            }
            let mut p = idx;
            while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
            if p >= bytes.len() || bytes[p] != b':' {
                continue;
            }
            p += 1;
            while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
            let Some(method) = methods
                .iter()
                .find(|m| text[p..].starts_with(**m) && bytes.get(p + m.len()).is_none_or(|&b| !is_ident_char(b)))
            else {
                continue;
            };
            p += method.len();
            while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
            if p >= bytes.len() || bytes[p] != b'(' {
                continue;
            }
            let Some(arg) = read_first_arg(bytes, p) else { continue };
            if let Some(name) = parse_string_literal(&arg, true) {
                out.literals.entry(name).or_default().insert(rel.clone());
            } else {
                out.dynamic.push((rel.clone(), format!("ctx.bus:{method}({arg})")));
            }
        }
    }
}

fn scan_rust_emit_sites(root: &Path, out: &mut ScanResult) {
    let crates_dir = root.join("crates");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates_dir).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            walk_files(&src, "rs", &mut files);
        }
    }
    files.sort();
    let methods = ["emit_json", "emit_checked", "emit", "bail", "waterfall_checked", "waterfall"];
    for file in files {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if rel.ends_with("_tests.rs")
            || rel.ends_with("/testkit.rs")
            || rel.contains("/markdown/")
            || rel.ends_with("crates/mare-shared/src/ansi.rs")
        {
            continue;
        }
        let full = std::fs::read_to_string(&file).unwrap();
        let text = match full.find("\n#[cfg(test)]\nmod ").or_else(|| full.find("#[cfg(test)]")) {
            Some(cut) => &full[..cut],
            None => full.as_str(),
        };
        let bytes = text.as_bytes();
        for method in methods {
            let needle = format!(".{method}");
            let mut idx = 0;
            while let Some(pos) = text[idx..].find(&needle) {
                let abs = idx + pos;
                idx = abs + needle.len();
                let mut p = idx;
                if bytes.get(p).is_some_and(|&b| is_ident_char(b)) {
                    continue;
                }
                while p < bytes.len() && bytes[p].is_ascii_whitespace() {
                    p += 1;
                }
                if bytes.get(p) != Some(&b'(') {
                    continue;
                }
                // Check receiver before `.` to skip non-event helpers or string methods if any.
                let mut r_end = abs;
                while r_end > 0 && bytes[r_end - 1].is_ascii_whitespace() {
                    r_end -= 1;
                }
                if r_end == 0 {
                    continue;
                }
                // Skip string comments `//` on same line.
                let line_start = text[..abs].rfind('\n').map_or(0, |k| k + 1);
                if text[line_start..abs].trim_start().starts_with("//") {
                    continue;
                }
                let Some(arg) = read_first_arg(bytes, p) else { continue };
                if let Some(name) = parse_string_literal(&arg, false) {
                    out.literals.entry(name).or_default().insert(rel.clone());
                } else {
                    out.dynamic.push((rel.clone(), format!(".{method}({arg})")));
                }
            }
        }
    }
}

/// Dynamic emit calls where the event name is computed at runtime rather than a literal.
const DYNAMIC_EMIT_SITES: &[(&str, &str, &str)] = &[
    (
        "crates/mare-kernel/src/dispatch/events.rs",
        ".emit(event)",
        "Kernel::emit_json forwards dynamic event name to kernel emit",
    ),
    (
        "crates/mare-kernel/src/dispatch/events.rs",
        ".bail(event)",
        "Kernel::bail_json forwards dynamic event name to kernel bail",
    ),
    (
        "crates/mare-kernel/src/plugins/prims.rs",
        ".emit(event)",
        "ctx.bus native dispatch primitive forwards caller event name",
    ),
    (
        "crates/mare-kernel/src/plugins/prims.rs",
        ".emit_checked(event)",
        "ctx.bus native dispatch primitive forwards caller event name",
    ),
    (
        "crates/mare-kernel/src/plugins/prims.rs",
        ".bail(event)",
        "ctx.bus native dispatch primitive forwards caller event name",
    ),
    (
        "crates/mare-kernel/src/plugins/prims.rs",
        ".waterfall(event)",
        "ctx.bus native dispatch primitive forwards caller event name",
    ),
    (
        "crates/mare-kernel/src/plugins/prims.rs",
        ".waterfall_checked(event)",
        "ctx.bus native dispatch primitive forwards caller event name",
    ),
    ("crates/mare-host/src/frontend.rs", ".emit_json(event)", "Host::emit_json forwards dynamic event name to kernel"),
    ("crates/mare-cli/src/app/mod.rs", ".emit_json(event)", "App::emit wrapper dispatches internal TUI events"),
    (
        "crates/mare-cli/src/app/input/keys.rs",
        ".emit(event)",
        "TUI editor scrolls dispatch internal input/scroll-* action names",
    ),
    (
        "crates/mare-cli/src/app/input/submit.rs",
        ".emit(event)",
        "TUI submit dispatches internal input/submit or input/steer",
    ),
];

#[test]
fn event_registry_emitters_match_declarations() {
    let root = repo_root();
    let mut scan = ScanResult::default();
    scan_lua_bus_sites(&root, &mut scan);
    scan_rust_emit_sites(&root, &mut scan);

    let mut registry_names: BTreeSet<String> = PROTOCOL_EVENTS.iter().map(|d| d.name.to_string()).collect();
    let lua = mlua::Lua::new();
    let contracts_val: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let parsed_contracts = mare_kernel::contracts::parse_contracts(&lua, contracts_val).unwrap();
    for contract in parsed_contracts.values() {
        for spec in contract.events.values() {
            registry_names.insert(spec.name.clone());
        }
    }
    // Check (a): every emitted literal event must be declared in protocol_events! or contracts.
    let mut undeclared = Vec::new();
    for (name, sites) in &scan.literals {
        if !registry_names.contains(name.as_str()) {
            undeclared.push((name.clone(), sites.iter().cloned().collect::<Vec<_>>()));
        }
    }
    assert!(
        undeclared.is_empty(),
        "emitted event(s) not declared in protocol_events! or service contracts: {undeclared:?}"
    );

    // Check (b): every declaration in protocol_events! must have at least one emit site or a
    // verified host_emitted site.
    let mut unemitted = Vec::new();
    for def in PROTOCOL_EVENTS {
        let emitted_sites = scan.literals.get(def.name);
        if let Some(host_site) = def.host_emitted {
            assert!(
                emitted_sites.is_some_and(|sites| sites.contains(host_site)),
                "host_emitted site `{host_site}` for `{}` does not contain a literal emit for it (found: {:?})",
                def.name,
                emitted_sites
            );
            continue;
        }
        if emitted_sites.is_none() {
            unemitted.push(def.name);
        }
    }
    assert!(
        unemitted.is_empty(),
        "declared event(s) in protocol_events! have no emit site and no host_emitted annotation: {unemitted:?}"
    );

    // Every dynamic emit site must be accounted for with a reason.
    let mut unmatched_dynamic = Vec::new();
    for (file, call) in &scan.dynamic {
        let matched = DYNAMIC_EMIT_SITES.iter().any(|(f, pat, _reason)| f == file && call == pat);
        if !matched {
            unmatched_dynamic.push((file.clone(), call.clone()));
        }
    }
    assert!(
        unmatched_dynamic.is_empty(),
        "undocumented dynamic emit site(s) found (document in DYNAMIC_EMIT_SITES or emit a literal): {unmatched_dynamic:?}"
    );
    for (expected_file, expected_pat, _reason) in DYNAMIC_EMIT_SITES {
        assert!(
            scan.dynamic.iter().any(|(f, call)| f == expected_file && call == expected_pat),
            "stale DYNAMIC_EMIT_SITES entry: ({expected_file}, {expected_pat})"
        );
    }
}

#[test]
fn event_registry_docs_cover_every_declared_event() {
    let root = repo_root();
    for lang in ["en", "ja", "zh"] {
        let events_doc = root.join(format!("docs/{lang}/reference/events.md"));
        let text = std::fs::read_to_string(&events_doc)
            .unwrap_or_else(|e| panic!("missing generated {}: {e}", events_doc.display()));
        for def in PROTOCOL_EVENTS {
            assert!(
                text.contains(&format!("### `{}`", def.name)),
                "{} is missing heading for `{}`",
                events_doc.display(),
                def.name
            );
        }
        for page in [
            root.join(format!("docs/{lang}/integrate/json-stream.md")),
            root.join(format!("docs/{lang}/extend/services-and-events.md")),
        ] {
            let content = std::fs::read_to_string(&page).unwrap();
            assert!(
                content.contains("../reference/events.md"),
                "{} must link to ../reference/events.md",
                page.display()
            );
        }
    }
}

/// `contracts.<root>.events` enforces metadata, naming, schema, `lossy`/`retain`,
/// `scope = "turn"`, `components` (no `$ref`), and example checks, rejecting every invalid variant.
#[test]
fn contract_events_validation_rejects_invalid_declarations() {
    let lua = mlua::Lua::new();

    // Valid event-only root + service root using expanded `components` and every retain/mode shape.
    let valid: mlua::Value = lua
        .load(
            r#"return {
                widget = {
                    methods = {
                        ping = { doc = "ping", expose = "both", result = { type = "boolean" } },
                    },
                    events = {
                        ["state-changed"] = {
                            doc = "Widget state changed.",
                            mode = "emit",
                            expose = "both",
                            scope = "turn",
                            delivery = "lossy",
                            ignorable = false,
                            retain = "last",
                            on_error = "continue",
                            data = {
                                type = "object",
                                properties = {
                                    turn_id = { type = "string" },
                                    count = { type = "integer" },
                                },
                                required = { "turn_id", "count" },
                                additionalProperties = false,
                            },
                            example = { turn_id = "t-1", count = 2 },
                        },
                        ["item-started"] = {
                            doc = "Keyed item started.",
                            mode = "waterfall",
                            expose = "lua",
                            retain = { key = "item_id", cleared_by = "widget/item-completed" },
                            data = {
                                type = "object",
                                properties = { item_id = { type = "string" } },
                                required = { "item_id" },
                            },
                            result = { type = "object" },
                        },
                        ["item-completed"] = {
                            mode = "parallel",
                            expose = "process",
                        },
                    },
                },
                ["widget-extra"] = {
                    owner = "widget",
                    events = {
                        updated = {
                            doc = "External event-only root.",
                            expose = "both",
                            delivery = "required",
                            ignorable = true,
                            data = { type = "object", properties = { ok_count = { type = "integer" } } },
                            example = { ok_count = 1 },
                        },
                    },
                },
            }"#,
        )
        .eval()
        .unwrap();
    let parsed = mare_kernel::contracts::parse_contracts(&lua, valid).unwrap();
    assert!(parsed["widget"].events.contains_key("state-changed"));
    assert_eq!(parsed["widget-extra"].owner.as_deref(), Some("widget"));
    assert!(parsed["widget-extra"].events.contains_key("updated"));

    let invalid_cases = [
        ("underscore event name", "return { s = { owner = 'svc', events = { bad_name = {} } } }"),
        ("colon event name", "return { s = { owner = 'svc', events = { ['bad:name'] = {} } } }"),
        ("trailing start", "return { s = { owner = 'svc', events = { ['job-start'] = {} } } }"),
        ("trailing end", "return { s = { owner = 'svc', events = { ['job-end'] = {} } } }"),
        ("trailing done", "return { s = { owner = 'svc', events = { ['job-done'] = {} } } }"),
        ("trailing finished", "return { s = { owner = 'svc', events = { ['job-finished'] = {} } } }"),
        ("changed without retain", "return { s = { owner = 'svc', events = { ['state-changed'] = {} } } }"),
        (
            "lossy without retain or ignorable",
            "return { s = { owner = 'svc', events = { pinged = { expose = 'both', doc = 'd', delivery = 'lossy', ignorable = false, data = { type = 'object' }, example = {} } } } }",
        ),
        (
            "expose both missing example",
            "return { s = { owner = 'svc', events = { pinged = { expose = 'both', doc = 'd', delivery = 'required', ignorable = true, data = { type = 'object' } } } } }",
        ),
        (
            "example fails schema",
            "return { s = { owner = 'svc', events = { pinged = { expose = 'both', doc = 'd', delivery = 'required', ignorable = true, data = { type = 'object', properties = { n = { type = 'integer' } }, required = { 'n' } }, example = { n = 'nope' } } } } }",
        ),
        (
            "turn scope without turn_id",
            "return { s = { owner = 'svc', events = { pinged = { scope = 'turn', expose = 'both', doc = 'd', delivery = 'required', ignorable = true, data = { type = 'object', properties = { x = { type = 'string' } } }, example = { x = 'a' } } } } }",
        ),
        (
            "result on emit mode",
            "return { s = { owner = 'svc', events = { pinged = { mode = 'emit', result = { type = 'string' } } } } }",
        ),
        (
            "$ref in data schema",
            "return { s = { owner = 'svc', events = { pinged = { data = { type = 'object', properties = { x = { ['$ref'] = '#/$defs/Foo' } } } } } } }",
        ),
        (
            "camelCase payload field",
            "return { s = { owner = 'svc', events = { pinged = { data = { type = 'object', properties = { callId = { type = 'string' } } } } } } }",
        ),
        (
            "forbidden ok field",
            "return { s = { owner = 'svc', events = { pinged = { data = { type = 'object', properties = { ok = { type = 'boolean' } } } } } } }",
        ),
        (
            "underscore-prefixed field",
            "return { s = { owner = 'svc', events = { pinged = { data = { type = 'object', properties = { _hidden = { type = 'string' } } } } } } }",
        ),
        (
            "retain.key missing from data",
            "return { s = { owner = 'svc', events = { pinged = { retain = { key = 'missing', cleared_by = 's/other' }, data = { type = 'object', properties = { present = { type = 'string' } } } } } } }",
        ),
        ("unknown field on event", "return { s = { owner = 'svc', events = { pinged = { bogon = 1 } } } }"),
        (
            "affirmative on non-policy event",
            "return { s = { owner = 'svc', events = { pinged = { mode = 'emit', affirmative = true } } } }",
        ),
        (
            "join_ask_reasons on non-policy event",
            "return { s = { owner = 'svc', events = { pinged = { mode = 'emit', join_ask_reasons = true } } } }",
        ),
        (
            "on_error on policy event",
            "return { s = { owner = 'svc', events = { pinged = { mode = 'policy', on_error = 'abort' } } } }",
        ),
        (
            "on_error on rewrite event",
            "return { s = { owner = 'svc', events = { pinged = { mode = 'rewrite', on_error = 'continue' } } } }",
        ),
    ];
    for (label, code) in invalid_cases {
        let value: mlua::Value = lua.load(code).eval().unwrap();
        let res = mare_kernel::contracts::parse_contracts(&lua, value);
        assert!(res.is_err(), "C1 expected rejection for `{label}`");
    }
}

/// Conflict, reserved root, `owner`, and third-party unload/reload cleanup checks.
#[test]
fn contract_events_conflicts_reserved_roots_and_plugin_lifecycle() {
    let lua = mlua::Lua::new();

    // Missing owner on event-only root and protocol-reserved roots must fail.
    for code in [
        "return { lonely = { events = { pinged = {} } } }",
        "return { turn = { owner = 'agent', events = { custom = {} } } }",
        "return { step = { owner = 'agent', events = { custom = {} } } }",
        "return { item = { owner = 'agent', events = { custom = {} } } }",
        "return { agent = { methods = {}, events = { start = {} } } }",
        "return { status = { owner = 'other', events = { pinged = {} } } }",
    ] {
        let val: mlua::Value = lua.load(code).eval().unwrap();
        assert!(mare_kernel::contracts::parse_contracts(&lua, val).is_err(), "expected failure: {code}");
    }

    // Kernel rejects event-only contract naming an unknown owner service at startup.
    let kernel = mare_kernel::Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
    let bad_contracts: mlua::Table =
        kernel.lua().load("return { ghost = { owner = 'no-such-service', events = { pinged = {} } } }").eval().unwrap();
    assert!(kernel.install_service_contracts(bad_contracts).is_err());

    // Third-party plugin loading: reserved protocol roots and conflicts with built-in events fail,
    // while valid custom events enter `list_events` while loaded and disappear on unload/set_defs.
    let ok_builtin: mlua::Table = kernel
        .lua()
        .load("return { custom = { methods = {}, events = { pinged = { doc = 'p', expose = 'both', delivery = 'required', ignorable = true, data = { type = 'object' }, example = {} } } } }")
        .eval()
        .unwrap();
    kernel.install_service_contracts(ok_builtin).unwrap();
    assert!(kernel.list_events().iter().any(|e| e.name == "custom/pinged"));

    let dir = tempfile::tempdir().unwrap();
    let p_reserved = dir.path().join("bad_reserved.lua");
    std::fs::write(
        &p_reserved,
        "return { name = 'bad-res', contracts = { status = { owner = 'badsvc', events = { foo = {} } } } }",
    )
    .unwrap();
    let err_reserved = kernel
        .load_def(&mare_kernel::PluginSource {
            name_hint: "bad-res".into(),
            path: Some(p_reserved),
            code: None,
            origin: mare_kernel::Origin::User,
        })
        .err()
        .expect("third-party cannot use protocol-reserved event root");
    assert!(err_reserved.to_string().contains("reserved"), "{err_reserved}");

    let p_valid = dir.path().join("tp_ok.lua");
    std::fs::write(
        &p_valid,
        "return { name = 'tp-ok', contracts = { tpsvc = { methods = {}, events = { pulsed = { doc = 'tp', expose = 'both', delivery = 'required', ignorable = true, data = { type = 'object', properties = { seq_n = { type = 'integer' } } }, example = { seq_n = 1 } } } } } }",
    )
    .unwrap();
    let def_valid = kernel
        .load_def(&mare_kernel::PluginSource {
            name_hint: "tp-ok".into(),
            path: Some(p_valid),
            code: None,
            origin: mare_kernel::Origin::User,
        })
        .unwrap();
    let p_dup = dir.path().join("tp_dup.lua");
    std::fs::write(
        &p_dup,
        "return { name = 'tp-dup', contracts = { ['custom'] = { owner = 'tpsvc', events = { pinged = {} } } } }",
    )
    .unwrap();
    // Third-party plugin declaring a built-in contract root is already refused cleanly.
    assert!(
        kernel
            .load_def(&mare_kernel::PluginSource {
                name_hint: "tp-dup".into(),
                path: Some(p_dup),
                code: None,
                origin: mare_kernel::Origin::User,
            })
            .is_err()
    );

    kernel.set_defs(vec![def_valid]);
    assert!(kernel.list_events().iter().any(|e| e.name == "tpsvc/pulsed"));
    // Unload plugin -> event disappears from runtime `list_events()`.
    kernel.set_defs(vec![]);
    assert!(!kernel.list_events().iter().any(|e| e.name == "tpsvc/pulsed"));
}

/// `components` injection + live `events.list` RPC merge.
#[test]
fn contract_events_luals_components_and_live_events_list() {
    // 1. Protocol `components` have no `$ref`/`$schema` and are injected as `(S, C)` into built-in contracts.
    let components = mare_protocol::protocol_components();
    let raw = components.to_string();
    assert!(!raw.contains("\"$ref\""), "components must not contain $ref: {raw}");
    assert!(!raw.contains("\"$schema\""), "components must not contain $schema: {raw}");
    for key in ["attachment", "content_part", "tool_result_block", "tool_result_content", "error"] {
        assert!(components.get(key).is_some(), "missing protocol component `{key}`");
    }
    let lua = mlua::Lua::new();
    let comp_expr = mare_kernel::contracts::protocol_components_lua();
    let comp_contract: mlua::Value = lua
        .load(format!(
            "local C = {comp_expr}\n\
             return {{ comp = {{ owner = 'agent', events = {{ attached = {{ doc = 'd', expose = 'both', delivery = 'required', ignorable = true, data = {{ type = 'object', properties = {{ file = C.attachment, err = C.error }}, required = {{ 'file' }}, additionalProperties = false }}, example = {{ file = {{ path = 'a.txt' }} }} }} }} }} }}"
        ))
        .eval()
        .unwrap();
    let comp_parsed = mare_kernel::contracts::parse_contracts(&lua, comp_contract).unwrap();
    assert!(comp_parsed["comp"].events["attached"].data["properties"]["file"]["properties"]["path"].is_object());

    // 2. `luals_events` reflects changes immediately (counter-test) and generates `mare.EventEnvelope` + overloads.
    let render = |extra_field: &str| {
        let lua = mlua::Lua::new();
        let val: mlua::Value = lua
            .load(format!(
                "return {{ demo = {{ owner = 'agent', events = {{ pinged = {{ doc = 'd', expose = 'both', delivery = 'required', ignorable = true, data = {{ type = 'object', properties = {{ {extra_field} = {{ type = 'string' }} }} }}, example = {{}} }} }} }} }}"
            ))
            .eval()
            .unwrap();
        let parsed = mare_kernel::contracts::parse_contracts(&lua, val).unwrap();
        mare_kernel::contracts::luals_events(&parsed)
    };
    let a = render("alpha");
    let b = render("beta");
    assert!(a.contains("---@class mare.EventEnvelope"));
    assert!(a.contains("---@class mare.event.demo__pinged"));
    assert!(a.contains("function Ctx:on(name, listener, options) end"));
    assert_ne!(a, b, "changing contract event payload must change generated LuaLS");

    // 3. `mare rpc schema` includes `events` section.
    let schema_out = crate::common::mare_cmd().args(["rpc", "schema"]).run_output();
    assert!(schema_out.status.success(), "{}", String::from_utf8_lossy(&schema_out.stderr));
    let schema: serde_json::Value = serde_json::from_slice(&schema_out.stdout).unwrap();
    let schema_events = schema["events"].as_object().expect("schema must contain top-level `events`");
    for def in PROTOCOL_EVENTS {
        assert!(schema_events.contains_key(def.name), "rpc schema events missing `{}`", def.name);
    }

    // 4. Live RPC `events.list` includes events declared by loaded third-party plugin contracts.
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/init.lua"), "").unwrap();
    std::fs::write(
        home.join("config/plugins/ev_plugin.lua"),
        r#"return {
    name = "ev-plugin",
    contracts = {
        ["ev-demo"] = {
            methods = {},
            events = {
                pulsed = {
                    doc = "Plugin emitted pulse.",
                    mode = "emit",
                    expose = "both",
                    delivery = "required",
                    ignorable = true,
                    data = {
                        type = "object",
                        properties = { seq_id = { type = "string" } },
                        required = { "seq_id" },
                        additionalProperties = false,
                    },
                    example = { seq_id = "p-1" },
                },
            },
        },
    },
}"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, root.path());
    client.call("initialize", serde_json::json!({ "protocol_version": "3.0" }));
    let listed = client.call("events.list", serde_json::json!({}));
    let decoded: mare_protocol::EventsListResult = serde_json::from_value(listed).unwrap();
    let pulse = decoded.data.iter().find(|e| e.name == "ev-demo/pulsed").expect("events.list missing ev-demo/pulsed");
    assert!(pulse.public);
    assert_eq!(pulse.example, serde_json::json!({ "seq_id": "p-1" }));
    client.shutdown_and_wait();
}

/// runtime dispatch gate rejects undeclared events under a registered root, verb/mode
/// mismatches, and Lua dispatches from non-owner plugins.
#[tokio::test]
async fn contract_events_runtime_dispatch_gate() {
    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/c3_owner.lua"),
            r#"return {
                name = "c3-owner",
                provide = { "c3_svc" },
                contracts = {
                    c3_svc = {
                        methods = {},
                        events = {
                            pinged = { doc = "emit event", mode = "emit", expose = "lua" },
                            gated = { doc = "policy point", mode = "policy", expose = "lua" },
                        },
                    },
                },
            }"#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("plugins/c3_impostor.lua"),
            r#"return {
                name = "c3-impostor",
                inject = { "bus" },
                apply = function(ctx)
                    local ok_undec, err_undec = pcall(function() ctx.bus:emit("c3_svc/undeclared", {}) end)
                    local ok_verb, err_verb = pcall(function() ctx.bus:emit("c3_svc/gated", {}) end)
                    local ok_owner, err_owner = pcall(function() ctx.bus:emit("c3_svc/pinged", {}) end)
                    mare._c3_probe = {
                        ok_undec = ok_undec, err_undec = tostring(err_undec),
                        ok_verb = ok_verb, err_verb = tostring(err_verb),
                        ok_owner = ok_owner, err_owner = tostring(err_owner),
                    }
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let raw: String = crate::common::eval(&h, "return mare.json.encode(mare._c3_probe)").await;
        let probe: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(probe["ok_undec"], false, "{probe}");
        assert!(probe["err_undec"].as_str().unwrap_or("").contains("undeclared"), "{probe}");
        assert_eq!(probe["ok_verb"], false, "{probe}");
        assert!(probe["err_verb"].as_str().unwrap_or("").contains("mode"), "{probe}");
        assert_eq!(probe["ok_owner"], false, "{probe}");
        assert!(probe["err_owner"].as_str().unwrap_or("").contains("c3-impostor"), "{probe}");
        h.shutdown().await;
    })
    .await;
}

/// exhaustive match over every `ProtocolEvent` variant (no `_` wildcard)
/// paired with a live headless `--json --stream-deltas` faux run asserting every protocol event decodes
/// and validates against `schema.json`.
#[test]
fn exhaustive_protocol_event_match_and_faux_roundtrip() {
    fn tag_of(ev: &mare_protocol::events::ProtocolEvent) -> &'static str {
        use mare_protocol::events::ProtocolEvent::*;
        match ev {
            TurnStarted(_) => "turn/started",
            TurnCompleted(_) => "turn/completed",
            StepStarted(_) => "step/started",
            StepCompleted(_) => "step/completed",
            ItemStarted(_) => "item/started",
            ItemDelta(_) => "item/delta",
            ItemCompleted(_) => "item/completed",
            QueueChanged(_) => "queue/changed",
            Notice(_) => "host/notice",
            SubagentStarted(_) => "subagent/started",
            SubagentProgress(_) => "subagent/progress",
            SubagentCompleted(_) => "subagent/completed",
            ApprovalRequested(_) => "approval/requested",
            ApprovalResolved(_) => "approval/resolved",
            StatusChanged(_) => "status/changed",
            InteractionRequested(_) => "interaction/requested",
            InteractionResolved(_) => "interaction/resolved",
        }
    }

    // 1. Every declared protocol example round-trips through ProtocolEvent and exhaustive match.
    for def in PROTOCOL_EVENTS {
        let env = serde_json::json!({ "type": def.name, "data": (def.example)() });
        let parsed: mare_protocol::events::ProtocolEvent =
            serde_json::from_value(env).unwrap_or_else(|e| panic!("failed ProtocolEvent decode for {}: {e}", def.name));
        assert_eq!(tag_of(&parsed), def.name);
    }

    // 2. Live faux run with a tool call exercises turn/*, step/*, and item/* end-to-end.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("note.txt"), "hello c8").unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        serde_json::json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "note.txt" } }] },
            { "text": "done-c8" }
        ])
        .to_string(),
    )
    .unwrap();

    let out = crate::common::mare_cmd()
        .args(["--json", "--stream-deltas", "--model", "faux/echo", "--no-session", "run"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(std::process::Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let catalog = mare_protocol::schema();
    let mut seen = BTreeSet::new();
    for line in String::from_utf8(out.stdout).unwrap().lines() {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        let kind = record["type"].as_str().unwrap();
        seen.insert(kind.to_string());
        if PROTOCOL_EVENTS.iter().any(|d| d.name == kind) {
            mare_protocol::validate_schema_at(&catalog, "#/headless/line", &record)
                .unwrap_or_else(|e| panic!("schema rejected headless line {record}: {e}"));
            let parsed: mare_protocol::events::ProtocolEvent =
                serde_json::from_value(serde_json::json!({ "type": kind, "data": record["data"] }))
                    .unwrap_or_else(|e| panic!("ProtocolEvent rejected live {kind}: {e}\nline={record}"));
            assert_eq!(tag_of(&parsed), kind);
        }
    }
    for required in [
        "turn/started",
        "step/started",
        "item/started",
        "item/delta",
        "item/completed",
        "step/completed",
        "turn/completed",
    ] {
        assert!(seen.contains(required), "live faux stream missing `{required}` in {seen:?}");
    }
}

#[tokio::test]
async fn undeclared_event_cannot_be_dispatched() {
    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/probe_undeclared.lua"),
            r#"return {
                name = "probe-undeclared",
                inject = { "bus" },
                apply = function(ctx)
                    local ok_bus, err_bus = pcall(function() ctx.bus:emit("unregistered/ping", {}) end)
                    local ok_ctx, err_ctx = pcall(function() ctx:emit("unregistered/ping", {}) end)
                    mare._k2_undec = {
                        ok_bus = ok_bus, err_bus = tostring(err_bus),
                        ok_ctx = ok_ctx, err_ctx = tostring(err_ctx),
                    }
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let raw: String = crate::common::eval(&h, "return mare.json.encode(mare._k2_undec)").await;
        let probe: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(probe["ok_bus"], false, "ctx.bus:emit must reject undeclared root: {probe}");
        assert!(probe["err_bus"].as_str().unwrap_or("").contains("undeclared"), "{probe}");
        assert_eq!(probe["ok_ctx"], false, "ctx:emit must reject undeclared root: {probe}");
        assert!(probe["err_ctx"].as_str().unwrap_or("").contains("undeclared"), "{probe}");

        let checked_err = h
            .kernel
            .emit_checked("unregistered/ping", mlua::MultiValue::new())
            .await
            .expect_err("emit_checked must reject undeclared event");
        assert!(checked_err.to_string().contains("undeclared"), "{checked_err}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn user_plugin_listed_in_emitters_can_emit() {
    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/k2_emit_owner.lua"),
            r#"return {
                name = "k2-emit-owner",
                provide = { "k2_ev_svc" },
                contracts = {
                    k2_ev_svc = {
                        methods = {},
                        events = {
                            pinged = {
                                doc = "User-owned event allowing a named user emitter.",
                                mode = "emit",
                                expose = "lua",
                                emitters = { "k2-emit-helper" },
                            },
                        },
                    },
                },
                apply = function(ctx)
                    mare._k2_emit_seen = 0
                    ctx:on("k2_ev_svc/pinged", function()
                        mare._k2_emit_seen = mare._k2_emit_seen + 1
                    end)
                end,
            }"#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("plugins/k2_emit_helper.lua"),
            r#"return {
                name = "k2-emit-helper",
                inject = { "bus" },
                apply = function(ctx)
                    mare._k2_call_helper = function()
                        local ok, err = pcall(function() ctx.bus:emit("k2_ev_svc/pinged", {}) end)
                        return { ok = ok, err = tostring(err) }
                    end
                end,
            }"#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("plugins/k2_emit_outsider.lua"),
            r#"return {
                name = "k2-emit-outsider",
                inject = { "bus" },
                apply = function(ctx)
                    mare._k2_call_outsider = function()
                        local ok, err = pcall(function() ctx.bus:emit("k2_ev_svc/pinged", {}) end)
                        return { ok = ok, err = tostring(err) }
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let raw: String = crate::common::eval(
            &h,
            "local helper = mare._k2_call_helper(); local outsider = mare._k2_call_outsider(); return mare.json.encode({ helper = helper, outsider = outsider, seen = mare._k2_emit_seen })",
        )
        .await;
        let res: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(res["helper"]["ok"], true, "user plugin listed in emitters must be allowed: {res}");
        assert_eq!(res["seen"], 1, "listener must observe helper emission: {res}");
        assert_eq!(res["outsider"]["ok"], false, "unlisted user plugin must still be rejected: {res}");
        h.shutdown().await;
    })
    .await;
}

#[test]
fn former_process_only_events_are_declared_process() {
    let lua = mlua::Lua::new();
    let raw: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
    let mut exposes: BTreeMap<String, String> = BTreeMap::new();
    for pair in raw.pairs::<String, mlua::Table>() {
        let (root, contract) = pair.unwrap();
        let Ok(events) = contract.get::<mlua::Table>("events") else {
            continue;
        };
        for row in events.pairs::<String, mlua::Table>() {
            let (short, spec) = row.unwrap();
            let expose = spec.get::<Option<String>>("expose").unwrap().unwrap_or_else(|| "lua".to_string());
            exposes.insert(format!("{root}/{short}"), expose);
        }
    }
    let expected_process = [
        "agent/approval-dialog",
        "agent/child-approval",
        "agent/commit",
        "compact/idle",
        "compact/overflow",
        "editor/changed",
        "jobs/internal-changed",
        "llm/model-changed",
        "llm/model-changing",
        "option/apply",
        "option/current",
        "provider/event",
        "provider/request",
        "provider/transport",
        "session/deleted",
        "tool/authorized",
        "ui/markdown-changed",
        "pipeline/approval-decided",
        "pipeline/approval-reviewed",
        "pipeline/tool-dispatching",
    ];
    let mut bad = Vec::new();
    for name in expected_process {
        match exposes.get(name).map(String::as_str) {
            Some("process") => {}
            other => bad.push(format!("{name} => {other:?}")),
        }
    }
    assert!(bad.is_empty(), "former PROCESS_ONLY events must be declared expose = \"process\": {bad:?}");
}

#[tokio::test]
async fn undeclared_internal_prefixed_event_is_rejected_and_docs_use_commit_before() {
    let root = repo_root();
    for lang in ["en", "ja", "zh"] {
        let doc = std::fs::read_to_string(root.join(format!("docs/{lang}/extend/services-and-events.md"))).unwrap();
        assert!(
            !doc.contains("ui/commit:before"),
            "docs/{lang}/extend/services-and-events.md still has ui/commit:before"
        );
        assert!(doc.contains("ui/commit-before"), "docs/{lang}/extend/services-and-events.md missing ui/commit-before");
    }

    crate::common::local(async {
        let env = crate::common::TestEnv::new().await;
        let plugins_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("k2-internal-spoof.lua"),
            r#"return {
                name = "k2-internal-spoof",
                apply = function(ctx)
                    mare._k2_emit_internal_spoof = function()
                        local ok, err = pcall(function()
                            ctx:emit("internal/spoof", { x = 1 })
                        end)
                        return { ok = ok, err = tostring(err or "") }
                    end
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        let raw: String = crate::common::eval(&h, "return mare.json.encode(mare._k2_emit_internal_spoof())").await;
        let res: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(res["ok"], false, "undeclared internal/* event must be rejected: {res}");
        h.shutdown().await;
    })
    .await;
}
