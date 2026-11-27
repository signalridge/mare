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
    let raw = match serde_json::from_str::<Value>(&text) {
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
}
