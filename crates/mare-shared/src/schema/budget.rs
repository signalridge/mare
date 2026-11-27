use super::multiple_of::ExactMultipleOfKeyword;
use super::{PATTERN_BACKTRACK_LIMIT, STEP_BUDGET_KEYWORD};
use serde_json::Value;
use std::cell::Cell;

thread_local! {
    static VALIDATION_BUDGET: Cell<Option<(usize, bool)>> = const { Cell::new(None) };
    #[cfg(test)]
    pub(super) static STEP_KEYWORD_CALLS: Cell<usize> = const { Cell::new(0) };
}

fn consume_validation_step(weight: usize) -> bool {
    #[cfg(test)]
    STEP_KEYWORD_CALLS.with(|c| c.set(c.get().saturating_add(1)));
    VALIDATION_BUDGET.with(|cell| {
        let Some((remaining, exceeded)) = cell.get() else {
            return true;
        };
        if exceeded || remaining < weight {
            cell.set(Some((0, true)));
            false
        } else {
            cell.set(Some((remaining - weight, false)));
            true
        }
    })
}

struct StepBudgetGuard {
    previous: Option<(usize, bool)>,
}

impl StepBudgetGuard {
    fn new(budget: usize) -> Self {
        let previous = VALIDATION_BUDGET.with(|cell| cell.replace(Some((budget, false))));
        Self { previous }
    }

    fn exceeded(&self) -> bool {
        VALIDATION_BUDGET.with(|cell| cell.get().is_some_and(|(_, exceeded)| exceeded))
    }
}

impl Drop for StepBudgetGuard {
    fn drop(&mut self) {
        VALIDATION_BUDGET.with(|cell| cell.set(self.previous));
    }
}

struct StepBudgetKeyword {
    weight: usize,
}

impl<'i> jsonschema::Keyword<'i> for StepBudgetKeyword {
    fn validate(&self, _instance: &'i Value) -> Result<(), jsonschema::ValidationError<'i>> {
        if consume_validation_step(self.weight) {
            Ok(())
        } else {
            Err(jsonschema::ValidationError::custom("schema validation resource limit exceeded"))
        }
    }

    fn is_valid(&self, _instance: &'i Value) -> bool {
        consume_validation_step(self.weight)
    }
}

fn instrument_schema_map(val: Option<&mut Value>, is_output_schema: bool) {
    if let Some(Value::Object(map)) = val {
        for sub in map.values_mut() {
            instrument_schema(sub, is_output_schema);
        }
    }
}

fn instrument_schema_list(val: Option<&mut Value>, is_output_schema: bool) {
    if let Some(Value::Array(arr)) = val {
        for sub in arr.iter_mut() {
            instrument_schema(sub, is_output_schema);
        }
    }
}

fn instrument_schema(v: &mut Value, is_output_schema: bool) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };
    let mut weight = 1usize;
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(arr)) = obj.get(key) {
            weight = weight.saturating_add(arr.len());
        }
    }
    instrument_schema_map(obj.get_mut("properties"), is_output_schema);
    if let Some(sub) = obj.get_mut("additionalProperties")
        && sub.is_object()
    {
        instrument_schema(sub, is_output_schema);
    }
    if let Some(items) = obj.get_mut("items") {
        if items.is_array() {
            instrument_schema_list(Some(items), is_output_schema);
        } else {
            instrument_schema(items, is_output_schema);
        }
    }
    instrument_schema_list(obj.get_mut("oneOf"), is_output_schema);

    if is_output_schema {
        for key in ["not", "if", "then", "else", "contentSchema"] {
            if let Some(sub) = obj.get_mut(key) {
                instrument_schema(sub, true);
            }
        }
        for key in ["allOf", "anyOf", "prefixItems"] {
            instrument_schema_list(obj.get_mut(key), true);
        }
        for key in ["patternProperties", "$defs", "definitions"] {
            instrument_schema_map(obj.get_mut(key), true);
        }
        for key in ["dependentSchemas", "dependencies"] {
            if let Some(Value::Object(map)) = obj.get_mut(key) {
                for sub in map.values_mut() {
                    if sub.is_object() {
                        instrument_schema(sub, true);
                    }
                }
            }
        }
        for key in ["unevaluatedProperties", "additionalItems", "unevaluatedItems", "contains", "propertyNames"] {
            if let Some(sub) = obj.get_mut(key)
                && sub.is_object()
            {
                instrument_schema(sub, true);
            }
        }
    }

    obj.insert(STEP_BUDGET_KEYWORD.to_string(), Value::from(weight as u64));
}

struct OfflineRetriever;

impl jsonschema::Retrieve for OfflineRetriever {
    fn retrieve(&self, uri: &jsonschema::Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("external $ref {uri:?} is not supported (offline validator)").into())
    }
}

pub(super) fn build_validator(schema: &Value, is_output_schema: bool) -> Result<jsonschema::Validator, String> {
    let mut instrumented = schema.clone();
    instrument_schema(&mut instrumented, is_output_schema);
    jsonschema::options()
        .with_retriever(OfflineRetriever)
        .with_pattern_options(jsonschema::PatternOptions::fancy_regex().backtrack_limit(PATTERN_BACKTRACK_LIMIT))
        .with_keyword(STEP_BUDGET_KEYWORD, |_, val, _| {
            let weight = val.as_u64().and_then(|n| usize::try_from(n).ok()).unwrap_or(1).max(1);
            Ok(Box::new(StepBudgetKeyword { weight }))
        })
        .with_keyword("multipleOf", |_, val, _| Ok(Box::new(ExactMultipleOfKeyword::build(val)?)))
        .build(&instrumented)
        .map_err(|e| format!("$: {e}"))
}

pub(super) fn validate_with_budget(
    validator: &jsonschema::Validator,
    value: &Value,
    schema_nodes: usize,
    instance_nodes: usize,
    max_steps: usize,
) -> Result<(), String> {
    let guard = StepBudgetGuard::new(max_steps);
    // Run `is_valid` first so budget exhaustion or valid inputs never trigger `collect_errors`,
    // which in `anyOf`/`oneOf` unconditionally traverses every failing branch.
    let valid = validator.is_valid(value);
    if guard.exceeded() {
        return Err("$: schema validation resource limit exceeded".into());
    }
    if valid {
        return Ok(());
    }
    if schema_nodes.saturating_mul(instance_nodes) > max_steps {
        return Err("$: value does not match schema".into());
    }
    let result = validator.validate(value);
    if guard.exceeded() {
        return Err("$: schema validation resource limit exceeded".into());
    }
    result.map_err(|e| format_validation_error(&e))
}

fn format_validation_error(err: &jsonschema::ValidationError<'_>) -> String {
    let instance_path = err.instance_path().as_str();
    let path = if instance_path.is_empty() { "$" } else { &format!("${instance_path}") };
    format!("{path}: {err}")
}
