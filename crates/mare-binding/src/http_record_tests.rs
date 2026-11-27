use super::*;

// Run env-sensitive tests in a fresh process, including under plain `cargo test`.
fn run_isolated(test: &str, vars: &[(&str, &std::ffi::OsStr)]) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .env("MARE_COV_HTTP_RECORD_CHILD", test)
        .env_remove("MARE_RECORD_CASSETTE")
        .env_remove("MARE_RECORD_PHASE");
    for (key, value) in vars {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child test {test} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "child did not run {test}");
}

#[test]
fn append_creates_a_cassette_and_pads_missing_phases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");
    append(&path, json!({ "id": 1 }), None).unwrap();
    append(&path, json!({ "id": 2 }), Some("start".into())).unwrap();
    append(&path, json!({ "id": 3 }), None).unwrap();
    append(&path, json!({ "id": 4 }), Some("finish".into())).unwrap();

    let cassette: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(cassette["version"], 1);
    assert_eq!(cassette["metadata"]["name"], "recording");
    assert_eq!(cassette["metadata"]["source"], "mare-record");
    assert!(cassette["metadata"]["recordedAt"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(cassette["interactions"], json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }, { "id": 4 }]));
    assert_eq!(cassette["metadata"]["phases"], json!(["", "start", "", "finish"]));
}

#[test]
fn append_refuses_non_json_without_changing_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.json");
    let original = "this is not a cassette\n";
    std::fs::write(&path, original).unwrap();
    let error = append(&path, json!({ "id": 1 }), Some("phase".into())).unwrap_err();
    assert!(error.starts_with("existing cassette is not JSON:"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

// Check sensitive and safe URL parts together.
#[test]
fn redact_url_masks_userinfo_and_each_sensitive_query_name() {
    let keys = [
        "key",
        "api_key",
        "api-key",
        "apikey",
        "token",
        "access_token",
        "signature",
        "sig",
        "code",
        "API-KEY",
        "x-amz-Credential",
        "X-AmZ-Signature",
    ];
    let query = keys.iter().map(|key| format!("{key}=value-{key}")).collect::<Vec<_>>().join("&");
    let redacted = redact_url(&format!("https://alice:password@example.com/v1?{query}&model=demo"));
    let parsed = url::Url::parse(&redacted).unwrap();
    assert_eq!(parsed.username(), "%5BREDACTED%5D");
    assert_eq!(parsed.password(), None);
    assert_eq!(parsed.host_str(), Some("example.com"));
    assert_eq!(parsed.path(), "/v1");
    let pairs = parsed.query_pairs().into_owned().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(pairs.len(), keys.len() + 1);
    for key in keys {
        assert_eq!(pairs.get(key).map(String::as_str), Some("[REDACTED]"), "{key}: {redacted}");
    }
    assert_eq!(pairs.get("model").map(String::as_str), Some("demo"));
}

#[test]
fn redact_url_uses_text_redaction_for_an_invalid_url() {
    let input = "not a url?token=hidden-value&model=demo";
    assert_eq!(redact_url(input), "not a url?token=[REDACTED]&model=demo");
}

#[test]
fn find_secrets_recognizes_each_supported_pattern() {
    for (sample, expected) in [
        ("Bearer abcdefghijklmnop", "bearer token"),
        ("sk-abcdefghijklmnopqrst", "sk- key"),
        ("AIza012345678901234567890123456789", "Google API key"),
        ("AKIA0123456789ABCDEF", "AWS access key"),
        ("ghp_012345678901234567890123456789", "GitHub token"),
        ("-----BEGIN RSA PRIVATE KEY-----", "private key"),
        ("eyJabcdefgh.abcdefghij.klmnopqrst", "JWT"),
    ] {
        let hits = find_secrets(sample);
        assert!(hits.iter().any(|hit| hit == expected), "expected {expected}: {hits:?}");
    }
}

#[test]
fn find_secrets_detects_a_credential_environment_value() {
    const TEST: &str = "http_record::tests::find_secrets_detects_a_credential_environment_value";
    const KEY: &str = "MARE_COV_RECORD_SECRET_2D09C1";
    const VALUE: &str = "cov-record-env-fixture-2d09c1";
    if std::env::var("MARE_COV_HTTP_RECORD_CHILD").as_deref() != Ok(TEST) {
        run_isolated(TEST, &[(KEY, std::ffi::OsStr::new(VALUE))]);
        return;
    }
    let hits = find_secrets(&format!("prefix {VALUE} suffix"));
    assert!(hits.iter().any(|hit| hit == &format!("value of ${KEY}")), "{hits:?}");
}

// A known credential the cassette would keep in an encoded form (a form field
// re-encoded with the rest of the body once another field is redacted, the client's own form
// encoding, a JSON escape, a re-encoded query) is refused like a verbatim one, and the cassette
// already on disk stays byte for byte as it was.
#[test]
fn append_refuses_a_credential_kept_encoded() {
    const TEST: &str = "http_record::tests::append_refuses_a_credential_kept_encoded";
    const KEY: &str = "MARE_COV_RECORD_TOKEN_5E8A27";
    const VALUE: &str = r#"a&b=c+d%"q\-5e8a27"#;
    if std::env::var("MARE_COV_HTTP_RECORD_CHILD").as_deref() != Ok(TEST) {
        run_isolated(TEST, &[(KEY, std::ffi::OsStr::new(VALUE))]);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("encoded.json");
    append(&path, json!({ "id": 1 }), None).unwrap();
    let encode = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let encoded = encode(VALUE);
    let form = "application/x-www-form-urlencoded";
    let query_url = format!("https://example.com/token?note={encoded}&token=t");
    // review b-http-secret ②: a query value that is itself percent-encoded (a whole target URL)
    let nested_url = format!("https://example.com/probe?target={}", encode(&format!("https://u:{encoded}@h/")));
    // review b-http-secret ①: a tool result that is a JSON string (the credential serialized)
    let json_string = json!({ "messages": [{ "content": serde_json::to_string(VALUE).unwrap() }] }).to_string();
    // review b-http-secret ③: a UTF-16LE text response (BOM first), which the recorder stores as base64
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(format!("password: {VALUE}").encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let none: &[(u64, Bytes)] = &[];
    let utf16_chunks = [(0, Bytes::from(utf16))];
    let cases = [
        ("redacted form", "https://example.com/token", form, format!("password=hunter2&note={encoded}"), none),
        ("kept form", "https://example.com/token", form, format!("note={encoded}&model=demo"), none),
        ("json", "https://example.com/token", "application/json", json!({ "note": VALUE }).to_string(), none),
        ("query", query_url.as_str(), form, "model=demo".to_string(), none),
        ("json string", "https://example.com/token", "application/json", json_string, none),
        ("nested query", nested_url.as_str(), form, "model=demo".to_string(), none),
        ("utf-16 response", "https://example.com/doc", form, "model=demo".to_string(), &utf16_chunks[..]),
    ];
    // every case is tried, so a failure names each one that got through
    let mut leaked = vec![];
    for (case, url, content_type, body, chunks) in cases {
        let before = std::fs::read(&path).unwrap();
        let req = RequestSnapshot {
            method: "POST".into(),
            url: url.to_string(),
            headers: vec![("content-type".into(), content_type.into())],
            body: Some(body.into_bytes()),
        };
        // refused while the interaction is built (a body stored as base64) or when it is written
        let response_type = (!chunks.is_empty()).then_some("text/plain; charset=utf-16");
        match interaction(&req, 200, response_type, chunks, None).and_then(|it| append(&path, it, None)) {
            Err(error) if error.contains(&format!("value of ${KEY}")) => {
                assert_eq!(std::fs::read(&path).unwrap(), before, "{case}: the cassette changed");
            }
            outcome => leaked.push(format!("{case}: {outcome:?}")),
        }
    }
    assert!(leaked.is_empty(), "not refused for the credential: {leaked:?}");
}

// Review b-http-secret ④: `Bearer` and the next line are not a credential (the shared redactor's
// rule, mare-shared redact.rs:72 avoids_bearer_false_positives).
#[test]
fn find_secrets_does_not_join_bearer_across_a_line() {
    for text in ["Bearer\nabcdefghijklmnop", "Bearer of good news", "NotABearer abcdefghijklmnop"] {
        assert!(!find_secrets(text).iter().any(|hit| hit == "bearer token"), "{text:?}");
    }
    assert!(find_secrets("Bearer \tabcdefghijklmnop").iter().any(|hit| hit == "bearer token"));
}

#[test]
fn interaction_keeps_plain_chunks_and_their_times() {
    let req = RequestSnapshot { method: "GET".into(), url: "https://example.com/".into(), headers: vec![], body: None };
    let chunks = [(11, Bytes::from_static(b"hello ")), (27, Bytes::from_static(b"world"))];
    let it = interaction(&req, 200, None, &chunks, Some("close")).unwrap();
    assert_eq!(it["response"]["body"], "hello world");
    assert_eq!(it["response"]["chunks"], json!([{ "t_ms": 11, "text": "hello " }, { "t_ms": 27, "text": "world" }]));
    assert!(it["response"].get("end").is_none());
}

#[test]
fn interaction_encodes_binary_chunks_and_records_abnormal_ends() {
    let req = RequestSnapshot { method: "GET".into(), url: "https://example.com/".into(), headers: vec![], body: None };
    let chunks = [(3, Bytes::from_static(b"hello")), (8, Bytes::from_static(b"\xff\0"))];
    for end in ["reset", "truncate"] {
        let it = interaction(&req, 206, Some("application/octet-stream"), &chunks, Some(end)).unwrap();
        assert_eq!(it["response"]["end"], end);
        assert_eq!(it["response"]["bodyEncoding"], "base64");
        let body = base64::engine::general_purpose::STANDARD.decode(it["response"]["body"].as_str().unwrap()).unwrap();
        assert_eq!(body, b"hello\xff\0");
        assert_eq!(it["response"]["chunks"], json!([{ "t_ms": 3, "text": "hello" }, { "t_ms": 8, "b64": "/wA=" }]));
    }
}

#[tokio::test]
async fn tee_preserves_stream_items_and_records_close_reset_and_truncate() {
    use futures_util::{StreamExt, stream};

    const TEST: &str = "http_record::tests::tee_preserves_stream_items_and_records_close_reset_and_truncate";
    if std::env::var("MARE_COV_HTTP_RECORD_CHILD").as_deref() != Ok(TEST) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.json");
        run_isolated(TEST, &[("MARE_RECORD_CASSETTE", path.as_os_str())]);
        return;
    }

    let request = || snapshot("POST", "https://example.com/stream", &[], Some(b"request body")).unwrap();
    let headers = [("content-type".into(), "text/plain".into())];
    let mut closed = tee(
        request(),
        200,
        &headers,
        Box::pin(stream::iter(vec![Ok(Bytes::from_static(b"first")), Ok(Bytes::from_static(b"second"))])),
    );
    assert_eq!(closed.next().await.unwrap().unwrap(), Bytes::from_static(b"first"));
    assert_eq!(closed.next().await.unwrap().unwrap(), Bytes::from_static(b"second"));
    assert!(closed.next().await.is_none());
    drop(closed);

    let response = reqwest::Response::from(::http::Response::builder().status(502).body("").unwrap());
    let error = response.error_for_status().unwrap_err();
    let error_message = error.to_string();
    let mut reset = tee(
        request(),
        502,
        &headers,
        Box::pin(stream::iter(vec![Ok(Bytes::from_static(b"before-error")), Err(error)])),
    );
    assert_eq!(reset.next().await.unwrap().unwrap(), Bytes::from_static(b"before-error"));
    assert_eq!(reset.next().await.unwrap().unwrap_err().to_string(), error_message);
    assert!(reset.next().await.is_none());
    drop(reset);

    let mut truncated = tee(
        request(),
        200,
        &headers,
        Box::pin(stream::iter(vec![Ok(Bytes::from_static(b"visible")), Ok(Bytes::from_static(b"unseen"))])),
    );
    assert_eq!(truncated.next().await.unwrap().unwrap(), Bytes::from_static(b"visible"));
    drop(truncated);

    let cassette: Value = serde_json::from_slice(&std::fs::read(target().unwrap()).unwrap()).unwrap();
    let interactions = cassette["interactions"].as_array().unwrap();
    assert_eq!(interactions.len(), 3);
    assert_eq!(interactions[0]["request"]["body"], "request body");
    assert_eq!(interactions[0]["response"]["headers"]["content-type"], "text/plain");
    assert_eq!(interactions[0]["response"]["body"], "firstsecond");
    assert!(interactions[0]["response"].get("end").is_none());
    let chunks = interactions[0]["response"]["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0]["text"], "first");
    assert_eq!(chunks[1]["text"], "second");
    assert!(chunks[0]["t_ms"].as_u64().unwrap() <= chunks[1]["t_ms"].as_u64().unwrap());
    assert_eq!(interactions[1]["response"]["body"], "before-error");
    assert_eq!(interactions[1]["response"]["end"], "reset");
    assert_eq!(interactions[2]["response"]["body"], "visible");
    assert_eq!(interactions[2]["response"]["end"], "truncate");
}

/// Credentials in headers, query and JSON fields never reach the cassette, and a secret that
/// survives redaction refuses the write.
#[test]
fn redaction_and_secret_scan_keep_credentials_out_of_the_cassette() {
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://user:pw@api.example.com/v1/chat?key=AIzaSyA0123456789012345678901234567890&model=m".into(),
        headers: vec![
            ("Authorization".into(), "Bearer sk-live-0123456789abcdef".into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: Some(br#"{"model":"m","api_key":"secret-value-1234"}"#.to_vec()),
    };
    let it =
        interaction(&req, 200, Some("text/event-stream"), &[(0, Bytes::from_static(b"data: hi\n\n"))], None).unwrap();
    let text = it.to_string();
    assert!(
        !text.contains("sk-live") && !text.contains("secret-value") && !text.contains("AIza") && !text.contains(":pw@"),
        "{text}"
    );
    assert_eq!(it["request"]["headers"], json!({ "content-type": "application/json" }));
    assert!(text.contains("model=m"), "non-secret query parameters stay: {text}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    std::fs::write(&path, "{\"version\":1,\"metadata\":{},\"interactions\":[]}\n").unwrap();
    let leaked =
        json!({ "transport": "http", "response": { "status": 200, "body": "token sk-ant-0123456789abcdefghij" } });
    let err = append(&path, leaked, None).unwrap_err();
    assert!(err.contains("secret"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"version\":1,\"metadata\":{},\"interactions\":[]}\n",
        "the existing cassette is untouched"
    );
}

/// An OAuth refresh request
/// is a form (provider-openai auth.lua:164); its refresh_token never reaches the cassette.
#[test]
fn an_oauth_refresh_form_is_written_without_its_token() {
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://auth.example.com/oauth/token".into(),
        headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        body: Some(b"grant_type=refresh_token&refresh_token=rt-mock-refresh-0123456789&client_id=app_mock".to_vec()),
    };
    let it = interaction(&req, 429, Some("application/json"), &[(0, Bytes::from_static(b"{}"))], None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    append(&path, it, None).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains("rt-mock-refresh"), "{written}");
    assert!(written.contains("grant_type=refresh_token") && written.contains("client_id=app_mock"), "{written}");
}

#[test]
fn an_oauth_code_exchange_form_is_written_without_credentials() {
    // provider-openai/login.lua sends these fields; form field order is not guaranteed.
    let form = "code=auth-code-fixture-6712&code_verifier=pkce-verifier-fixture-5834\
                &grant_type=authorization_code&client_id=app_mock\
                &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback";
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://auth.example.com/oauth/token".into(),
        headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        body: Some(form.as_bytes().to_vec()),
    };
    let it = interaction(&req, 200, Some("application/json"), &[(0, Bytes::from_static(b"{}"))], None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    append(&path, it, None).unwrap();
    let written = std::fs::read_to_string(path).unwrap();
    let leaks: Vec<_> = ["auth-code-fixture-6712", "pkce-verifier-fixture-5834"]
        .into_iter()
        .filter(|value| written.contains(value))
        .collect();
    assert!(leaks.is_empty(), "cassette leaked OAuth credentials: {leaks:?}");

    let cassette: Value = serde_json::from_str(&written).unwrap();
    let body = cassette["interactions"][0]["request"]["body"].as_str().unwrap();
    let fields = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(fields.get("code").map(String::as_str), Some("[REDACTED]"));
    assert_eq!(fields.get("code_verifier").map(String::as_str), Some("[REDACTED]"));
    assert_eq!(fields.get("grant_type").map(String::as_str), Some("authorization_code"));
    assert_eq!(fields.get("client_id").map(String::as_str), Some("app_mock"));
    assert_eq!(fields.get("redirect_uri").map(String::as_str), Some("http://localhost:1455/auth/callback"));
}

// Check this exchange's token and both public type URNs.
#[test]
fn an_oauth_token_exchange_form_keeps_token_types_without_credentials() {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange")
        .append_pair("subject_token", "subject-token-fixture-6712")
        .append_pair("subject_token_type", "urn:ietf:params:oauth:token-type:refresh_token")
        .append_pair("requested_token_type", "urn:ietf:params:oauth:token-type:id-jag")
        .append_pair("subject_token_type_secret", "near-match-fixture-5834")
        .append_pair("subject-token-type", "separator-variant-fixture-3791")
        .finish();
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://auth.example.com/oauth/token".into(),
        headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
        body: Some(form.into_bytes()),
    };
    let it = interaction(&req, 200, Some("application/json"), &[(0, Bytes::from_static(b"{}"))], None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth-token-exchange.json");
    append(&path, it, None).unwrap();
    let written = std::fs::read_to_string(path).unwrap();
    for secret in ["subject-token-fixture-6712", "near-match-fixture-5834", "separator-variant-fixture-3791"] {
        assert!(!written.contains(secret), "cassette leaked {secret}: {written}");
    }

    let cassette: Value = serde_json::from_str(&written).unwrap();
    let body = cassette["interactions"][0]["request"]["body"].as_str().unwrap();
    let fields = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(fields.get("subject_token").map(String::as_str), Some(REDACTED));
    assert_eq!(
        fields.get("subject_token_type").map(String::as_str),
        Some("urn:ietf:params:oauth:token-type:refresh_token")
    );
    assert_eq!(fields.get("requested_token_type").map(String::as_str), Some("urn:ietf:params:oauth:token-type:id-jag"));
    assert_eq!(fields.get("subject_token_type_secret").map(String::as_str), Some(REDACTED));
    assert_eq!(fields.get("subject-token-type").map(String::as_str), Some(REDACTED));
    assert_eq!(fields.get("grant_type").map(String::as_str), Some("urn:ietf:params:oauth:grant-type:token-exchange"));
}

#[test]
fn oauth_token_type_form_values_still_use_text_redaction() {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("subject_token_type", "Bearer abcdefghijklmnop")
        .append_pair("requested_token_type", "token=fixture-secret")
        .finish();
    let (body, changed) = redact_body(form.as_bytes(), Some("application/x-www-form-urlencoded"));
    assert!(changed);
    let fields = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(fields.get("subject_token_type").map(String::as_str), Some("Bearer [REDACTED]"));
    assert_eq!(fields.get("requested_token_type").map(String::as_str), Some("token=[REDACTED]"));
}

#[test]
fn form_field_with_a_secret_in_its_name_masks_the_value() {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("Bearer abcdefghijklmnop", "fixture-secret")
        .finish();
    let (body, changed) = redact_body(form.as_bytes(), Some("application/x-www-form-urlencoded"));
    assert!(changed);
    let fields = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<Vec<_>>();
    assert_eq!(fields, vec![("Bearer [REDACTED]".into(), REDACTED.into())]);
}

#[test]
fn form_fields_use_the_shared_json_credential_rules() {
    let keys = [
        "code",
        "code_verifier",
        "authorization",
        "proxy_authorization",
        "api_key",
        "x-api-key",
        "token",
        "refresh_token",
        "access_token",
        "id_token",
        "password",
        "secret",
        "client_secret",
        "api_secret",
        "cookie",
        "set_cookie",
        "bearer",
        "custom.password",
        "github_token",
        "signature",
        "sig",
        "custom_key",
        "custom_auth",
        "prefix&sig",
        "signature=embedded",
    ];
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (index, key) in keys.iter().enumerate() {
        serializer.append_pair(key, &format!("fixture-{index}"));
    }
    serializer.append_pair("client_id", "keep me");
    serializer.append_pair("code", "another-code");
    serializer.append_pair("note", "Bearer abcdefghijklmnop");
    let body = serializer.finish().replace("code_verifier=", "%63ode%5Fverifier=");
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://example.com/".into(),
        headers: vec![("CONTENT-TYPE".into(), "Application/X-Www-Form-Urlencoded; charset=utf-8".into())],
        body: Some(body.into_bytes()),
    };
    let it = interaction(&req, 200, None, &[], None).unwrap();
    let body = it["request"]["body"].as_str().unwrap();
    let fields = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<Vec<_>>();
    for key in keys {
        let values: Vec<_> = fields.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect();
        assert!(!values.is_empty(), "{key}: {body}");
        assert!(values.iter().all(|value| *value == "[REDACTED]"), "{key}: {body}");
    }
    assert_eq!(fields.iter().filter(|(key, _)| key == "code").count(), 2);
    assert!(fields.contains(&("client_id".into(), "keep me".into())));
    assert!(fields.contains(&("note".into(), "Bearer [REDACTED]".into())));

    for key in keys {
        if matches!(
            key,
            "code"
                | "code_verifier"
                | "signature"
                | "sig"
                | "custom_key"
                | "custom_auth"
                | "prefix&sig"
                | "signature=embedded"
        ) {
            continue;
        }
        let mut object = serde_json::Map::new();
        object.insert(key.into(), json!("fixture"));
        assert_eq!(mare_shared::redact::redact_json(&Value::Object(object))[key], "[REDACTED]", "{key}");
    }
    let safe_json = json!({ "code": "invalid_request", "code_verifier": "ordinary text", "client_id": "app" });
    assert_eq!(mare_shared::redact::redact_json(&safe_json), safe_json);
    let safe_form = "note=hello%20world&empty=&flag";
    assert_eq!(redact_body(safe_form.as_bytes(), Some("application/x-www-form-urlencoded")), (safe_form.into(), false));
}

#[test]
fn a_form_response_redacts_credentials_and_drops_raw_chunks() {
    let req =
        RequestSnapshot { method: "POST".into(), url: "https://example.com/".into(), headers: vec![], body: None };
    let chunks = [
        (0, Bytes::from_static(b"code=response-code-fixture&")),
        (3, Bytes::from_static(b"code_verifier=response-verifier-fixture&client_id=app")),
    ];
    let it = interaction(&req, 200, Some("application/x-www-form-urlencoded; charset=utf-8"), &chunks, None).unwrap();
    let body = it["response"]["body"].as_str().unwrap();
    let fields = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect::<std::collections::HashMap<_, _>>();
    assert_eq!(fields.get("code").map(String::as_str), Some("[REDACTED]"));
    assert_eq!(fields.get("code_verifier").map(String::as_str), Some("[REDACTED]"));
    assert_eq!(fields.get("client_id").map(String::as_str), Some("app"));
    assert!(it["response"].get("chunks").is_none(), "raw chunks must not expose credentials: {it}");
}

/// A token response that arrives in several pieces is written redacted and
/// without its raw pieces (they hold the original bytes, here a JWT split across two of them).
#[test]
fn a_multi_chunk_oauth_response_keeps_no_raw_chunks() {
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://auth.example.com/oauth/token".into(),
        headers: vec![],
        body: None,
    };
    let chunks = [
        (0, Bytes::from_static(br#"{"access_token":"eyJhbGciOiJub25lIn0.eyJzdWIi"#)),
        (
            3,
            Bytes::from_static(
                br#"OiJtb2NrIn0.c2lnbmF0dXJlMDEy","refresh_token":"rt-mock-0123456789","expires_in":3600}"#,
            ),
        ),
    ];
    let it = interaction(&req, 200, Some("application/json"), &chunks, None).unwrap();
    assert!(it["response"].get("chunks").is_none(), "{it}");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    append(&path, it, None).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains("eyJhbGci") && !written.contains("rt-mock-0123456789"), "{written}");
    assert!(written.contains("expires_in"), "{written}");
}

/// A token response cut mid-character (not UTF-8, so stored as
/// base64) with a JWT split across two pieces is refused, not written.
#[test]
fn a_non_utf8_body_holding_a_token_is_refused() {
    let req = RequestSnapshot {
        method: "POST".into(),
        url: "https://auth.example.com/oauth/token".into(),
        headers: vec![],
        body: None,
    };
    let chunks = [
        (0, Bytes::from_static(br#"{"access_token":"eyJhbGciOiJub25lIn0.eyJzdWIi"#)),
        (3, Bytes::from_static(b"OiJtb2NrIn0.c2lnbmF0dXJlMDEy\",\"note\":\"\xe4\xb8")),
    ];
    let err = interaction(&req, 200, Some("application/json"), &chunks, None).unwrap_err();
    assert!(err.contains("refused"), "{err}");
    let plain = [(0, Bytes::from_static(b"\x89PNG\r\n\x1a\n\x00\x00"))];
    let it = interaction(&req, 200, Some("image/png"), &plain, None).unwrap();
    assert_eq!(it["response"]["bodyEncoding"], "base64", "a binary body without a credential is kept: {it}");
}

// Deeply encoded credentials — base64 tool outputs
// (Kubernetes secrets, URL-safe tokens, base64-encoded JSON or UTF-16 text) and secrets split across
// SSE JSON delta events must still trigger refusal before writing the cassette.
#[test]
fn append_refuses_a_credential_in_base64_or_split_across_sse_events() {
    const TEST: &str = "http_record::tests::append_refuses_a_credential_in_base64_or_split_across_sse_events";
    const KEY: &str = "MARE_COV_DEEP_SECRET_7F3A19";
    const VALUE: &str = "deep-secret-token-7f3a19+/_=";
    const ESC_KEY: &str = "MARE_COV_DEEP_SECRET_ESC_7F3A19";
    const ESC_VALUE: &str = r#"esc-secret-"q\-7f3a19"#;
    if std::env::var("MARE_COV_HTTP_RECORD_CHILD").as_deref() != Ok(TEST) {
        run_isolated(TEST, &[(KEY, std::ffi::OsStr::new(VALUE)), (ESC_KEY, std::ffi::OsStr::new(ESC_VALUE))]);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep.json");
    append(&path, json!({ "id": 1 }), None).unwrap();

    let std_b64 = base64::engine::general_purpose::STANDARD.encode(VALUE.as_bytes());
    let url_b64_nopad = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(VALUE.as_bytes());
    let json_b64 = base64::engine::general_purpose::STANDARD.encode(json!({ "db": VALUE }).to_string().as_bytes());
    let utf16_b64 = base64::engine::general_purpose::STANDARD
        .encode([0xFFu8, 0xFE].into_iter().chain(VALUE.encode_utf16().flat_map(u16::to_le_bytes)).collect::<Vec<_>>());

    let sse_openai_tool = concat_sse(&[
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"secret\":\""}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":&VALUE[..12]}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":format!("{}\"}}", &VALUE[12..])}}]}}]}),
    ]);
    let sse_anthropic_text = concat_sse(&[
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":&VALUE[..10]}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":&VALUE[10..]}}),
    ]);
    let sse_responses_b64 = concat_sse(&[
        json!({"type":"response.output_text.delta","output_index":0,"delta":&std_b64[..10]}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":&std_b64[10..]}),
    ]);

    let req_only = |body: String| RequestSnapshot {
        method: "POST".into(),
        url: "https://api.example.com/v1/chat".into(),
        headers: vec![("content-type".into(), "application/json".into())],
        body: Some(body.into_bytes()),
    };
    let sse_resp = |sse: String| {
        (
            RequestSnapshot {
                method: "POST".into(),
                url: "https://api.example.com/v1/chat".into(),
                headers: vec![],
                body: None,
            },
            "text/event-stream",
            sse,
        )
    };

    let mut cases: Vec<(&str, RequestSnapshot, &str, String)> = vec![
        (
            "k8s-base64",
            req_only(json!({"messages":[{"content":json!({"data":{"pw":std_b64}}).to_string()}]}).to_string()),
            "application/json",
            "{}".into(),
        ),
        (
            "url-safe-nopad-base64",
            req_only(json!({"payload": url_b64_nopad}).to_string()),
            "application/json",
            "{}".into(),
        ),
        ("json-inside-base64", req_only(json!({"blob": json_b64}).to_string()), "application/json", "{}".into()),
        ("utf16-inside-base64", req_only(json!({"blob": utf16_b64}).to_string()), "application/json", "{}".into()),
        (
            "basic-auth-base64",
            req_only(json!({"auth": format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("u:{VALUE}")))}).to_string()),
            "application/json",
            "{}".into(),
        ),
    ];
    for (name, sse) in [
        ("sse-openai-tool", sse_openai_tool),
        ("sse-anthropic-text", sse_anthropic_text),
        ("sse-responses-b64", sse_responses_b64),
        (
            "sse-single-event-b64",
            concat_sse(&[json!({"type":"response.output_text.delta","output_index":0,"delta":std_b64})]),
        ),
        (
            "sse-single-event-basic",
            concat_sse(&[
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("u:{VALUE}")))}}),
            ]),
        ),
        (
            "sse-split-auth-basic",
            concat_sse(&[
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Authorization: "}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("u:{VALUE}")))}}),
            ]),
        ),
        (
            "sse-single-event-nested-json",
            concat_sse(&[
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":json!({"pw":std_b64}).to_string()}}),
            ]),
        ),
        (
            "sse-single-chunk-preceded-by-empty-args",
            concat_sse(&[
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}),
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":json!({"secret":std_b64}).to_string()}}]}}]}),
            ]),
        ),
        ("sse-bom-first-line", format!("\u{FEFF}{}", concat_sse(&[json!({"delta":std_b64})]))),
    ] {
        let (req, ct, body) = sse_resp(sse);
        cases.push((name, req, ct, body));
    }

    let mut leaked = vec![];
    for (name, req, resp_ct, resp_body) in cases {
        let before = std::fs::read(&path).unwrap();
        let chunks = [(0, Bytes::from(resp_body.into_bytes()))];
        match interaction(&req, 200, Some(resp_ct), &chunks, None).and_then(|it| append(&path, it, None)) {
            Err(error) if error.contains(&format!("value of ${KEY}")) => {
                assert_eq!(std::fs::read(&path).unwrap(), before, "{name}: cassette modified on refusal");
            }
            outcome => leaked.push(format!("{name}: {outcome:?}")),
        }
    }
    assert!(leaked.is_empty(), "not refused for deep credential: {leaked:?}");

    let esc_sse = concat_sse(&[
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":ESC_VALUE}}]}}]}),
    ]);
    let (esc_req, esc_ct, esc_body) = sse_resp(esc_sse);
    let esc_err = interaction(&esc_req, 200, Some(esc_ct), &[(0, Bytes::from(esc_body))], None)
        .and_then(|it| append(&path, it, None))
        .unwrap_err();
    assert!(
        esc_err.contains(&format!("value of ${ESC_KEY}")),
        "single-event JSON-escaped secret must be refused: {esc_err}"
    );

    let sk_sse = concat_sse(&[
        json!({"choices":[{"index":0,"delta":{"content":"here is sk-ant-01234"}}]}),
        json!({"choices":[{"index":0,"delta":{"content":"56789abcdef"}}]}),
    ]);
    let (sk_req, sk_ct, sk_body) = sse_resp(sk_sse);
    let mid = sk_body.len() / 2;
    let err = interaction(
        &sk_req,
        200,
        Some(sk_ct),
        &[(0, Bytes::from(sk_body.as_bytes()[..mid].to_vec())), (10, Bytes::from(sk_body.as_bytes()[mid..].to_vec()))],
        None,
    )
    .and_then(|it| append(&path, it, None))
    .unwrap_err();
    assert!(err.contains("sk- key"), "split sk- key across multi-chunk SSE events must be refused: {err}");

    let safe_b64 = base64::engine::general_purpose::STANDARD.encode(b"ordinary non-secret payload");
    let png_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode([0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0xD8, 0xFF, 0xE0, 0x80, 0xFE, 0xFF]);
    let safe_sse = concat_sse(&[
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\": \""}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"Paris\"}"}}]}}]}),
        json!({"thought_signature": png_b64}),
    ]);
    let safe_req = req_only(json!({"note": safe_b64, "img": format!("data:image/png;base64,{png_b64}")}).to_string());
    let it = interaction(&safe_req, 200, Some("text/event-stream"), &[(0, Bytes::from(safe_sse))], None).unwrap();
    append(&path, it, None).unwrap();
}

fn concat_sse(events: &[Value]) -> String {
    events.iter().map(|v| format!("data: {v}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}
