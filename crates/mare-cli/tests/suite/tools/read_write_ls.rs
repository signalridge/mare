//! Built-in `read`, `write`, and `ls` tool tests.

use super::{call, host, text};
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn read_returns_plain_lines_with_offset_and_limit() {
    local(async {
        let env = TestEnv::new().await;
        let body: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(env.cwd.join("f.txt"), body).unwrap();
        let h = host(&env).await;
        let r = call(&h, "read", json!({ "path": "f.txt", "offset": 10, "limit": 3 })).await;
        let t = text(&r);
        assert!(t.starts_with("line 10\nline 11\nline 12\n"), "{t}");
        assert!(t.contains("continue with offset=13"), "{t}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn read_truncates_single_lines_over_2000_bytes_on_utf8_boundaries() {
    local(async {
        let env = TestEnv::new().await;
        let exact_2000 = "a".repeat(2000);
        let long_ascii = "b".repeat(2500);
        // 1998 ASCII bytes + 3-byte CJK char '中' (bytes 1998..2001) + trailing chars:
        // cutting at 2000 bytes must step back to 1998 without splitting '中'.
        let utf8_boundary_3 = format!("{}中trailing", "c".repeat(1998));
        // 1999 ASCII bytes + 4-byte emoji '🦀' (bytes 1999..2003) + trailing chars:
        // cutting at 2000 bytes must step back to 1999 without splitting '🦀'.
        let utf8_boundary_4 = format!("{}🦀trailing", "d".repeat(1999));
        let body = format!("{exact_2000}\n{long_ascii}\n{utf8_boundary_3}\n{utf8_boundary_4}\n");
        std::fs::write(env.cwd.join("long_lines.txt"), body).unwrap();

        let h = host(&env).await;
        let r = call(&h, "read", json!({ "path": "long_lines.txt" })).await;
        assert!(r["is_error"] != true, "{r}");
        let t = text(&r);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 4, "{t}");
        assert_eq!(lines[0], exact_2000, "2000-byte line must not be truncated");
        assert_eq!(lines[1], format!("{} ... [line truncated at 2000 bytes]", "b".repeat(2000)));
        assert_eq!(lines[2], format!("{} ... [line truncated at 2000 bytes]", "c".repeat(1998)));
        assert_eq!(lines[3], format!("{} ... [line truncated at 2000 bytes]", "d".repeat(1999)));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn read_line_cap_follows_the_tools_option() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("long.txt"), format!("{}\nshort\n", "x".repeat(300))).unwrap();
        let h = env
            .host()
            .no_session()
            .config(json!({ "plugins": { "tools": { "read_max_line_bytes": 100 } } }))
            .build()
            .await;
        expose(&h);
        let r = call(&h, "read", json!({ "path": "long.txt" })).await;
        let t = text(&r);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], format!("{} ... [line truncated at 100 bytes]", "x".repeat(100)), "{t}");
        assert_eq!(lines[1], "short", "{t}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn read_of_an_image_returns_an_image_block() {
    local(async {
        let env = TestEnv::new().await;
        // 1×1 transparent PNG
        const PNG: [u8; 67] = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
            0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d,
            0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(env.cwd.join("dot.png"), PNG).unwrap();
        let h = host(&env).await;
        let r = call(&h, "read", json!({ "path": "dot.png" })).await;
        assert!(r["is_error"] != true, "{r}");
        assert!(r["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "image")), "{r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn write_creates_parent_directories() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        // a new file needs no prior read
        let r = call(&h, "write", json!({ "path": "deep/er/new.txt", "content": "hi\n" })).await;
        assert!(r["is_error"] != true, "{r}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("deep/er/new.txt")).unwrap(), "hi\n");
        h.shutdown().await;
    })
    .await
}

/// `fs/write-intent` is a decision point (dispatch/policy.rs POLICY_POINTS): with no voter the
/// write goes through (decide = allow); a deny voter refuses it with its reason.
#[tokio::test]
async fn edit_and_write_intents_are_decided_by_their_voters() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        for (tool, point) in [("write", "fs/write-intent"), ("edit", "fs/edit-intent")] {
            let (path, file) = (format!("{tool}.txt"), env.cwd.join(format!("{tool}.txt")));
            std::fs::write(&file, "a\n").unwrap();
            call(&h, "read", json!({ "path": path })).await;
            let attempt = |to: &str| match tool {
                "write" => json!({ "path": path, "content": format!("{to}\n") }),
                _ => json!({ "path": path, "old_string": std::fs::read_to_string(&file).unwrap().trim(), "new_string": to }),
            };
            let r = call(&h, tool, attempt("b")).await;
            assert!(r["is_error"] != true, "{tool} with no voter: {r}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "b\n");
            exec(&h, &format!(r#"OFF = C.cordis:get("policy"):voter("{point}", function() return {{ decision = "deny", reason = "x" }} end)"#))
                .await;
            let r = call(&h, tool, attempt("c")).await;
            assert!(r["is_error"] == true && text(&r).contains(": x"), "{tool} under a deny voter: {r}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "b\n", "{tool}: a refused change leaves the file");
            exec(&h, "OFF()").await;
            let r = call(&h, tool, attempt("d")).await;
            assert!(r["is_error"] != true, "{tool} after the voter is disposed: {r}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "d\n");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn ls_limit_notes_the_remaining_entries() {
    local(async {
        let env = TestEnv::new().await;
        for i in 0..12 {
            std::fs::write(env.cwd.join(format!("f{i:02}")), "").unwrap();
        }
        let h = host(&env).await;
        let t = text(&call(&h, "ls", json!({ "limit": 5 })).await);
        assert!(t.contains("[… 7 more entries]"), "{t}");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn binary_files_and_binary_output_are_refused_or_cleaned_with_a_note() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("blob.bin"), b"\x7fELF\x02\x01\x01\0\0\0junk").unwrap();
        let h = host(&env).await;
        let r = call(&h, "read", json!({ "path": "blob.bin" })).await;
        assert!(
            r["is_error"] == true
                && text(&r).contains("ELF executable")
                && text(&r).contains("14 bytes")
                && r["error_code"] == "binary_file",
            "{r}"
        );
        let r = call(&h, "bash", json!({ "command": "printf 'a\\000b\\001c\\n'" })).await;
        let t = text(&r);
        assert!(t.contains("binary data") && t.contains("abc") && !t.contains('\0'), "{t:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn read_rejects_non_positive_offset_and_limit() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("sample.txt"), "one\ntwo\nthree\n").unwrap();
        let h = host(&env).await;
        for args in [
            json!({ "path": "sample.txt", "offset": 0 }),
            json!({ "path": "sample.txt", "offset": -1 }),
            json!({ "path": "sample.txt", "limit": 0 }),
            json!({ "path": "sample.txt", "limit": -5 }),
        ] {
            let r = call(&h, "read", args.clone()).await;
            assert!(r["is_error"] == true, "expected error for {args}: {r}");
        }
        h.shutdown().await;
    })
    .await
}

/// Item 9 (应修, self-written): `read` must reject non-positive offset/limit even on
/// directories and images instead of returning early before validation.
#[tokio::test]
async fn read_rejects_non_positive_offset_and_limit_on_dirs_and_images() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("sub")).unwrap();
        // Minimal valid 1x1 PNG
        std::fs::write(
            env.cwd.join("dot.png"),
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90wS\xde\x00\x00\x00\x0cIDATx\x9cc\xf8\xcf\xc0\x00\x00\x03\x01\x01\x00\xc9\xfe\x92\xef\x00\x00\x00\x00IEND\xaeB`\x82",
        )
        .unwrap();
        let h = host(&env).await;
        for args in [
            json!({ "path": "sub", "limit": 0 }),
            json!({ "path": "sub", "offset": 0 }),
            json!({ "path": "dot.png", "limit": 0 }),
            json!({ "path": "dot.png", "offset": -1 }),
        ] {
            let r = call(&h, "read", args.clone()).await;
            assert!(r["is_error"] == true, "expected error for {args}: {r}");
        }
        h.shutdown().await;
    })
    .await
}
