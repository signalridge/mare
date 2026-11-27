use super::*;

#[test]
fn explicit_null_callback_result_is_present() {
    let result: Message =
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":"call","result":null})).unwrap();
    assert_eq!(result.result, Some(Value::Null));
    let missing: Message = serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":"call"})).unwrap();
    assert_eq!(missing.result, None);
    assert!(
        serde_json::from_value::<Message>(serde_json::json!({"jsonrpc":"2.0","id":"call","result":null,"error":null}))
            .is_err()
    );
}

#[test]
fn peer_ref_rejects_camel_case_session_id() {
    assert!(
        serde_json::from_str::<PeerRef>(r#"{"sessionId":"s1"}"#).is_err(),
        "PeerRef must reject camelCase `sessionId`"
    );
}

#[test]
fn envelopes_roundtrip() {
    let line = r#"{"jsonrpc":"2.0","id":7,"method":"registrations.release","params":{"registration_id":"reg-1"}}"#;
    let m: Message = serde_json::from_str(line).unwrap();
    assert_eq!(m.id, Some(RequestId::Number(7)));
    let p: services::ReleaseParams = serde_json::from_value(m.params.unwrap()).unwrap();
    assert_eq!(p.registration_id, "reg-1");
    assert!(compatible("3.0") && compatible("3.1") && !compatible("2.0"));
}

#[test]
fn protocol_version_requires_strict_major_minor() {
    assert_eq!(parse_version("3.0"), Some((3, 0)));
    assert_eq!(parse_version("3.12"), Some((3, 12)));
    assert!(compatible("3.0"));
    assert!(compatible("3.5"));
    for bad in [
        "1.7",
        "2.0",
        "3",
        "3.",
        ".0",
        "3.invalid",
        "3.0.1",
        "",
        " ",
        " 3.0",
        "3.0 ",
        "3. 0",
        "+2.0",
        "-2.0",
        "2.-1",
        "999999999999999999999999.0",
    ] {
        assert!(!compatible(bad), "unexpectedly accepted {bad:?}");
    }
}

#[test]
fn partial_permission_answers_are_typed_and_explicit_null_is_not_full_approval() {
    let full: ApprovalRequestResult = serde_json::from_value(serde_json::json!({"decision":"allow"})).unwrap();
    assert!(full.permissions.is_none());
    let empty: ApprovalRequestResult =
        serde_json::from_value(serde_json::json!({"decision":"allow", "permissions":{}, "scope":"turn"})).unwrap();
    assert!(empty.permissions.is_some());
    assert_eq!(empty.scope.unwrap().as_str(), "turn");
    for bad in [
        serde_json::json!({"decision":"allow", "permissions":null}),
        serde_json::json!({"decision":"allow", "scope":null}),
        serde_json::json!({"decision":"allow", "permissions":{"write":"all"}}),
        serde_json::json!({"decision":"allow", "permissions":{"all":true}}),
        serde_json::json!({"decision":"allow", "scope":"forever"}),
        serde_json::json!({"decision":"allow", "permission":{}}),
    ] {
        assert!(serde_json::from_value::<ApprovalRequestResult>(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn schema_omits_realtime_and_media_capabilities_while_keeping_attachment_kinds() {
    let cat = schema();
    for method in ["realtime.start", "realtime.append", "realtime.stop"] {
        assert!(cat["methods"].get(method).is_none(), "unexpected method {method} in schema");
    }
    assert!(cat["notifications"].get("realtime.audio_delta").is_none());
    assert_eq!(cat["errors"]["unsupported"]["number"], codes::SERVER_ERROR);
    let server_caps = &cat["methods"]["initialize"]["result"]["$defs"]["ServerCapabilities"]["properties"];
    for field in ["video", "realtime", "media"] {
        assert!(server_caps.get(field).is_none(), "ServerCapabilities unexpectedly has {field}");
    }
    let attachment_schema = serde_json::to_value(schemars::schema_for!(Attachment)).unwrap();
    let attachment = &attachment_schema["properties"];
    assert!(attachment["kind"].is_object());
    assert!(attachment["url"].is_object());
    let kind_enum = attachment_schema["$defs"]["AttachmentKind"]["enum"].as_array().unwrap();
    assert!(kind_enum.iter().any(|v| v == "audio"), "{kind_enum:?}");
    assert!(kind_enum.iter().any(|v| v == "video"), "{kind_enum:?}");
}

#[test]
fn error_responses_always_carry_an_id() {
    let parse = serde_json::to_value(Message::error(None, ErrorCode::ParseError, "bad")).unwrap();
    assert!(parse.as_object().unwrap().get("id").is_some_and(serde_json::Value::is_null), "{parse}");
    let notification = serde_json::to_value(Message::notification("event", serde_json::json!({}))).unwrap();
    assert!(notification.get("id").is_none(), "{notification}");
}

#[test]
fn envelope_schema_pins_the_version_and_requires_an_id_on_errors() {
    let schema = crate::schema();
    let envelope = &schema["envelope"];
    assert_eq!(envelope["properties"]["jsonrpc"]["const"], "2.0", "{envelope}");
    assert_eq!(envelope["if"]["required"], serde_json::json!(["error"]), "{envelope}");
    assert_eq!(envelope["then"]["required"], serde_json::json!(["id"]), "{envelope}");
}

/// Every error sent carries `data.code`, and its number follows from that code:
/// a call site that gave only a number (deserialized from wire JSON) gets the code it stands for;
/// its other `data` fields move under `data.data`.
#[test]
fn every_sent_error_has_a_vocabulary_code() {
    let sent = |error: RpcError| {
        serde_json::to_value(Message::rpc_error(Some(RequestId::Number(1)), error)).unwrap()["error"].clone()
    };
    let wire_plain: RpcError =
        serde_json::from_value(serde_json::json!({ "code": codes::INVALID_PARAMS, "message": "bad" })).unwrap();
    let plain = sent(wire_plain);
    assert_eq!(plain, serde_json::json!({ "code": -32602, "message": "bad", "data": { "code": "invalid_arguments" } }));
    let wire_extra: RpcError = serde_json::from_value(
        serde_json::json!({ "code": codes::INTERNAL_ERROR, "message": "x", "data": { "path": "/p" } }),
    )
    .unwrap();
    let extra = sent(wire_extra);
    // internal: a fixed message, none of the original's data
    assert_eq!(
        extra,
        serde_json::json!({ "code": -32603, "message": "internal error (details are in the log)", "data": { "code": "internal" } })
    );
    let safe = sent(RpcError::core(ErrorCode::Internal, "theme.get failed (details are in the log)"));
    assert_eq!(safe["message"], "theme.get failed (details are in the log)");
    let coded = sent(RpcError::coded("busy", "full").with_data(serde_json::json!([1])));
    assert_eq!(
        coded,
        serde_json::json!({ "code": -32000, "message": "full", "data": { "code": "busy", "data": [1] } })
    );
    assert_eq!(sent(RpcError::coded("cancelled", "c"))["code"], -32800);
    assert_eq!(sent(RpcError::coded("theme.not_found", "n"))["code"], -32000);
    // a code outside the vocabulary and without a service's shape is `internal`
    let unknown = sent(RpcError::coded("not_a_real_code", "x"));
    assert_eq!(
        (unknown["code"].clone(), unknown["data"]["code"].clone()),
        (serde_json::json!(-32603), serde_json::json!("internal"))
    );
    assert_eq!(sent(RpcError::coded("Theme.NotFound", "x"))["data"]["code"], "internal");
}

/// Exhaustive core error-code table and schema.json snapshot check.
#[test]
fn error_code_exhaustive_table_and_schema_snapshot() {
    let live = crate::schema();
    let disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/schema.json")).unwrap())
            .unwrap();
    let live_errors = live["errors"].as_object().expect("schema.errors object");
    let disk_errors = disk["errors"].as_object().expect("schema.json errors object");
    assert_eq!(live_errors, disk_errors, "on-disk schema.json errors table drifted from live schema()");

    let enum_schema = schema::s::<ErrorCode>();
    let schema_enum: Vec<String> = enum_schema["enum"]
        .as_array()
        .expect("ErrorCode JSON Schema must emit enum")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let expected_codes: Vec<String> = ErrorCode::ALL.iter().map(|c| c.as_str().to_string()).collect();
    assert_eq!(schema_enum, expected_codes, "ErrorCode JSON Schema enum drifted from ErrorCode::ALL");
    assert_eq!(
        live_errors.len(),
        ErrorCode::ALL.len() + schema::PROTOCOL_METHOD_ERRORS.len(),
        "schema errors count mismatch"
    );

    for &variant in ErrorCode::ALL {
        let code = variant.as_str();
        let entry = live_errors.get(code).unwrap_or_else(|| panic!("missing {code} in schema errors"));
        assert_eq!(entry["number"].as_i64(), Some(variant.rpc_number()), "{code} number mismatch");
        assert_eq!(entry["meaning"].as_str(), Some(variant.meaning()), "{code} meaning mismatch");
        assert_eq!(serde_json::to_value(variant).unwrap(), serde_json::json!(code));
        assert_eq!(serde_json::from_value::<ErrorCode>(serde_json::json!(code)).unwrap(), variant);

        let normalized = RpcError::core(variant, "details (details are in the log)").normalized();
        assert_eq!(normalized.code(), variant.rpc_number());
        assert_eq!(normalized.error_code(), Some(code));
    }

    for &(code, meaning) in schema::PROTOCOL_METHOD_ERRORS {
        let entry = live_errors.get(code).unwrap_or_else(|| panic!("missing {code} in schema errors"));
        assert_eq!(entry["number"].as_i64(), Some(codes::SERVER_ERROR), "{code} number mismatch");
        assert_eq!(entry["meaning"].as_str(), Some(meaning), "{code} meaning mismatch");
    }
    let baseline_errors = live["methods"][methods::BASELINE_READ]["errors"]
        .as_array()
        .expect("baseline.read must declare its domain error codes");
    let baseline_codes: Vec<&str> = baseline_errors.iter().filter_map(Value::as_str).collect();
    assert_eq!(baseline_codes, vec!["baseline.stale", "baseline.unknown_part", "baseline.item_too_large"]);
}

#[test]
fn time_patterns_queries_and_results_roundtrip_with_strict_validation() {
    let patterns = [
        serde_json::json!({"kind": "instant", "at_ms": 1_700_000_000_000_i64}),
        serde_json::json!({"kind": "interval", "anchor_ms": 0.0, "every_ms": 60_000.0}),
        serde_json::json!({"kind": "calendar", "time": "09:30:00.000", "timezone": "UTC"}),
        serde_json::json!({"kind": "calendar", "time": "09:30:00.000", "timezone": "America/New_York", "weekdays": [1.0, 5.0, 7.0]}),
        serde_json::json!({"kind": "cron", "expression": "0 9 * * 1-5", "timezone": "UTC"}),
        serde_json::json!({"kind": "rrule", "rrule": "DTSTART:20260101T090000Z\nRRULE:FREQ=DAILY;COUNT=2"}),
    ];
    for raw in &patterns {
        let parsed: TimePattern = serde_json::from_value(raw.clone()).unwrap();
        let back = serde_json::to_value(&parsed).unwrap();
        let reparsed: TimePattern = serde_json::from_value(back).unwrap();
        assert_eq!(serde_json::to_value(&reparsed).unwrap(), serde_json::to_value(&parsed).unwrap());
    }

    // Safe-integer boundaries (both i64 and integral f64)
    let max_safe = 9_007_199_254_740_991_i64;
    let min_safe = -9_007_199_254_740_991_i64;
    for ok_at in [serde_json::json!(max_safe), serde_json::json!(min_safe), serde_json::json!(-42.0)] {
        assert!(
            serde_json::from_value::<TimePattern>(serde_json::json!({"kind": "instant", "at_ms": ok_at})).is_ok(),
            "rejected valid safe integer {ok_at}"
        );
    }
    for bad_at in [
        serde_json::json!(max_safe + 1),
        serde_json::json!(min_safe - 1),
        serde_json::json!(1.5),
        serde_json::json!(9_007_199_254_740_992.0),
        serde_json::json!("1000"),
        serde_json::json!(true),
        serde_json::json!(null),
    ] {
        assert!(
            serde_json::from_value::<TimePattern>(serde_json::json!({"kind": "instant", "at_ms": bad_at})).is_err(),
            "accepted invalid integer {bad_at}"
        );
    }

    // Weekdays validation (null/absent -> None, invalid element -> error)
    let cal_null: TimePattern = serde_json::from_value(
        serde_json::json!({"kind": "calendar", "time": "09:00", "timezone": "UTC", "weekdays": null}),
    )
    .unwrap();
    assert!(matches!(cal_null, TimePattern::Calendar { weekdays: None, .. }));
    for bad_days in [
        serde_json::json!([0]),
        serde_json::json!([8]),
        serde_json::json!([-1]),
        serde_json::json!([1.5]),
        serde_json::json!(["mon"]),
        serde_json::json!(123),
    ] {
        assert!(
            serde_json::from_value::<TimePattern>(
                serde_json::json!({"kind": "calendar", "time": "09:00", "timezone": "UTC", "weekdays": bad_days}),
            )
            .is_err(),
            "accepted invalid weekdays {bad_days}"
        );
    }

    // TimeQuery variants + default UTC timezone + optional_integer + deny_unknown_fields
    let queries = [
        serde_json::json!({"action": "sample"}),
        serde_json::json!({"action": "sample", "instant_ms": null}),
        serde_json::json!({"action": "sample", "instant_ms": 1234.0}),
        serde_json::json!({"action": "instant", "value": "2026-01-01T00:00:00Z"}),
        serde_json::json!({"action": "instant", "date": "2026-01-01", "time": "09:00", "timezone": "UTC"}),
        serde_json::json!({"action": "format", "instant_ms": 0}),
        serde_json::json!({"action": "format", "instant_ms": 0, "timezone": "Asia/Tokyo", "format": "%Y-%m-%d"}),
        serde_json::json!({"action": "normalize", "pattern": patterns[0], "after_ms": 0}),
        serde_json::json!({"action": "next", "pattern": patterns[1], "after_ms": 0}),
        serde_json::json!({"action": "resolve", "pattern": patterns[1], "scheduled_ms": 0, "accepted_ms": 60_000}),
    ];
    for raw in queries {
        let q: TimeQuery = serde_json::from_value(raw.clone()).unwrap();
        let back = serde_json::to_value(&q).unwrap();
        let _: TimeQuery = serde_json::from_value(back).unwrap();
    }
    let fmt_default: TimeQuery =
        serde_json::from_value(serde_json::json!({"action": "format", "instant_ms": 0})).unwrap();
    assert!(matches!(fmt_default, TimeQuery::Format { timezone, format: None, .. } if timezone == "UTC"));
    assert!(serde_json::from_value::<TimeQuery>(serde_json::json!({"action": "sample", "instant_ms": 1.5})).is_err());
    assert!(serde_json::from_value::<TimeQuery>(serde_json::json!({"action": "sample", "extra": 1})).is_err());

    // TimeQueryResult variants roundtrip + deny_unknown_fields
    let results = [
        TimeQueryResult::Sample {
            instant_ms: 0,
            local_date: "1970-01-01".into(),
            timestamp: "1970-01-01T00:00:00+00:00".into(),
            timezone: Some("UTC".into()),
            offset: "+00:00".into(),
        },
        TimeQueryResult::Instant { instant_ms: 0, iso: "1970-01-01T00:00:00.000Z".into() },
        TimeQueryResult::Format { text: "1970-01-01".into(), timezone: "UTC".into() },
        TimeQueryResult::Normalize { pattern: TimePattern::Instant { at_ms: 1000 }, next_ms: Some(1000) },
        TimeQueryResult::Next { next_ms: None },
        TimeQueryResult::Resolve { occurrence_ms: 1000, next_ms: Some(2000) },
    ];
    for res in results {
        let val = serde_json::to_value(&res).unwrap();
        let back: TimeQueryResult = serde_json::from_value(val.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), val);
    }
    assert!(
        serde_json::from_value::<TimeQueryResult>(serde_json::json!({"action": "next", "next_ms": null, "extra": 1}))
            .is_err()
    );
}

#[test]
fn ui_params_reject_or_ignore_legacy_session_id_alias() {
    assert!(
        serde_json::from_value::<UiAttachParams>(serde_json::json!({ "sessionId": "s2", "responder": true })).is_err()
    );
    assert!(serde_json::from_value::<UiDetachParams>(serde_json::json!({ "sessionId": "s2" })).is_err());
    assert!(
        serde_json::from_value::<UiComponentUpdateParams>(
            serde_json::json!({ "sessionId": "s2", "component": "status" })
        )
        .is_err()
    );

    assert!(serde_json::from_value::<UiAttachParams>(serde_json::json!({})).is_err());
    assert!(serde_json::from_value::<UiDetachParams>(serde_json::json!({})).is_err());
    assert_eq!(
        serde_json::from_value::<UiAttachParams>(serde_json::json!({ "session_id": "s2" })).unwrap().session_id,
        "s2"
    );
    assert_eq!(
        serde_json::from_value::<UiSelectParams>(serde_json::json!({ "sessionId": "s2", "items": [] }))
            .unwrap()
            .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiInputParams>(serde_json::json!({ "sessionId": "s2" })).unwrap().session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiConfirmParams>(serde_json::json!({ "sessionId": "s2", "prompt": "p" }))
            .unwrap()
            .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiNotifyParams>(serde_json::json!({ "sessionId": "s2", "text": "t" }))
            .unwrap()
            .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiEditorParams>(serde_json::json!({ "sessionId": "s2" })).unwrap().session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiSetEditorParams>(serde_json::json!({ "sessionId": "s2", "text": "t" }))
            .unwrap()
            .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiEditorHintParams>(serde_json::json!({ "sessionId": "s2", "text": "t" }))
            .unwrap()
            .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiEditorConfigParams>(serde_json::json!({ "sessionId": "s2" })).unwrap().session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiWidgetParams>(
            serde_json::json!({ "sessionId": "s2", "widget_id": "w", "slot": "above-editor" })
        )
        .unwrap()
        .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiStatusParams>(
            serde_json::json!({ "sessionId": "s2", "status_id": "st", "align": "left" })
        )
        .unwrap()
        .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiOverlayParams>(
            serde_json::json!({ "sessionId": "s2", "title": "ov", "open": true })
        )
        .unwrap()
        .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiEditorStateParams>(
            serde_json::json!({ "sessionId": "s2", "text": "", "cursor": 0, "revision": 1 })
        )
        .unwrap()
        .session_id,
        None
    );
    assert_eq!(
        serde_json::from_value::<UiQuestionsParams>(serde_json::json!({ "sessionId": "s2", "questions": [] }))
            .unwrap()
            .session_id,
        None
    );
}

#[test]
fn schema_requires_protocol_and_live_session_epoch() {
    let cat = schema();
    let init = &cat["methods"][methods::INITIALIZE];
    let init_params = &init["params"];
    let req: Vec<&str> = init_params["required"]
        .as_array()
        .expect("initialize params required")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(req.contains(&"protocol_version"), "initialize must require protocol_version: {init_params}");
    assert_eq!(init_params["additionalProperties"], false, "initialize params must deny unknown fields: {init_params}");
    assert!(init_params["properties"].get("protocol_version").is_some(), "{init_params}");
    assert!(init_params["properties"].get("protocolVersion").is_none(), "{init_params}");
    assert!(init_params["properties"].get("callback_timeout_ms").is_some(), "{init_params}");
    assert!(init_params["properties"].get("approval_timeout_ms").is_some(), "{init_params}");

    assert!(serde_json::from_value::<InitializeParams>(serde_json::json!({})).is_err());
    assert!(serde_json::from_value::<InitializeParams>(serde_json::json!({ "protocol_version": null })).is_err());
    assert!(
        serde_json::from_value::<InitializeParams>(serde_json::json!({ "protocol_version": "3.0", "sessionId": "s1" }))
            .is_err()
    );

    let set_caps = cat["methods"]
        .get("connection.set_capabilities")
        .expect("connection.set_capabilities must be in protocol schema");
    assert_eq!(set_caps["params"]["additionalProperties"], false, "{set_caps}");
    let caps_ref = set_caps["params"]["properties"]["capabilities"]["$ref"]
        .as_str()
        .and_then(|r| r.rsplit('/').next())
        .map(|name| &set_caps["params"]["$defs"][name])
        .unwrap_or(&set_caps["params"]["properties"]["capabilities"]);
    let caps_req: Vec<&str> = caps_ref["required"]
        .as_array()
        .expect("StrictClientCapabilities must require all four booleans")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for key in ["callbacks", "ui", "approvals", "elicitation"] {
        assert!(caps_req.contains(&key), "connection.set_capabilities missing required {key}: {caps_ref}");
    }

    let init_res = &init["result"];
    assert!(init_res["properties"].get("session_id").is_none(), "{init_res}");
    assert!(init_res["properties"].get("session_epoch").is_none(), "{init_res}");
    assert!(init_res["properties"].get("session").is_some(), "{init_res}");
    assert!(init_res["properties"].get("protocol_version").is_some(), "{init_res}");
    assert!(init_res["properties"].get("protocolVersion").is_none(), "{init_res}");

    let peer_msg = &cat["methods"][methods::PEER_MESSAGE]["params"];
    assert!(peer_msg["properties"].get("queue_only").is_some(), "{peer_msg}");
    assert!(peer_msg["properties"].get("queueOnly").is_none(), "{peer_msg}");
    let peer_info = &cat["methods"][methods::PEER_INFO]["result"];
    assert!(peer_info["properties"].get("delivery_modes").is_some(), "{peer_info}");
    assert!(peer_info["properties"].get("deliveryModes").is_none(), "{peer_info}");
}

/// Every wire schema (`methods`, `callbacks`, `notifications`,
/// `client_notifications`, `headless`, `ui_image_resource`, `ui_output_image_key`, and `PROTOCOL_EVENTS`)
/// must have no bare `id`, `handle`, `*generation*`, or non-`_at_ms` timestamp property names.
///
/// Structural exclusions (not exemptions):
/// - JSON-RPC 2.0 envelope `Message.id` (`schema()["envelope"]`) and request/call correlation
///   `CancelRequestParams.id` (`$/cancel_request`) / `CallbackUpdateParams.id` (`callback.update`).
///
/// Intentional exemption:
/// - `ToolCallBlock.id`: kept as `id` to match LLM provider tool-call message block shapes.
#[test]
fn wire_schema_has_no_bare_id_generation_or_handle() {
    fn is_non_ms_timestamp(prop: &str) -> bool {
        prop == "at"
            || prop.ends_with("_at")
            || prop == "time"
            || prop.ends_with("_time")
            || prop == "timestamp"
            || prop.ends_with("_timestamp")
            || prop == "created"
            || prop.ends_with("_created")
            || prop == "updated"
            || prop.ends_with("_updated")
            || prop == "expires"
            || prop.ends_with("_expires")
            || prop.starts_with("created_at_")
            || prop.starts_with("updated_at_")
            || matches!(prop, "time_from" | "time_to")
    }

    fn walk(node: &Value, path: &str, skip_top_rpc_id: bool, violations: &mut std::collections::BTreeSet<String>) {
        let Some(obj) = node.as_object() else { return };
        let is_tool_call_block =
            obj.get("title").and_then(Value::as_str) == Some("ToolCallBlock") || path.ends_with(".$defs.ToolCallBlock");
        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            for (name, prop_schema) in props {
                let is_jsonrpc_corr_id = skip_top_rpc_id && name == "id";
                let is_exempt_tool_call_id = is_tool_call_block && name == "id";
                if !is_jsonrpc_corr_id && !is_exempt_tool_call_id {
                    if matches!(name.as_str(), "id" | "handle") || name.contains("generation") {
                        violations.insert(format!("{path}.{name}"));
                    }
                    if is_non_ms_timestamp(name) {
                        violations.insert(format!("{path}.{name} (non-_at_ms timestamp)"));
                    }
                }
                walk(prop_schema, &format!("{path}.{name}"), false, violations);
            }
        }
        if let Some(defs) = obj.get("$defs").and_then(Value::as_object) {
            for (def_name, def_schema) in defs {
                walk(def_schema, &format!("{path}.$defs.{def_name}"), false, violations);
            }
        }
        if let Some(items) = obj.get("items") {
            walk(items, &format!("{path}[]"), false, violations);
        }
        if let Some(addl) = obj.get("additionalProperties") {
            walk(addl, &format!("{path}.*"), false, violations);
        }
        for key in ["oneOf", "anyOf", "allOf"] {
            if let Some(arr) = obj.get(key).and_then(Value::as_array) {
                for (i, item) in arr.iter().enumerate() {
                    walk(item, &format!("{path}.{key}[{i}]"), false, violations);
                }
            }
        }
    }

    let cat = schema();
    let mut violations = std::collections::BTreeSet::new();

    if let Some(methods_obj) = cat["methods"].as_object() {
        for (m, entry) in methods_obj {
            walk(&entry["params"], &format!("methods.{m}.params"), false, &mut violations);
            walk(&entry["result"], &format!("methods.{m}.result"), false, &mut violations);
        }
    }
    if let Some(callbacks_obj) = cat["callbacks"].as_object() {
        for (cb, entry) in callbacks_obj {
            walk(&entry["params"], &format!("callbacks.{cb}.params"), false, &mut violations);
            walk(&entry["result"], &format!("callbacks.{cb}.result"), false, &mut violations);
        }
    }
    if let Some(notifs) = cat["notifications"].as_object() {
        for (n, entry) in notifs {
            let is_corr = n == methods::CANCEL_REQUEST || n == methods::CALLBACK_UPDATE;
            walk(entry, &format!("notifications.{n}"), is_corr, &mut violations);
        }
    }
    if let Some(cnotifs) = cat["client_notifications"].as_object() {
        for (n, entry) in cnotifs {
            let is_corr = n == methods::CANCEL_REQUEST || n == methods::CALLBACK_UPDATE;
            walk(entry, &format!("client_notifications.{n}"), is_corr, &mut violations);
        }
    }
    if let Some(headless_obj) = cat["headless"].as_object() {
        for (k, entry) in headless_obj {
            walk(entry, &format!("headless.{k}"), false, &mut violations);
        }
    }
    walk(&cat["ui_image_resource"], "ui_image_resource", false, &mut violations);
    walk(&cat["ui_output_image_key"], "ui_output_image_key", false, &mut violations);

    for ev in event_registry::PROTOCOL_EVENTS {
        let ev_schema = (ev.data_schema)();
        walk(&ev_schema, &format!("protocol_event.{}", ev.name), false, &mut violations);
    }

    assert!(
        violations.is_empty(),
        "protocol wire schemas contain forbidden id/handle/generation/non-ms timestamp properties:\n{}",
        violations.into_iter().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn queue_lane_has_one_follow_spelling() {
    let ev = event_registry::PROTOCOL_EVENTS
        .iter()
        .find(|e| e.name == event_registry::EVENT_QUEUE_CHANGED)
        .expect("queue/changed must be in PROTOCOL_EVENTS");
    let schema = (ev.data_schema)();
    let props = schema["properties"].as_object().expect("QueueChanged properties");
    assert!(props.contains_key("items"), "QueueChanged must keep items: {schema}");
    assert!(!props.contains_key("steer"), "QueueChanged must drop legacy steer array: {schema}");
    assert!(!props.contains_key("follow"), "QueueChanged must drop legacy follow array: {schema}");

    let lane_enum = schema["$defs"]["QueueLane"]["enum"]
        .as_array()
        .or_else(|| props["items"]["items"]["properties"]["lane"]["enum"].as_array())
        .expect("QueueLane enum must be present");
    let lanes: Vec<&str> = lane_enum.iter().filter_map(Value::as_str).collect();
    assert_eq!(lanes, vec!["steer", "follow_up"], "QueueLane must expose only steer and follow_up: {lanes:?}");
}

#[test]
fn protocol_event_names_have_a_root() {
    let bad: Vec<&str> =
        event_registry::PROTOCOL_EVENTS.iter().map(|e| e.name).filter(|name| !name.contains('/')).collect();
    assert!(bad.is_empty(), "every protocol event must use `root/name`: unrooted events = {bad:?}");
}

#[test]
fn no_serde_alias_on_wire_types() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&src_dir).expect("read mare-protocol/src") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs")
            || path.file_name().and_then(|n| n.to_str()) == Some("tests.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed == "#[cfg(test)]" {
                break;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains("alias =") || trimmed.contains("alias=") {
                hits.push(format!("{}:{}: {trimmed}", path.file_name().unwrap().to_string_lossy(), idx + 1));
            }
        }
    }
    assert!(hits.is_empty(), "wire types must not carry serde `alias =`: {}", hits.join("\n"));
}

#[test]
fn service_method_spec_and_info_use_typed_enums_without_remote() {
    let ok_spec: services::MethodSpec = serde_json::from_value(serde_json::json!({
        "expose": "both",
        "access": "peer_default",
        "scope": "session"
    }))
    .expect("valid MethodSpec enum values must deserialize");
    assert_eq!(ok_spec.expose, Some(services::MethodExpose::Both));
    assert_eq!(ok_spec.access, Some(MethodAccess::PeerDefault));
    assert_eq!(ok_spec.scope, Some(services::MethodScope::Session));

    let bad_remote = serde_json::from_value::<services::MethodSpec>(serde_json::json!({ "expose": "remote" }));
    assert!(bad_remote.is_err(), "MethodSpec must reject removed expose = \"remote\"");

    let enum_values = |def: &Value| -> Vec<String> {
        if let Some(arr) = def.get("enum").and_then(Value::as_array) {
            return arr.iter().filter_map(Value::as_str).map(str::to_string).collect();
        }
        def.get("oneOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.get("const").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    };

    let cat = schema();
    let spec_defs = &cat["methods"][methods::SERVICES_REGISTER]["params"]["$defs"];
    assert_eq!(enum_values(&spec_defs["MethodExpose"]), vec!["both", "lua"]);
    assert_eq!(enum_values(&spec_defs["MethodAccess"]), vec!["primary", "peer", "peer_default"]);
    assert_eq!(enum_values(&spec_defs["MethodScope"]), vec!["host", "workspace", "session"]);

    let info_defs = &cat["methods"][methods::SERVICES_LIST]["result"]["$defs"];
    assert_eq!(enum_values(&info_defs["MethodExpose"]), vec!["both", "lua"]);
    assert_eq!(enum_values(&info_defs["MethodAccess"]), vec!["primary", "peer", "peer_default"]);
    assert_eq!(enum_values(&info_defs["MethodScope"]), vec!["host", "workspace", "session"]);
}

#[test]
fn schema_catalog_keys_and_notification_names_are_consistent() {
    let is_snake =
        |k: &str| !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    let cat = schema();
    for key in cat.as_object().unwrap().keys() {
        if key.starts_with('$') {
            continue;
        }
        assert!(is_snake(key), "top-level catalog key must be snake_case: {key}");
    }
    for key in cat["headless"].as_object().unwrap().keys() {
        if key.starts_with('$') {
            continue;
        }
        assert!(is_snake(key), "headless catalog key must be snake_case: {key}");
    }
    for section in ["notifications", "client_notifications"] {
        for name in cat[section].as_object().unwrap().keys() {
            assert!(!name.bytes().any(|b| b.is_ascii_uppercase()), "{section}.{name} must not contain camelCase");
            assert!(name.split('.').count() <= 2, "{section}.{name} must not use three dot-separated segments");
        }
    }
    let notifs = cat["notifications"].as_object().unwrap();
    for expected in ["fs/changed", "ui/select-updated", "ui/select-closed", "ui/component-updated", "$/cancel_request"]
    {
        assert!(notifs.contains_key(expected), "notifications missing {expected}");
    }
    assert!(cat["client_notifications"].as_object().unwrap().contains_key("$/cancel_request"));
}

#[test]
fn compaction_item_error_is_structured_and_inbound_ui_results_reject_unknown_fields() {
    // Item::Compaction accepts structured EventError and rejects bare string error.
    let valid = serde_json::json!({
        "type": "compaction",
        "reason": "auto",
        "error": { "code": "compact.not_compacted", "message": "summary failed" }
    });
    assert!(serde_json::from_value::<events::Item>(valid).is_ok());

    let legacy_string = serde_json::json!({
        "type": "compaction",
        "reason": "auto",
        "error": "summary failed"
    });
    assert!(serde_json::from_value::<events::Item>(legacy_string).is_err());

    // All 7 inbound handshake/UI callback structs reject unknown fields.
    assert!(serde_json::from_value::<ClientInfo>(serde_json::json!({ "name": "cli", "extra": 1 })).is_err());
    assert!(serde_json::from_value::<CancelRequestParams>(serde_json::json!({ "id": 1, "extra": 1 })).is_err());
    assert!(serde_json::from_value::<UiSelectResult>(serde_json::json!({ "selected": null })).is_err());
    assert!(serde_json::from_value::<UiSelectResult>(serde_json::json!({ "index": null })).is_ok());
    assert!(serde_json::from_value::<UiTextPromptResult>(serde_json::json!({ "value": "hi" })).is_err());
    assert!(serde_json::from_value::<UiConfirmResult>(serde_json::json!({ "ok": true })).is_err());
    assert!(serde_json::from_value::<UiQuestionAnswer>(serde_json::json!({ "selected": [], "extra": 1 })).is_err());
    assert!(serde_json::from_value::<UiQuestionsResult>(serde_json::json!({ "answers": {}, "extra": 1 })).is_err());
}
