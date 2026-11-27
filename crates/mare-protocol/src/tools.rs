//! Tool result wire types for contract schemas.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Host tool content is lossless text or the public text/image content blocks.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

impl Default for ToolResultContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolResultBlock {
    Text { text: String },
    Image { data: String, mime: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_content_default_and_roundtrip() {
        let def = ToolResultContent::default();
        assert!(matches!(&def, ToolResultContent::Text(s) if s.is_empty()));
        assert_eq!(serde_json::to_string(&def).unwrap(), r#""""#);

        let blocks = ToolResultContent::Blocks(vec![
            ToolResultBlock::Text { text: "ok".into() },
            ToolResultBlock::Image { data: "AQID".into(), mime: "image/png".into() },
        ]);
        let raw = serde_json::to_value(&blocks).unwrap();
        let parsed: ToolResultContent = serde_json::from_value(raw).unwrap();
        assert!(matches!(
            parsed,
            ToolResultContent::Blocks(ref b)
                if matches!(b.as_slice(), [ToolResultBlock::Text { text }, ToolResultBlock::Image { data, mime }]
                    if text == "ok" && data == "AQID" && mime == "image/png")
        ));
    }
}
