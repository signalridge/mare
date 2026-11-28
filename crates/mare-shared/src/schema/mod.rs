//! Offline, bounded validator for the tool JSON Schema subset and structured outputs.

mod budget;
mod check;
mod equal;
mod multiple_of;
mod shape;
#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use budget::{build_validator, validate_with_budget};
use check::{
    PathSeg, check_contract_schema, check_contract_schema_at, check_output_schema, check_schema, check_value,
    check_value_at,
};
pub use equal::{json_equal, try_json_equal};
use serde_json::Value;
pub use shape::shape_empty;

const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 10_000;
const MAX_ENUM_VALUES: usize = 128;
const MAX_VALIDATION_STEPS: usize = 50_000;
const PATTERN_BACKTRACK_LIMIT: usize = 10_000;
const STEP_BUDGET_KEYWORD: &str = "__mare_step_budget__";

#[derive(Debug)]
struct SchemaInner {
    value: Value,
    validator: OnceLock<Result<jsonschema::Validator, String>>,
    is_object_root: bool,
    schema_nodes: usize,
    external: bool,
}

/// An owned tool schema snapshot. Compilation rejects unsupported or malformed keywords; the
/// underlying `jsonschema::Validator` is constructed lazily on first validation and shared across
/// clones so unused contract schemas pay no startup compilation or memory cost.
#[derive(Clone, Debug)]
pub struct Schema(Arc<SchemaInner>);

impl Schema {
    #[cfg(test)]
    pub(super) fn is_validator_built(&self) -> bool {
        self.0.validator.get().is_some()
    }

    pub fn compile(value: Value) -> Result<Self, String> {
        Self::compile_object(value, check_schema)
    }

    /// Compile an object-root external JSON Schema (such as an MCP tool `inputSchema`) using the
    /// bounded offline validator rather than the built-in tool schema keyword allowlist.
    pub fn compile_external(value: Value) -> Result<Self, String> {
        check_value(&value, "$", 0, &mut MAX_NODES.clone())?;
        if !value.is_object() {
            return Err("$: schema must be an object".into());
        }
        if value.get("type").and_then(Value::as_str) != Some("object") {
            return Err("$: root schema must have type object".into());
        }
        let mut budget = MAX_NODES;
        check_output_schema(&value, "$", &mut budget)?;
        let schema_nodes = MAX_NODES - budget;
        let validator = build_validator(&value, true)?;
        let cell = OnceLock::new();
        let _ = cell.set(Ok(validator));
        Ok(Self(Arc::new(SchemaInner { value, validator: cell, is_object_root: true, schema_nodes, external: true })))
    }

    pub fn is_external(&self) -> bool {
        self.0.external
    }

    pub fn is_object_root(&self) -> bool {
        self.0.is_object_root
    }

    /// Service contracts share the tool schema subset plus scalar bounds (`minimum`, `maximum`,
    /// `minLength`, `maxLength`) so RPC parameter ranges validate at the schema boundary without
    /// expanding LLM tool declarations.
    pub fn compile_contract(value: Value) -> Result<Self, String> {
        Self::compile_object(value, check_contract_schema)
    }

    /// Validate an object-root tool/event schema without taking ownership or allocating a `Schema`.
    pub fn validate_schema(value: &Value) -> Result<(), String> {
        Self::validate_object_with(value, check_schema).map(|_| ())
    }

    /// Validate an object-root contract schema without taking ownership or allocating a `Schema`.
    pub fn validate_contract_schema(value: &Value) -> Result<(), String> {
        Self::validate_object_with(value, check_contract_schema).map(|_| ())
    }

    /// Validate a contract result schema as if wrapped in
    /// `{"type":"object","properties":{"value":<value>},"required":["value"],"additionalProperties":false}`,
    /// without allocating or cloning the wrapper object.
    pub fn validate_contract_result_schema(value: &Value) -> Result<(), String> {
        let root = PathSeg::Root("$");
        let props = PathSeg::Key(&root, "properties");
        let val_seg = PathSeg::Key(&props, "value");
        // The wrapper object has 6 JSON nodes outside `value`: root object, `type: "object"`,
        // `properties: {...}`, `required: ["value"]` (array + item), `additionalProperties: false`.
        let mut val_budget = MAX_NODES - 6;
        check_value_at(value, &val_seg, 2, &mut val_budget)?;
        // In `check_schema_with`, the wrapper root consumes 1 schema node before descending into
        // `properties` -> `value` at depth 1.
        let mut schema_budget = MAX_NODES - 1;
        check_contract_schema_at(value, &val_seg, 1, &mut schema_budget)?;
        Ok(())
    }

    fn validate_object_with(
        value: &Value,
        checker: fn(&Value, &str, usize, &mut usize) -> Result<(), String>,
    ) -> Result<usize, String> {
        let is_object_root = value.get("type").and_then(Value::as_str) == Some("object");
        let schema_nodes = Self::check_value_with(value, checker)?;
        if !is_object_root {
            return Err("$: root schema must have type object".into());
        }
        Ok(schema_nodes)
    }

    fn compile_object(
        value: Value,
        checker: fn(&Value, &str, usize, &mut usize) -> Result<(), String>,
    ) -> Result<Self, String> {
        let schema_nodes = Self::validate_object_with(&value, checker)?;
        Ok(Self(Arc::new(SchemaInner {
            value,
            validator: OnceLock::new(),
            is_object_root: true,
            schema_nodes,
            external: false,
        })))
    }

    /// A canonical result may be any lossless JSON value; argument schemas remain objects.
    pub fn compile_value(value: Value) -> Result<Self, String> {
        Self::compile_value_with(value, check_schema)
    }

    pub fn compile_contract_value(value: Value) -> Result<Self, String> {
        Self::compile_value_with(value, check_contract_schema)
    }

    fn check_value_with(
        value: &Value,
        checker: fn(&Value, &str, usize, &mut usize) -> Result<(), String>,
    ) -> Result<usize, String> {
        check_value(value, "$", 0, &mut MAX_NODES.clone())?;
        let mut budget = MAX_NODES;
        checker(value, "$", 0, &mut budget)?;
        Ok(MAX_NODES - budget)
    }

    fn compile_value_with(
        value: Value,
        checker: fn(&Value, &str, usize, &mut usize) -> Result<(), String>,
    ) -> Result<Self, String> {
        let schema_nodes = Self::check_value_with(&value, checker)?;
        Ok(Self(Arc::new(SchemaInner {
            value,
            validator: OnceLock::new(),
            is_object_root: false,
            schema_nodes,
            external: false,
        })))
    }

    pub fn validate(&self, value: &Value) -> Result<(), String> {
        self.validate_with_node_limit(value, MAX_NODES)
    }

    /// Shape empty containers in `value` only when needed and validate using this schema's cached
    /// validator (avoiding compiling a throwaway validator inside `shape_empty`). Returns
    /// `Ok(reshaped)` indicating whether any empty container in `value` was mutated.
    pub fn shape_and_validate(&self, value: &mut Value) -> Result<bool, String> {
        self.shape_and_validate_with_node_limit(value, MAX_NODES)
    }

    /// Bounded variant of [`Self::shape_and_validate`].
    pub fn shape_and_validate_with_node_limit(&self, value: &mut Value, max_nodes: usize) -> Result<bool, String> {
        match self.validate_with_node_limit(value, max_nodes) {
            Ok(()) => Ok(false),
            Err(first_err) => {
                if !shape::has_empty_container(value) {
                    return Err(first_err);
                }
                let (_, changed) = shape::shape_empty_unchecked(value, &self.0.value);
                if !changed {
                    return Err(first_err);
                }
                self.validate_with_node_limit(value, max_nodes)?;
                Ok(true)
            }
        }
    }

    /// Shape and validate a service result value against this wrapped response schema
    /// (`{"type":"object","properties":{"value":<result_schema>},...}`) without cloning `value`.
    /// Returns `Ok(reshaped)` indicating whether any empty container in `value` was mutated.
    pub fn shape_and_validate_wrapped_result(
        &self,
        value: &mut Value,
        result_schema: &Value,
        max_nodes: usize,
    ) -> Result<bool, String> {
        let mut map = serde_json::Map::with_capacity(1);
        map.insert("value".to_string(), std::mem::take(value));
        let wrapped = Value::Object(map);
        let first = self.validate_with_node_limit(&wrapped, max_nodes);
        if let Value::Object(mut m) = wrapped {
            *value = m.remove("value").unwrap_or(Value::Null);
        }
        match first {
            Ok(()) => Ok(false),
            Err(first_err) => {
                if !shape::has_empty_container(value) {
                    return Err(first_err);
                }
                let (_, changed) = shape::shape_empty_unchecked(value, result_schema);
                if !changed {
                    return Err(first_err);
                }
                let mut map = serde_json::Map::with_capacity(1);
                map.insert("value".to_string(), std::mem::take(value));
                let wrapped = Value::Object(map);
                let second = self.validate_with_node_limit(&wrapped, max_nodes);
                if let Value::Object(mut m) = wrapped {
                    *value = m.remove("value").unwrap_or(Value::Null);
                }
                second?;
                Ok(true)
            }
        }
    }

    /// Validate an in-process result with an explicit finite JSON-node budget.
    /// The validation-step budget scales with it; the default wire limit is unchanged.
    pub fn validate_with_node_limit(&self, value: &Value, max_nodes: usize) -> Result<(), String> {
        if self.0.is_object_root && !value.is_object() {
            return Err("$: arguments must be an object".into());
        }
        let mut val_budget = max_nodes;
        check_value(value, "$", 0, &mut val_budget)?;
        let instance_nodes = max_nodes - val_budget;
        let steps = max_nodes.saturating_mul(MAX_VALIDATION_STEPS / MAX_NODES);
        let validator = match self.0.validator.get_or_init(|| build_validator(&self.0.value, false)) {
            Ok(v) => v,
            Err(e) => return Err(e.clone()),
        };
        validate_with_budget(validator, value, self.0.schema_nodes, instance_nodes, steps)
    }
}

/// An owned structured-output JSON Schema snapshot.
#[derive(Clone, Debug)]
pub struct OutputSchema {
    value: Value,
    validator: jsonschema::Validator,
    schema_nodes: usize,
}

impl OutputSchema {
    pub fn compile(value: Value) -> Result<Self, String> {
        check_value(&value, "$", 0, &mut MAX_NODES.clone())?;
        if !value.is_object() {
            return Err("$: schema must be an object".into());
        }
        let mut budget = MAX_NODES;
        check_output_schema(&value, "$", &mut budget)?;
        let schema_nodes = MAX_NODES - budget;
        let validator = build_validator(&value, true)?;
        Ok(Self { value, validator, schema_nodes })
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn validate(&self, value: &Value) -> Result<(), String> {
        let mut val_budget = MAX_NODES;
        check_value(value, "$", 0, &mut val_budget)?;
        let instance_nodes = MAX_NODES - val_budget;
        validate_with_budget(&self.validator, value, self.schema_nodes, instance_nodes, MAX_VALIDATION_STEPS)
    }

    pub fn validate_json_text(&self, text: &str) -> Result<Value, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("response is not valid JSON: {e}"))?;
        self.validate(&value)?;
        Ok(value)
    }
}
