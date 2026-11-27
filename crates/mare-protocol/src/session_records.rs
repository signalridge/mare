//! Components shared by session records and public message payloads: a message, its
//! content blocks and its usage. Only the fields the host reads are typed; every other field,
//! and every block kind other than `text` and `tool_call`, is kept as it is.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Number, Value};

/// A message's `content`: a string or a list of blocks.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// One block of a message's content.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentBlock {
    Text(TextBlock),
    ToolCall(ToolCallBlock),
    /// Any other kind (`reasoning`, `image`, a provider's own): opaque, `type` included.
    Other(Map<String, Value>),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Token counts. Numbers keep their written form, so a record reads back as it was written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Number>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_aborted: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Content {
    /// The first text: the string itself, or the first `text` block's.
    pub fn first_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                ContentBlock::Text(TextBlock { text: Some(text), .. }) => Some(text.as_str()),
                _ => None,
            }),
        }
    }
}

impl Serialize for Content {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => serializer.serialize_str(text),
            Self::Blocks(blocks) => blocks.serialize(serializer),
        }
    }
}

// By hand rather than `untagged`: a bad block is reported with its own field, not as "data did
// not match any variant".
impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = Content;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string or a list of content blocks")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Content, E> {
                Ok(Content::Text(text.to_owned()))
            }
            fn visit_string<E: de::Error>(self, text: String) -> Result<Content, E> {
                Ok(Content::Text(text))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut blocks = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(block) = seq.next_element()? {
                    blocks.push(block);
                }
                Ok(Content::Blocks(blocks))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Content, A::Error> {
                // Lua encodes an empty untagged table (`content = {}`, e.g. a filtered refusal)
                // as `{}`; accept it as empty blocks while still rejecting non-empty maps.
                if map.next_key::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_type(de::Unexpected::Map, &self));
                }
                Ok(Content::Blocks(Vec::new()))
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Known<'a> {
            Text(&'a TextBlock),
            ToolCall(&'a ToolCallBlock),
        }
        match self {
            Self::Text(block) => Known::Text(block).serialize(serializer),
            Self::ToolCall(block) => Known::ToolCall(block).serialize(serializer),
            Self::Other(block) => block.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // the failing field of a typed block is named in the error, as for the rest of a record
        fn typed<T: de::DeserializeOwned, E: de::Error>(block: Map<String, Value>) -> Result<T, E> {
            serde_path_to_error::deserialize(Value::Object(block))
                .map_err(|error| E::custom(format!("{}: {}", error.path(), error.inner())))
        }
        struct BlockVisitor;
        impl<'de> Visitor<'de> for BlockVisitor {
            type Value = ContentBlock;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a content block object")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<ContentBlock, A::Error> {
                let mut block = Map::deserialize(de::value::MapAccessDeserializer::new(map))?;
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        block.remove("type");
                        typed(block).map(ContentBlock::Text)
                    }
                    Some("tool_call") => {
                        block.remove("type");
                        typed(block).map(ContentBlock::ToolCall)
                    }
                    _ => Ok(ContentBlock::Other(block)),
                }
            }
        }
        deserializer.deserialize_map(BlockVisitor)
    }
}
