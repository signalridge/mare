//! Schema-directed removal, distinct from heuristic log redaction. JSON Schema structural
//! positions plus the explicit `x-mare-secret: true` annotation; never executes schema code.
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Debug)]
pub struct SecretPosition {
    pub path: Vec<String>,
    pub set: bool,
}
#[derive(Debug)]
pub struct RedactedValue {
    pub value: Option<Value>,
    pub secrets: Vec<SecretPosition>,
}

impl RedactedValue {
    pub fn into_json(self) -> Value {
        let mut result = Map::new();
        if let Some(value) = self.value {
            result.insert("value".into(), value);
        }
        result.insert(
            "secrets".into(),
            Value::Array(self.secrets.into_iter().map(|s| serde_json::json!({"path":s.path,"set":s.set})).collect()),
        );
        Value::Object(result)
    }
}

fn consume(depth: usize, budget: &mut usize) -> Result<(), String> {
    if depth > 64 || *budget == 0 {
        return Err("schema redaction exceeds depth/node limit".into());
    }
    *budget -= 1;
    Ok(())
}
fn check(node: &Value, depth: usize, budget: &mut usize) -> Result<(), String> {
    consume(depth, budget)?;
    let obj = node.as_object().ok_or("redaction schema must be an object")?;
    for (key, value) in obj {
        match key.as_str() {
            "x-mare-secret" if value.is_boolean() => {}
            "type" => {
                let names = match value {
                    Value::String(_) => vec![value],
                    Value::Array(a) if !a.is_empty() => a.iter().collect(),
                    _ => return Err("invalid redaction schema type".into()),
                };
                if names.iter().any(|t| {
                    !matches!(
                        t.as_str(),
                        Some("object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
                    )
                }) {
                    return Err("invalid redaction schema type".into());
                }
            }
            "properties" => {
                for child in value.as_object().ok_or("properties must be an object")?.values() {
                    check(child, depth + 1, budget)?;
                }
            }
            "items" => check(value, depth + 1, budget)?,
            "additionalProperties" if value.is_boolean() => {}
            "additionalProperties" => check(value, depth + 1, budget)?,
            "anyOf" | "oneOf" | "allOf" => {
                for child in value.as_array().ok_or("schema branches must be an array")? {
                    check(child, depth + 1, budget)?;
                }
            }
            // Inert validation/description keywords: these contain user data, never schemas.
            "title" | "description" | "default" | "examples" | "enum" | "const" | "required" | "format" | "pattern"
            | "minLength" | "maxLength" | "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
            | "minItems" | "maxItems" | "uniqueItems" | "minProperties" | "maxProperties" | "x-mare-order" => {}
            _ => return Err("unsupported or malformed redaction schema keyword".into()),
        }
    }
    Ok(())
}
struct Walker {
    secrets: Vec<SecretPosition>,
    positions: HashMap<Vec<String>, usize>,
    budget: usize,
}
impl Walker {
    fn walk(
        &mut self,
        node: &Value,
        value: Option<Value>,
        path: &mut Vec<String>,
        depth: usize,
    ) -> Result<Option<Value>, String> {
        consume(depth, &mut self.budget)?;
        if node.get("x-mare-secret").and_then(Value::as_bool) == Some(true) {
            if let Some(index) = self.positions.get(path) {
                self.secrets[*index].set |= value.is_some();
            } else {
                self.positions.insert(path.clone(), self.secrets.len());
                self.secrets.push(SecretPosition { path: path.clone(), set: value.is_some() });
            }
            return Ok(None);
        }
        let mut result = value;
        if node.get("properties").is_some()
            || node.get("additionalProperties").is_some()
            || node.get("type").and_then(Value::as_str) == Some("object")
        {
            let source = result.as_ref().and_then(Value::as_object);
            let mut rebuilt = Map::new();
            let properties = node.get("properties").and_then(Value::as_object);
            if let Some(source) = source {
                for (key, entry) in source {
                    if properties.is_some_and(|p| p.contains_key(key)) {
                        continue;
                    }
                    if let Some(child) = node.get("additionalProperties").filter(|v| v.is_object()) {
                        path.push(key.clone());
                        let stripped = self.walk(child, Some(entry.clone()), path, depth + 1)?;
                        path.pop();
                        if let Some(stripped) = stripped {
                            rebuilt.insert(key.clone(), stripped);
                        }
                    } else {
                        rebuilt.insert(key.clone(), entry.clone());
                    }
                }
            }
            if let Some(properties) = properties {
                for (key, child) in properties {
                    path.push(key.clone());
                    let stripped = self.walk(child, source.and_then(|v| v.get(key)).cloned(), path, depth + 1)?;
                    path.pop();
                    if let Some(stripped) = stripped {
                        rebuilt.insert(key.clone(), stripped);
                    }
                }
            }
            if source.is_some() || !rebuilt.is_empty() {
                result = Some(Value::Object(rebuilt));
            }
        }
        if let Some(Value::Array(source)) = result.as_ref() {
            let mut rebuilt = Vec::with_capacity(source.len());
            for (index, entry) in source.iter().enumerate() {
                let child = node.get("items");
                if let Some(child) = child {
                    path.push(index.to_string());
                    let stripped = self.walk(child, Some(entry.clone()), path, depth + 1)?;
                    path.pop();
                    rebuilt.push(stripped.unwrap_or(Value::Null));
                } else {
                    rebuilt.push(entry.clone());
                }
            }
            result = Some(Value::Array(rebuilt));
        }
        // Conservatively visit every union/intersection branch, never executing predicates.
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(branches) = node.get(key).and_then(Value::as_array) {
                for child in branches {
                    result = self.walk(child, result, path, depth + 1)?;
                }
            }
        }
        Ok(result)
    }
}
/// Remove every annotated schema position. Missing is `None`; JSON null is present (`set=true`).
/// Array removal leaves null to preserve indexes. Unsupported schema constructs fail closed.
pub fn redact(schema: &Value, value: Option<Value>) -> Result<RedactedValue, String> {
    check(schema, 0, &mut 10_000)?;
    let mut walker = Walker { secrets: Vec::new(), positions: HashMap::new(), budget: 10_000 };
    let value = walker.walk(schema, value, &mut Vec::new(), 0)?;
    Ok(RedactedValue { value, secrets: walker.secrets })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn removes_schema_fields_and_describes_present_and_missing_slots() {
        let schema = json!({"type":"object","properties":{
            "key":{"x-mare-secret":true},"unset":{"x-mare-secret":true},
            "optional":{"type":"object","properties":{"inner":{"x-mare-secret":true}}},
            "records":{"type":"object","additionalProperties":{"x-mare-secret":true}},
            "rows":{"type":"array","items":{"type":"object","properties":{"value":{"x-mare-secret":true}}}}
        }});
        let input = json!({"key":null,"public":"keep","records":{"a":"fake-one"},"rows":[{"value":"fake-two","id":1}]});
        let result = redact(&schema, Some(input.clone())).unwrap();
        assert_eq!(result.value, Some(json!({"public":"keep","records":{},"rows":[{"id":1}]})));
        assert_eq!(input["records"]["a"], "fake-one");
        let slots: HashMap<_, _> = result.secrets.into_iter().map(|s| (s.path, s.set)).collect();
        assert!(slots[&vec!["key".into()]]);
        assert!(!slots[&vec!["unset".into()]]);
        assert!(!slots[&vec!["optional".into(), "inner".into()]]);
        assert!(slots[&vec!["records".into(), "a".into()]]);
        assert!(slots[&vec!["rows".into(), "0".into(), "value".into()]]);
    }
    #[test]
    fn unions_merge_set_conservatively_and_arrays_keep_indexes() {
        let schema = json!({"anyOf":[{"properties":{"x":{"x-mare-secret":true}}},{"properties":{"x":{"x-mare-secret":true},"y":{"x-mare-secret":true}}}]});
        let result = redact(&schema, Some(json!({"x":"fake"}))).unwrap();
        assert_eq!(result.secrets.len(), 2);
        assert!(result.secrets[0].set);
        assert!(!result.secrets[1].set);
        let result = redact(&json!({"items":{"x-mare-secret":true}}), Some(json!(["fake", null]))).unwrap();
        assert_eq!(result.value, Some(json!([null, null])));
        assert!(result.secrets.iter().all(|s| s.set));
        assert!(redact(&json!({"$ref":"unhandled"}), None).is_err());
        assert!(redact(&json!({"x-mare-secret":"yes"}), None).is_err());
        assert!(!redact(&json!({"x-mare-secret":true}), None).unwrap().secrets[0].set);
    }
}
