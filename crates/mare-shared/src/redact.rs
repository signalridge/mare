//! The one redactor: crash reports, logs, and session approval
//! records pass through it before they're written. Includes key-name and raw secret
//! patterns (MIT, Copyright (c) 2026 Moonshot AI) and token patterns (Apache-2.0).

use std::sync::LazyLock;

use regex::Regex;

pub const REDACTED: &str = "[REDACTED]";

/// Credential keys (lowercased, without `_ - .`); the bool marks form-only keys.
const REDACTED_KEYS: &[(&str, bool)] = &[
    ("authorization", false),
    ("proxyauthorization", false),
    ("apikey", false),
    ("xapikey", false),
    ("token", false),
    ("refreshtoken", false),
    ("accesstoken", false),
    ("idtoken", false),
    ("password", false),
    ("secret", false),
    ("clientsecret", false),
    ("apisecret", false),
    ("cookie", false),
    ("setcookie", false),
    ("bearer", false),
    ("code", true),
    ("codeverifier", true),
];

/// Key fragments: a key containing one of these (e.g. `openai_api_key`, `x-goog-api-key`,
/// `github_token`) loses a *string* value — numbers stay, so usage counts like `max_tokens` or
/// `input_tokens` survive.
const REDACTED_FRAGMENTS: &[&str] = &["apikey", "token", "secret", "authorization", "password", "passwd", "cookie"];

const QUERY_KEY_PATTERN: &str = r"(?:[a-z0-9_-]*(?:key|token|secret|password|auth)|sig|signature|code)";

// A decoded form key can itself contain query delimiters. Preserve the query rule's behavior
// for those keys too, including a key followed by an embedded `=` and a value.
static QUERY_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"(?i)(?:\A|[?&]){QUERY_KEY_PATTERN}(?:\z|=(?:\z|[^&#\s"']))"#))
        .expect("form query key pattern")
});

static KV_SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([a-z0-9_-]*(?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|secret[_-]?access[_-]?key|secret[_-]?key|private[_-]?key|token|password|passwd|secret)(?:[_-][a-z0-9_-]+)?["']?\s*[:=]\s*["']?)[^\s"'`,&]+"#,
    )
    .expect("kv secret redaction pattern")
});

static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let re = |p: &str| Regex::new(p).expect("redaction pattern");
    vec![
        // Raw key/value secret patterns (keep the label, drop the value); the label may be
        // part of a longer name (`openai_api_key=`, `x-api-key:`) and quoted (`"api_key": "…"`)
        (
            re(
                r#"(?i)\b((?:proxy-)?authorization["']?\s*[:=]\s*["']?)(?:(?:basic|bearer|token|digest)\s+)?[^\s"'`,]+"#,
            ),
            "${1}[REDACTED]",
        ),
        (re(r#"(?i)\b((?:set-)?cookie["']\s*[:=]\s*["'])[^"'\r\n]+"#), "${1}[REDACTED]"),
        (re(r"(?i)\b((?:set-)?cookie\s*[:=]\s*)[^\r\n]+"), "${1}[REDACTED]"),
        // Credentials in a URL's userinfo and secret query params
        (re(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s@?#]+@"), "${1}[REDACTED]@"),
        (re(&format!(r#"(?i)([?&]{QUERY_KEY_PATTERN}=)[^&#\s"']+"#)), "${1}[REDACTED]"),
        // PEM private key blocks (multi-line)
        (
            re(
                r"(?s)-----BEGIN [A-Z0-9 ]*(?:PRIVATE KEY|PRIVATE KEY BLOCK)-----.*?-----END [A-Z0-9 ]*(?:PRIVATE KEY|PRIVATE KEY BLOCK)-----",
            ),
            "[REDACTED]",
        ),
        // Well-known token shapes (JWT, GitHub, Google, AWS, Slack, Stripe, GitLab, npm, HF).
        // Match `Bearer <token>` (RFC 6750 b64token charset) before embedded token shapes
        // (`sk-`, `AKIA`, …) so a Bearer token containing one does not leave its prefix/suffix
        // unredacted.
        (re(r"(?i:\bBearer)[ \t]+[A-Za-z0-9._~+/-]{16,}=*"), "Bearer [REDACTED]"),
        (re(r"sk-[A-Za-z0-9_-]{20,}"), "[REDACTED]"),
        (re(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"), "[REDACTED]"),
        (re(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"), "[REDACTED]"),
        (re(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}"), "[REDACTED]"),
        (re(r"\bgithub_pat_[A-Za-z0-9_]{20,}"), "[REDACTED]"),
        (re(r"\bAIza[0-9A-Za-z_-]{30,}"), "[REDACTED]"),
        (re(r"\bya29\.[A-Za-z0-9._-]{20,}"), "[REDACTED]"),
        (re(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"), "[REDACTED]"),
        (re(r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{16,}\b"), "[REDACTED]"),
        (re(r"\bglpat-[A-Za-z0-9_-]{20,}\b"), "[REDACTED]"),
        (re(r"\bnpm_[A-Za-z0-9]{20,}\b"), "[REDACTED]"),
        (re(r"\bhf_[A-Za-z0-9]{20,}\b"), "[REDACTED]"),
    ]
});

/// Redact secrets in free text (best effort, pattern based).
pub fn redact_str(s: &str) -> String {
    let mut out = PATTERNS[0].0.replace_all(s, PATTERNS[0].1).into_owned();
    out = KV_SECRET_RE
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            let key = caps[1].trim_end_matches([' ', '\t', ':', '=', '"', '\'']).to_ascii_lowercase();
            if key.ends_with("token_type") || key.ends_with("token-type") {
                caps[0].to_string()
            } else {
                format!("{}{REDACTED}", &caps[1])
            }
        })
        .into_owned();
    for (re, rep) in &PATTERNS[1..] {
        out = re.replace_all(&out, *rep).into_owned();
    }
    out
}

fn norm_key(k: &str) -> String {
    k.chars().filter(|c| !matches!(c, '_' | '-' | '.')).flat_map(char::to_lowercase).collect()
}

/// Always redacted, whatever the value.
fn sensitive_key(k: &str, form: bool) -> bool {
    let key = norm_key(k);
    REDACTED_KEYS.iter().any(|(name, form_only)| *name == key && (form || !*form_only))
}

/// Redacted when the value is a string.
fn sensitive_fragment(k: &str) -> bool {
    let n = norm_key(k);
    REDACTED_FRAGMENTS.iter().any(|f| n.contains(f))
}

/// Whether a decoded URL query key identifies a credential or AWS signing parameter.
pub fn sensitive_query_key(k: &str) -> bool {
    k.to_ascii_lowercase().starts_with("x-amz-") || QUERY_KEY.is_match(k)
}

/// Whether a URL-encoded form field name is credential-like.
pub fn sensitive_form_key(k: &str) -> bool {
    if matches!(k, "subject_token_type" | "requested_token_type") {
        return false;
    }
    sensitive_key(k, true) || sensitive_fragment(k) || sensitive_query_key(k)
}

/// Whether a content type is a URL-encoded form, including optional parameters.
pub fn is_form_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';').next().unwrap_or_default().trim().eq_ignore_ascii_case("application/x-www-form-urlencoded")
    })
}

/// Decode and redact one URL-encoded form segment, returning the fields and whether they changed.
/// Empty segments return `None`; callers may retain their original bytes when unchanged.
pub fn redact_form_field(field: &[u8]) -> Option<(String, String, bool)> {
    let (key, value) = url::form_urlencoded::parse(field).next()?;
    let redacted_key = redact_str(&key);
    let redacted_value = if sensitive_form_key(&key) || redacted_key != key.as_ref() {
        REDACTED.to_string()
    } else {
        redact_str(&value)
    };
    let changed = redacted_key != key.as_ref() || redacted_value != value.as_ref();
    Some((redacted_key, redacted_value, changed))
}

/// Redact a JSON value: sensitive keys lose their values, strings go through [`redact_str`].
pub fn redact_json(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let hide = sensitive_key(k, false) || (v.is_string() && sensitive_fragment(k));
                    (k.clone(), if hide { Value::String(REDACTED.into()) } else { redact_json(v) })
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact_json).collect()),
        Value::String(s) => Value::String(redact_str(s)),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_segments_decode_and_redact_credentials_without_losing_business_values() {
        assert!(is_form_content_type(Some("Application/X-Www-Form-Urlencoded; charset=utf-8")));
        assert!(!is_form_content_type(Some("application/json")));
        assert_eq!(redact_form_field(b""), None);
        assert_eq!(redact_form_field(b"%63ode=secret"), Some(("code".into(), REDACTED.into(), true)));
        assert_eq!(redact_form_field(b"scope=read+write"), Some(("scope".into(), "read write".into(), false)));
        assert_eq!(
            redact_form_field(b"subject_token_type=urn%3Arefresh"),
            Some(("subject_token_type".into(), "urn:refresh".into(), false))
        );
        assert_eq!(
            redact_str("urn:ietf:params:oauth:token-type:refresh_token"),
            "urn:ietf:params:oauth:token-type:refresh_token"
        );
        assert_eq!(redact_str("refresh_token=secret123"), "refresh_token=[REDACTED]");
    }

    #[test]
    fn secrets_are_redacted_in_text_and_json() {
        let t = redact_str(
            "Authorization: Bearer abcdefghijklmnopqrstuv api_key=xyz123456 sk-ABCDEFGHIJKLMNOPQRSTUVWX AKIAABCDEFGHIJKLMNOP cookie: a=b; c=d",
        );
        assert!(!t.contains("abcdefghijklmnop") && !t.contains("xyz123456") && !t.contains("sk-ABC"), "{t}");
        assert!(!t.contains("AKIAABCD") && !t.contains("a=b"), "{t}");
        let j = redact_json(
            &serde_json::json!({ "Api-Key": "k", "nested": { "refresh_token": "r", "note": "token=abc123" } }),
        );
        assert_eq!(j["Api-Key"], REDACTED);
        assert_eq!(j["nested"]["refresh_token"], REDACTED);
        assert_eq!(j["nested"]["note"], "token=[REDACTED]");
    }

    #[test]
    fn url_userinfo_is_redacted_even_without_a_password_and_inside_text() {
        let text = redact_str("proxy https://FAKE_URL_TOKEN@example.invalid/path and https://example.invalid/path");
        assert_eq!(text, "proxy https://[REDACTED]@example.invalid/path and https://example.invalid/path");
    }

    #[test]
    fn key_fragments_urls_and_token_shapes_are_redacted() {
        let j = redact_json(&serde_json::json!({
            "x-api-key": "k1", "openai_api_key": "k2", "Proxy-Authorization": "Basic dXNlcjpwYXNz",
            "github_token": "k3", "max_tokens": 4096, "input_tokens": 12
        }));
        for k in ["x-api-key", "openai_api_key", "Proxy-Authorization", "github_token"] {
            assert_eq!(j[k], REDACTED, "{k}: {j}");
        }
        assert_eq!(j["max_tokens"], 4096, "counts survive");
        assert_eq!(j["input_tokens"], 12);
        let t = redact_str(concat!(
            "x-api-key: XKEY0123456789 openai_api_key=OKEY0123456789 \"api_key\": \"QKEY0123456789\" ",
            "proxy-authorization: Basic dXNlcjpwYXNzd29yZA== ",
            "https://alice:hunter2pass@example.com/x?api_key=QP123456&ok=1&access_token=AT123456 ",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlLXZhbHVl ",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 github_pat_11ABCDEFG0123456789_abcdefghijk ",
            "AIzaSyA-0123456789abcdefghijklmnopqrstu"
        ));
        for leak in [
            "XKEY",
            "OKEY",
            "QKEY",
            "dXNlcjpw",
            "alice",
            "hunter2",
            "QP123456",
            "AT123456",
            "eyJzdWIi",
            "ghp_ABC",
            "github_pat_11",
            "AIzaSyA",
        ] {
            assert!(!t.contains(leak), "{leak} leaked: {t}");
        }
        assert!(t.contains("example.com/x?api_key=[REDACTED]&ok=1"), "{t}");
    }

    #[test]
    fn bearer_with_embedded_token_shapes_and_suffixes_is_fully_redacted() {
        let sk = redact_str("Bearer sk-ABCDEFGHIJKLMNOPQRSTUVWX.suffix123");
        assert_eq!(sk, "Bearer [REDACTED]");
        let prefixed_sk = redact_str("Bearer A+sk-ABCDEFGHIJKLMNOPQRSTUVWX.suffix123");
        assert_eq!(prefixed_sk, "Bearer [REDACTED]");
        let akia = redact_str("Bearer tok_AKIAABCDEFGHIJKLMNOP/~suffix456==");
        assert_eq!(akia, "Bearer [REDACTED]");
        let b64 = redact_str("Bearer AbcdefghijklMN09._~+/-==; echo done");
        assert_eq!(b64, "Bearer [REDACTED]; echo done");
        let jwt = redact_str("Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlLXZhbHVl.extra");
        assert_eq!(jwt, "Bearer [REDACTED]");
        assert_eq!(redact_str("Bearer of good news"), "Bearer of good news");
        assert_eq!(redact_str("Bearer short1234567890"), "Bearer short1234567890");
    }

    #[test]
    fn pem_asia_env_suffix_json_cookies_and_provider_tokens_are_redacted() {
        let pem =
            "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAsecretline\n-----END RSA PRIVATE KEY-----\nafter";
        let red_pem = redact_str(pem);
        assert_eq!(red_pem, "before\n[REDACTED]\nafter");
        assert!(!red_pem.contains("secretline"));

        let samples = redact_str(concat!(
            "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY ",
            "ASIAABCDEFGHIJKLMNOP ",
            "{\"cookie\":\"sid=secret_cookie_val\",\"ok\":\"yes\",\"max_tokens\":4096} ",
            "xoxb-1234567890-abcdef ",
            "sk_live_51ABCDE0123456789abcdef ",
            "glpat-0123456789abcdefghij ",
            "npm_0123456789abcdefghijklmnop ",
            "hf_0123456789abcdefghijklmnop ",
            "ya29.a0AfB_byC0123456789abcdefghij"
        ));
        for leak in [
            "wJalrXUtnFEMI",
            "ASIAABCD",
            "secret_cookie_val",
            "xoxb-1234",
            "sk_live_51",
            "glpat-0123",
            "npm_0123",
            "hf_0123",
            "ya29.a0Af",
        ] {
            assert!(!samples.contains(leak), "{leak} leaked: {samples}");
        }
        assert!(samples.contains("\"ok\":\"yes\""), "cookie redaction must not eat rest of JSON: {samples}");
        assert!(samples.contains("\"max_tokens\":4096"), "numeric token counts survive: {samples}");
    }
}
