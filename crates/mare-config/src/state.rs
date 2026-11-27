//! Shared reader for the versioned runtime choices in `state.json`.

use std::path::Path;

use serde_json::{Map, Value};

/// `state.json` v1 stores the runtime choices alongside this reserved metadata field.
pub const VERSION: u64 = 1;

/// A missing file has no choices. Any existing file must be a supported versioned object.
pub fn read(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let mut raw = crate::versioned_json::read(path, VERSION)?;
    if let Some(raw) = &mut raw {
        raw.remove("version");
    }
    Ok(raw)
}
