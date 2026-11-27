//! An empty Lua table has no shape of its own: converted without a schema it is `{}`, and a
//! table marked with `mare.json.array()` is `[]`. Where a contract's schema says which one a
//! position holds, the empty value takes that shape (as Neovim's typed conversion takes `{}` for a
//! Dict, src/nvim/lua/converter.c:886-889). Positions the schema leaves open keep the encoding
//! they had. Only empty containers change; anything with content already has its shape.
//!
//! Only for values converted from Lua: JSON a caller outside the process sent is validated as it
//! is. The value was bounded when it was converted (STRICT_JSON_MAX_NODES); shaping itself has a
//! step budget that counts every node it visits and every `oneOf` branch it tries, and stops
//! shaping (the rest keeps its encoding) when the budget is spent.
use serde_json::{Map, Value};

/// Steps one shaping may take (nodes visited plus the size of every branch trial).
pub const SHAPE_STEPS: usize = 50_000;

/// Which container kinds `schema` admits at this position: (array, object). An open position
/// (no `type`, or a `oneOf` with an open branch) admits neither for this purpose.
fn admits(schema: &Value) -> (bool, bool) {
    let mut kinds = (false, false);
    let mut open = true;
    match schema.get("type") {
        Some(Value::String(kind)) => {
            open = false;
            kinds = (kind == "array", kind == "object");
        }
        Some(Value::Array(types)) => {
            open = false;
            for kind in types.iter().filter_map(Value::as_str) {
                kinds.0 |= kind == "array";
                kinds.1 |= kind == "object";
            }
        }
        _ => {}
    }
    // contracts accept `oneOf` only (the schema subset has no `anyOf`)
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let (mut array, mut object, mut any_open) = (false, false, false);
        for branch in branches {
            let (a, o) = admits(branch);
            if !a && !o && branch.get("type").is_none() && branch.get("oneOf").is_none() {
                any_open = true;
            }
            array |= a;
            object |= o;
        }
        if !any_open {
            kinds = if open { (array, object) } else { (kinds.0 && array, kinds.1 && object) };
            open = false;
        }
    }
    if open { (false, false) } else { kinds }
}

fn valid(value: &Value, schema: &Value) -> bool {
    super::Schema::compile_contract_value(schema.clone()).is_ok_and(|check| check.validate(value).is_ok())
}

fn size(value: &Value) -> usize {
    1 + match value {
        Value::Object(map) => map.values().map(size).sum(),
        Value::Array(items) => items.iter().map(size).sum(),
        _ => 0,
    }
}

struct Budget(usize);
impl Budget {
    fn spend(&mut self, steps: usize) -> bool {
        if self.0 < steps {
            self.0 = 0;
            return false;
        }
        self.0 -= steps;
        true
    }
}

/// Whether `value` contains any empty object or empty array anywhere in its tree.
/// Shaping only ever mutates empty containers, so a tree with none is already shaped.
pub(crate) fn has_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.is_empty() || map.values().any(has_empty_container),
        Value::Array(items) => items.is_empty() || items.iter().any(has_empty_container),
        _ => false,
    }
}

/// Give every empty container in `value` the shape `schema` declares for its position. False
/// when the step budget ran out (what was not reached keeps its encoding).
pub fn shape_empty(value: &mut Value, schema: &Value) -> bool {
    if !has_empty_container(value) {
        return true;
    }
    // a value the schema accepts as it is keeps its shape: JSON a caller sent and validated,
    // or a Lua value whose encoding already fits, is never reshaped
    if valid(value, schema) {
        return true;
    }
    let (within_budget, _) = shape_empty_unchecked(value, schema);
    within_budget
}

/// Run empty-container shaping without re-validating `value` against `schema` at the root
/// (callers that already ran a cached [`super::Schema`] validator know `value` did not validate
/// as-is). Returns `(within_budget, changed)`.
pub(crate) fn shape_empty_unchecked(value: &mut Value, schema: &Value) -> (bool, bool) {
    let mut budget = Budget(SHAPE_STEPS);
    let mut changed = false;
    shape(value, schema, &mut budget, &mut changed);
    (budget.0 > 0, changed)
}

fn shape(value: &mut Value, schema: &Value, budget: &mut Budget, changed: &mut bool) {
    if !budget.spend(1) {
        return;
    }
    let (array, object) = admits(schema);
    let empty_object = matches!(value, Value::Object(map) if map.is_empty());
    let empty_array = matches!(value, Value::Array(items) if items.is_empty());
    // under oneOf only the one satisfied branch decides (satisfied_branch below)
    let decided = schema.get("oneOf").is_none();
    if decided && empty_object && array && !object {
        *value = Value::Array(Vec::new());
        *changed = true;
        return;
    }
    if decided && empty_array && object && !array {
        *value = Value::Object(Map::new());
        *changed = true;
        return;
    }
    // the position's own properties and items, then the one branch the value satisfies
    shape_children(value, schema, budget, changed);
    if (value.is_object() || value.is_array())
        && let Some(shaped) = satisfied_branch(value, schema, budget)
    {
        if shaped != *value {
            *changed = true;
        }
        *value = shaped;
    }
}

fn shape_children(value: &mut Value, schema: &Value, budget: &mut Budget, changed: &mut bool) {
    match value {
        Value::Object(map) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let extra = schema.get("additionalProperties").filter(|extra| extra.is_object());
            for (key, child) in map.iter_mut() {
                if let Some(child_schema) = properties.and_then(|properties| properties.get(key)).or(extra) {
                    shape(child, child_schema, budget, changed);
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) {
                for item in items {
                    shape(item, item_schema, budget, changed);
                }
            }
        }
        _ => {}
    }
}

/// For a container under `oneOf`: the value shaped by the one branch it satisfies once shaped.
/// The value's own fields decide (a `const` tag, required fields, ...), not only its container
/// kind. None when no branch or several branches are satisfied (the value keeps its shape and
/// validation says what is wrong), or when the budget runs out.
fn satisfied_branch(value: &Value, schema: &Value, budget: &mut Budget) -> Option<Value> {
    let branches = schema.get("oneOf").and_then(Value::as_array)?;
    let is_array = value.is_array();
    let mut found: Option<Value> = None;
    let empty = matches!(value, Value::Object(map) if map.is_empty())
        || matches!(value, Value::Array(items) if items.is_empty());
    for branch in branches {
        let (a, o) = admits(branch);
        let open = branch.get("type").is_none();
        // an empty container may become either kind: every container branch is a candidate
        if !(open || empty && (a || o) || if is_array { a } else { o }) {
            continue;
        }
        if !budget.spend(size(value)) {
            return None;
        }
        let mut trial = value.clone();
        let mut trial_changed = false;
        shape(&mut trial, branch, budget, &mut trial_changed);
        if valid(&trial, branch) {
            if found.is_some() {
                return None;
            }
            found = Some(trial);
        }
    }
    found
}
