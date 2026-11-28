//! Read JSON objects with an explicit format version without exposing scalar contents.

use std::path::Path;

use serde_json::{Map, Value};

/// A missing file is absent; existing files must be readable versioned objects.
pub fn read(path: &Path, expected_version: u64) -> Result<Option<Map<String, Value>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(e) => return Err(format!("{} cannot be read: {e}", path.display())),
    };
    let raw = match serde_json::from_str::<Value>(mare_shared::strip_utf8_bom(&text)) {
        Ok(Value::Object(raw)) => raw,
        Ok(_) => return Err(format!("{} is corrupt (not a JSON object)", path.display())),
        Err(e) => return Err(format!("{} is corrupt (not valid JSON: {e})", path.display())),
    };
    let Some(version) = raw.get("version").and_then(Value::as_u64) else {
        return Err(format!("{} is corrupt (missing or invalid version)", path.display()));
    };
    if version != expected_version {
        return Err(format!("{} is corrupt (unsupported version {version})", path.display()));
    }
    Ok(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_a_directory_ancestor_is_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"regular file").unwrap();
        let res = read(&file.join("state.json"), 1);
        assert_eq!(res, Ok(None), "NotADirectory must be treated as absent: {res:?}");
    }

    #[test]
    fn leading_utf8_bom_is_accepted_and_double_bom_is_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let single = dir.path().join("single-bom.json");
        std::fs::write(&single, "\u{FEFF}{\"version\":1,\"model\":\"openai/gpt-5\"}\n").unwrap();
        let map = read(&single, 1).expect("single UTF-8 BOM must parse").expect("file exists");
        assert_eq!(map.get("model").and_then(Value::as_str), Some("openai/gpt-5"));

        let double = dir.path().join("double-bom.json");
        std::fs::write(&double, "\u{FEFF}\u{FEFF}{\"version\":1}\n").unwrap();
        let err = read(&double, 1).unwrap_err();
        assert!(err.contains("not valid JSON"), "double BOM must fail: {err}");

        let wrong_ver = dir.path().join("bom-wrong-ver.json");
        std::fs::write(&wrong_ver, "\u{FEFF}{\"version\":2}\n").unwrap();
        let err = read(&wrong_ver, 1).unwrap_err();
        assert!(err.contains("unsupported version 2"), "version check must still run: {err}");
    }
}
