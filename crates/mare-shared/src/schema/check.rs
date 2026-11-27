use super::{MAX_DEPTH, MAX_ENUM_VALUES, try_json_equal};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Borrowed linked JSON-pointer segment list so validation never allocates path strings on the
/// success path; full paths are materialized only when formatting an error message.
#[derive(Clone, Copy)]
pub(super) enum PathSeg<'a> {
    Root(&'a str),
    Key(&'a Self, &'a str),
    EscapedKey(&'a Self, &'a str),
    Index(&'a Self, usize),
}

impl PathSeg<'_> {
    fn render(&self) -> String {
        fn write_seg(seg: &PathSeg<'_>, out: &mut String) {
            match seg {
                PathSeg::Root(r) => out.push_str(r),
                PathSeg::Key(parent, key) => {
                    write_seg(parent, out);
                    out.push('/');
                    out.push_str(key);
                }
                PathSeg::EscapedKey(parent, key) => {
                    write_seg(parent, out);
                    out.push('/');
                    for ch in key.chars() {
                        match ch {
                            '~' => out.push_str("~0"),
                            '/' => out.push_str("~1"),
                            c => out.push(c),
                        }
                    }
                }
                PathSeg::Index(parent, idx) => {
                    write_seg(parent, out);
                    let _ = write!(out, "/{idx}");
                }
            }
        }
        let mut out = String::new();
        write_seg(self, &mut out);
        out
    }
}

pub(super) fn check_value(v: &Value, path: &str, depth: usize, budget: &mut usize) -> Result<(), String> {
    let root = PathSeg::Root(path);
    check_value_seg(v, &root, depth, budget)
}

fn check_value_seg(v: &Value, path: &PathSeg<'_>, depth: usize, budget: &mut usize) -> Result<(), String> {
    consume_seg(path, depth, budget)?;
    match v {
        Value::Object(m) => {
            for (key, value) in m {
                let child = PathSeg::Key(path, key);
                check_value_seg(value, &child, depth + 1, budget)?;
            }
        }
        Value::Array(a) => {
            for (index, value) in a.iter().enumerate() {
                let child = PathSeg::Index(path, index);
                check_value_seg(value, &child, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_schema_list(
    val: &Value,
    path: &PathSeg<'_>,
    depth: usize,
    budget: &mut usize,
    allow_bounds: bool,
) -> Result<(), String> {
    let a = val
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("{}: expected a nonempty array", path.render()))?;
    for (i, s) in a.iter().enumerate() {
        let child = PathSeg::Index(path, i);
        check_schema_with(s, &child, depth + 1, budget, allow_bounds)?;
    }
    Ok(())
}

fn check_schema_map(
    val: &Value,
    path: &PathSeg<'_>,
    depth: usize,
    budget: &mut usize,
    allow_bounds: bool,
) -> Result<(), String> {
    let map = val.as_object().ok_or_else(|| format!("{}: expected an object", path.render()))?;
    for (name, schema) in map {
        let child = PathSeg::EscapedKey(path, name);
        check_schema_with(schema, &child, depth + 1, budget, allow_bounds)?;
    }
    Ok(())
}

pub(super) fn check_schema(v: &Value, path: &str, depth: usize, budget: &mut usize) -> Result<(), String> {
    let root = PathSeg::Root(path);
    check_schema_with(v, &root, depth, budget, false)
}

pub(super) fn check_contract_schema(v: &Value, path: &str, depth: usize, budget: &mut usize) -> Result<(), String> {
    let root = PathSeg::Root(path);
    check_schema_with(v, &root, depth, budget, true)
}

pub(super) fn check_contract_schema_at(
    v: &Value,
    path: &PathSeg<'_>,
    depth: usize,
    budget: &mut usize,
) -> Result<(), String> {
    check_schema_with(v, path, depth, budget, true)
}

pub(super) fn check_value_at(v: &Value, path: &PathSeg<'_>, depth: usize, budget: &mut usize) -> Result<(), String> {
    check_value_seg(v, path, depth, budget)
}

fn is_valid_schema_type(name: &str) -> bool {
    matches!(name, "object" | "array" | "string" | "number" | "integer" | "boolean" | "null")
}

fn check_schema_with(
    v: &Value,
    path: &PathSeg<'_>,
    depth: usize,
    budget: &mut usize,
    allow_bounds: bool,
) -> Result<(), String> {
    consume_seg(path, depth, budget)?;
    let obj = v.as_object().ok_or_else(|| format!("{}: schema must be an object", path.render()))?;
    for (key, val) in obj {
        let p = PathSeg::EscapedKey(path, key);
        match key.as_str() {
            "type" => match val {
                Value::String(name) => {
                    if !is_valid_schema_type(name) {
                        return Err(format!("{}: invalid or duplicate type {name:?}", p.render()));
                    }
                }
                Value::Array(a) if !a.is_empty() => {
                    for (i, t) in a.iter().enumerate() {
                        let name = t.as_str().ok_or_else(|| format!("{}: type must be a string", p.render()))?;
                        if !is_valid_schema_type(name) || a[..i].iter().any(|prev| prev.as_str() == Some(name)) {
                            return Err(format!("{}: invalid or duplicate type {name:?}", p.render()));
                        }
                    }
                }
                _ => return Err(format!("{}: expected a type name or nonempty array of type names", p.render())),
            },
            "properties" => check_schema_map(val, &p, depth, budget, allow_bounds)?,
            "required" => {
                let a = val.as_array().ok_or_else(|| format!("{}: expected an array", p.render()))?;
                if a.len() <= 16 {
                    for (i, v) in a.iter().enumerate() {
                        let s = v.as_str().ok_or_else(|| format!("{}: expected strings", p.render()))?;
                        if a[..i].iter().any(|prev| prev.as_str() == Some(s)) {
                            return Err(format!("{}: duplicate required property {s:?}", p.render()));
                        }
                    }
                } else {
                    let mut seen = HashSet::with_capacity(a.len());
                    for v in a {
                        let s = v.as_str().ok_or_else(|| format!("{}: expected strings", p.render()))?;
                        if !seen.insert(s) {
                            return Err(format!("{}: duplicate required property {s:?}", p.render()));
                        }
                    }
                }
            }
            "additionalProperties" if val.is_boolean() => {}
            "additionalProperties" | "items" => check_schema_with(val, &p, depth + 1, budget, allow_bounds)?,
            "oneOf" => check_schema_list(val, &p, depth, budget, allow_bounds)?,
            "enum" => {
                let a = val
                    .as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| format!("{}: expected a nonempty array", p.render()))?;
                if a.len() > MAX_ENUM_VALUES {
                    return Err(format!("{}: enum exceeds {MAX_ENUM_VALUES} values", p.render()));
                }
                for (i, v) in a.iter().enumerate() {
                    for other in &a[..i] {
                        if try_json_equal(other, v).map_err(|e| format!("{}: {e}", p.render()))? {
                            return Err(format!("{}: duplicate enum value", p.render()));
                        }
                    }
                }
            }
            "minimum" | "maximum" if allow_bounds => {
                if !val.is_number() {
                    return Err(format!("{}: expected a number", p.render()));
                }
            }
            "minLength" | "maxLength" if allow_bounds => {
                if val.as_u64().is_none() {
                    return Err(format!("{}: expected a non-negative integer", p.render()));
                }
            }
            "title" | "description" if val.is_string() => {}
            "title" | "description" => return Err(format!("{}: expected a string", p.render())),
            "examples" if !val.is_array() => return Err(format!("{}: expected an array", p.render())),
            "x-mare-order" => {
                let a = val.as_array().ok_or_else(|| format!("{}: expected an array of strings", p.render()))?;
                if !a.iter().all(Value::is_string) {
                    return Err(format!("{}: expected an array of strings", p.render()));
                }
            }
            "default" | "examples" | "const" => {}
            _ => return Err(format!("{}: unsupported schema keyword", p.render())),
        }
    }
    Ok(())
}

fn consume_seg(path: &PathSeg<'_>, depth: usize, budget: &mut usize) -> Result<(), String> {
    if depth > MAX_DEPTH || *budget == 0 {
        return Err(format!("{}: schema validation resource limit exceeded", path.render()));
    }
    *budget -= 1;
    Ok(())
}

fn consume(path: &str, depth: usize, budget: &mut usize) -> Result<(), String> {
    if depth > MAX_DEPTH || *budget == 0 {
        return Err(format!("{path}: schema validation resource limit exceeded"));
    }
    *budget -= 1;
    Ok(())
}

fn resolve_local_pointer<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    let rest = pointer.strip_prefix('/')?;
    let mut cur = root;
    for raw in rest.split('/') {
        let key = raw.replace("~1", "/").replace("~0", "~");
        cur = match cur {
            Value::Object(map) => map.get(&key)?,
            Value::Array(arr) => key.parse::<usize>().ok().and_then(|i| arr.get(i))?,
            _ => return None,
        };
    }
    Some(cur)
}

fn merge_hits(acc: &mut HashMap<String, usize>, other: HashMap<String, usize>, sum: bool) {
    for (k, v) in other {
        let entry = acc.entry(k).or_insert(0);
        *entry = if sum { (*entry).saturating_add(v) } else { (*entry).max(v) };
    }
}

/// Bounds `$ref` and combinator expansion in `OutputSchema` at compile time:
/// - rejects same-instance `$ref` cycles (which cannot terminate on any finite instance),
/// - rejects exponential `$ref`/combinator DAGs exceeding `MAX_NODES`, and
/// - rejects recursive `$ref` cycles with combinator branching factor > 1 on the same instance.
pub(super) fn check_output_schema(root: &Value, path: &str, budget: &mut usize) -> Result<(), String> {
    let mut active_refs = vec![("#".to_string(), false)];
    check_output_node(root, root, path, 0, budget, &mut active_refs)?;
    Ok(())
}

fn check_output_node(
    root: &Value,
    s: &Value,
    path: &str,
    depth: usize,
    budget: &mut usize,
    active_refs: &mut Vec<(String, bool)>,
) -> Result<HashMap<String, usize>, String> {
    consume(path, depth, budget)?;
    let Some(obj) = s.as_object() else {
        return Ok(HashMap::new());
    };
    let mut hits = HashMap::new();

    if let Some(reference) = obj.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve_local_pointer(root, reference)
    {
        if let Some((_, descended)) = active_refs.iter().rev().find(|(r, _)| r == reference) {
            if !*descended {
                return Err(format!("{path}: schema validation resource limit exceeded"));
            }
            *hits.entry(reference.to_string()).or_insert(0) += 1;
        } else {
            active_refs.push((reference.to_string(), false));
            let mut sub_hits = check_output_node(root, target, path, depth + 1, budget, active_refs)?;
            active_refs.pop();
            if sub_hits.remove(reference).unwrap_or(0) > 1 {
                return Err(format!("{path}: schema validation resource limit exceeded"));
            }
            merge_hits(&mut hits, sub_hits, true);
        }
    }

    for key in ["not", "if", "then", "else", "contentSchema"] {
        if let Some(sub) = obj.get(key) {
            let sub_hits = check_output_node(root, sub, &format!("{path}/{key}"), depth + 1, budget, active_refs)?;
            merge_hits(&mut hits, sub_hits, true);
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            for (i, sub) in arr.iter().enumerate() {
                let sub_hits =
                    check_output_node(root, sub, &format!("{path}/{key}/{i}"), depth + 1, budget, active_refs)?;
                merge_hits(&mut hits, sub_hits, true);
            }
        }
    }
    for key in ["dependentSchemas", "dependencies"] {
        if let Some(map) = obj.get(key).and_then(Value::as_object) {
            for (name, sub) in map {
                if sub.is_object() || sub.is_boolean() {
                    let sub_hits =
                        check_output_node(root, sub, &format!("{path}/{key}/{name}"), depth + 1, budget, active_refs)?;
                    merge_hits(&mut hits, sub_hits, true);
                }
            }
        }
    }

    // Instance-descending keywords advance to a child value, allowing safe structural recursion.
    let saved_descended: Vec<bool> = active_refs.iter().map(|(_, d)| *d).collect();
    for (_, d) in active_refs.iter_mut() {
        *d = true;
    }

    if let Some(map) = obj.get("properties").and_then(Value::as_object) {
        let mut prop_hits = HashMap::new();
        for (name, sub) in map {
            let sub_hits =
                check_output_node(root, sub, &format!("{path}/properties/{name}"), depth + 1, budget, active_refs)?;
            merge_hits(&mut prop_hits, sub_hits, false);
        }
        merge_hits(&mut hits, prop_hits, true);
    }
    if let Some(arr) = obj.get("prefixItems").and_then(Value::as_array) {
        let mut tuple_hits = HashMap::new();
        for (i, sub) in arr.iter().enumerate() {
            let sub_hits =
                check_output_node(root, sub, &format!("{path}/prefixItems/{i}"), depth + 1, budget, active_refs)?;
            merge_hits(&mut tuple_hits, sub_hits, false);
        }
        merge_hits(&mut hits, tuple_hits, true);
    }
    if let Some(items) = obj.get("items") {
        if let Some(arr) = items.as_array() {
            let mut tuple_hits = HashMap::new();
            for (i, sub) in arr.iter().enumerate() {
                let sub_hits =
                    check_output_node(root, sub, &format!("{path}/items/{i}"), depth + 1, budget, active_refs)?;
                merge_hits(&mut tuple_hits, sub_hits, false);
            }
            merge_hits(&mut hits, tuple_hits, true);
        } else {
            let sub_hits = check_output_node(root, items, &format!("{path}/items"), depth + 1, budget, active_refs)?;
            merge_hits(&mut hits, sub_hits, true);
        }
    }
    if let Some(map) = obj.get("patternProperties").and_then(Value::as_object) {
        for (pat, sub) in map {
            let sub_hits = check_output_node(
                root,
                sub,
                &format!("{path}/patternProperties/{pat}"),
                depth + 1,
                budget,
                active_refs,
            )?;
            merge_hits(&mut hits, sub_hits, true);
        }
    }
    for key in [
        "additionalProperties",
        "unevaluatedProperties",
        "additionalItems",
        "unevaluatedItems",
        "contains",
        "propertyNames",
    ] {
        if let Some(sub) = obj.get(key) {
            let sub_hits = check_output_node(root, sub, &format!("{path}/{key}"), depth + 1, budget, active_refs)?;
            merge_hits(&mut hits, sub_hits, true);
        }
    }

    for ((_, d), saved) in active_refs.iter_mut().zip(saved_descended) {
        *d = saved;
    }

    Ok(hits)
}
