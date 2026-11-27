use super::budget::STEP_KEYWORD_CALLS;
use super::*;
use serde_json::json;

#[test]
fn nested_constraints_and_json_identity() {
    let schema = Schema::compile(json!({
        "type":"object", "required":["value","list"], "additionalProperties":false,
        "properties":{
            "value":{"oneOf":[{"type":"null"},{"type":"integer", "enum":[1,2]}]},
            "list":{"type":"array", "items":{"type":"object", "required":["x"],
                "properties":{"x":{"const":{ "a": [] }}}}}
        }
    }))
    .unwrap();
    assert!(schema.validate(&json!({"value":null,"list":[{"x":{"a":[]}}]})).is_ok());
    assert!(schema.validate(&json!({"value":1.0,"list":[]})).is_ok());
    for bad in [
        json!({"value":1.5,"list":[]}),
        json!({"value":null,"list":[{"x":{"a":{}}}]}),
        json!({"value":null,"list":[],"extra":0}),
        json!({"list":[]}),
    ] {
        assert!(schema.validate(&bad).is_err(), "{bad}");
    }
    let overlapping = Schema::compile(json!({"type":"object","oneOf":[{},{}]})).unwrap();
    assert!(overlapping.validate(&json!({})).is_err());
}

#[test]
fn explicit_history_budget_keeps_the_default_limit_and_still_validates_items() {
    let schema = Schema::compile_contract(json!({
        "type":"object", "properties":{"items":{"type":"array", "items":{"type":"integer"}}},
        "required":["items"], "additionalProperties":false
    }))
    .unwrap();
    let mut value = json!({"items": vec![0; 12_000]});
    assert!(schema.validate(&value).is_err(), "the default limit must remain unchanged");
    assert!(schema.validate_with_node_limit(&value, 20_000).is_ok());
    value["items"][11_999] = json!("not an integer");
    assert!(schema.validate_with_node_limit(&value, 20_000).is_err(), "all history items must be checked");
    assert!(schema.validate_with_node_limit(&json!({"items":vec![0; 20_000]}), 20_000).is_err());
}

#[test]
fn rejects_unknown_and_malformed_keywords_with_paths() {
    for (schema, path) in [
        (json!({"type":"object","properties":{"x":{"minimum":0}}}), "$/properties/x/minimum"),
        (json!({"type":"object","items":[]}), "$/items"),
        (json!({"type":"object","required":["x","x"]}), "$/required"),
        (json!({"type":"object","type_bad":"string"}), "$/type_bad"),
        (json!({"type":"object","oneOf":[]}), "$/oneOf"),
        (json!({"type":"array"}), "$"),
    ] {
        let err = Schema::compile(schema).unwrap_err();
        assert!(err.contains(path), "{err}");
    }
}

#[test]
fn json_numbers_compare_as_decimals_without_losing_large_integer_precision() {
    assert!(json_equal(&json!(1), &json!(1.0)));
    assert!(!json_equal(&json!(9_007_199_254_740_993u64), &json!(9_007_199_254_740_992u64)));
    assert!(Schema::compile(json!({"type":"object","enum":[{"v":1},{"v":1.0}]})).is_err());
}

#[test]
fn decimal_parts_handles_extreme_exponents_without_overflow_or_panic() {
    assert_eq!(equal::decimal_parts("1.0e-9223372036854775808"), Ok((false, "1".into(), -9_223_372_036_854_775_808)));
    assert_eq!(
        equal::decimal_parts("1.23e-9223372036854775808"),
        Ok((false, "123".into(), -9_223_372_036_854_775_810))
    );
    assert_eq!(equal::decimal_parts("1.0e9223372036854775807"), Ok((false, "1".into(), 9_223_372_036_854_775_807)));
    assert_eq!(equal::decimal_parts("1e-9223372036854775808"), equal::decimal_parts("1.0e-9223372036854775808"));

    assert!(equal::decimal_parts("1.23e-170141183460469231731687303715884105728").is_err());
    assert!(equal::decimal_parts("1e9999999999999999999999999999999999999999").is_err());
    assert!(equal::decimal_parts("0e9999999999999999999999999999999999999999").is_err());
}

#[test]
fn rejects_values_beyond_resource_limits() {
    let mut nested = json!(0);
    for _ in 0..65 {
        nested = json!([nested]);
    }
    assert!(Schema::compile(json!({"type":"object","default":nested})).is_err());
    let schema = Schema::compile(json!({"type":"object"})).unwrap();
    assert!(schema.validate(&json!({"values":vec![0; MAX_NODES]})).is_err());
    let out_schema = OutputSchema::compile(json!({"type":"object"})).unwrap();
    assert!(out_schema.validate(&json!({"values":vec![0; MAX_NODES]})).is_err());
}

#[test]
fn output_schema_supports_refs_combinators_and_text_validation() {
    let schema = OutputSchema::compile(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "item": {
                "type": "object",
                "required": ["name", "score"],
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "minLength": 2 },
                    "score": { "type": "integer", "minimum": 0, "maximum": 100 }
                }
            }
        },
        "type": "object",
        "required": ["status", "items"],
        "additionalProperties": false,
        "properties": {
            "status": { "anyOf": [{ "const": "ok" }, { "const": "warn" }] },
            "items": { "type": "array", "minItems": 1, "items": { "$ref": "#/$defs/item" } }
        }
    }))
    .unwrap();
    let parsed = schema.validate_json_text(r#"{"status":"ok","items":[{"name":"ab","score":42}]}"#).unwrap();
    assert_eq!(parsed["items"][0]["score"], 42);
    assert!(schema.validate_json_text("not json").is_err());
    assert!(schema.validate_json_text(r#"{"status":"bad","items":[{"name":"ab","score":42}]}"#).is_err());
    assert!(schema.validate_json_text(r#"{"status":"ok","items":[{"name":"a","score":42}]}"#).is_err());
    assert!(OutputSchema::compile(json!({"$ref": "#/$defs/missing"})).is_err());
    assert!(OutputSchema::compile(json!("not-an-object")).is_err());
}

#[test]
fn output_schema_supports_recursive_refs_and_rejects_external_refs() {
    let recursive = OutputSchema::compile(json!({
        "$defs": {
            "node": {
                "type": "object",
                "required": ["value"],
                "additionalProperties": false,
                "properties": {
                    "value": { "type": "integer" },
                    "next": { "$ref": "#/$defs/node" }
                }
            }
        },
        "$ref": "#/$defs/node"
    }))
    .unwrap();
    assert!(recursive.validate(&json!({"value": 1, "next": {"value": 2}})).is_ok());
    assert!(recursive.validate(&json!({"value": 1, "next": {"value": "bad"}})).is_err());

    // Self-referential / cyclic $ref must not hang or stack-overflow at compile or validate time.
    let cyclic = OutputSchema::compile(json!({ "$ref": "#" })).unwrap_err();
    assert!(cyclic.contains("resource limit exceeded"), "{cyclic}");

    // External $refs must be rejected offline.
    assert!(OutputSchema::compile(json!({"$ref": "https://example.com/schema.json"})).is_err());
    assert!(OutputSchema::compile(json!({"$ref": "file:///etc/passwd"})).is_err());
}

#[test]
fn output_schema_bounds_ref_and_combinator_expansion() {
    // Mutual same-instance $ref cycle must be rejected with a budget error.
    let mutual_cycle = OutputSchema::compile(json!({
        "$defs": {
            "a": { "$ref": "#/$defs/b" },
            "b": { "$ref": "#/$defs/a" }
        },
        "$ref": "#/$defs/a"
    }))
    .unwrap_err();
    assert!(mutual_cycle.contains("resource limit exceeded"), "{mutual_cycle}");

    // Exponential $ref + anyOf/oneOf/allOf DAG (2^16 > MAX_NODES) must hit the budget limit cleanly.
    let mut defs = serde_json::Map::new();
    for i in 0..16 {
        let next =
            if i + 1 == 16 { json!({ "type": "integer" }) } else { json!({ "$ref": format!("#/$defs/s{}", i + 1) }) };
        defs.insert(format!("s{i}"), json!({ "anyOf": [next.clone(), next] }));
    }
    let exp_dag = OutputSchema::compile(json!({
        "$defs": defs,
        "$ref": "#/$defs/s0"
    }))
    .unwrap_err();
    assert!(exp_dag.contains("resource limit exceeded"), "{exp_dag}");

    // Recursive $ref with combinator branching > 1 on the same instance must hit the budget limit cleanly.
    let branching_rec = OutputSchema::compile(json!({
        "$defs": {
            "node": {
                "anyOf": [
                    { "properties": { "next": { "$ref": "#/$defs/node" } } },
                    { "properties": { "next": { "$ref": "#/$defs/node" } } }
                ]
            }
        },
        "$ref": "#/$defs/node"
    }))
    .unwrap_err();
    assert!(branching_rec.contains("resource limit exceeded"), "{branching_rec}");

    // Valid recursive $ref with non-branching combinator and binary tree must still succeed.
    let list = OutputSchema::compile(json!({
        "$defs": {
            "node": {
                "anyOf": [
                    { "type": "null" },
                    {
                        "type": "object",
                        "required": ["val"],
                        "properties": {
                            "val": { "type": "integer" },
                            "next": { "$ref": "#/$defs/node" }
                        }
                    }
                ]
            }
        },
        "$ref": "#/$defs/node"
    }))
    .unwrap();
    assert!(list.validate(&json!({"val": 1, "next": {"val": 2, "next": null}})).is_ok());

    let tree = OutputSchema::compile(json!({
        "$defs": {
            "tree": {
                "type": "object",
                "properties": {
                    "left": { "$ref": "#/$defs/tree" },
                    "right": { "$ref": "#/$defs/tree" }
                }
            }
        },
        "$ref": "#/$defs/tree"
    }))
    .unwrap();
    assert!(tree.validate(&json!({"left": {}, "right": {"left": {}}})).is_ok());
}

#[test]
fn output_schema_supports_ecmascript_patterns_and_bounds_backtracking() {
    // Lookahead is ECMAScript-only (unsupported by standard Rust `regex`).
    let ecmascript = OutputSchema::compile(json!({
        "type": "string",
        "pattern": "^(?=.*[0-9])(?=.*[a-z])[a-z0-9]+$"
    }))
    .unwrap();
    assert!(ecmascript.validate(&json!("abc123")).is_ok());
    assert!(ecmascript.validate(&json!("abcdef")).is_err());
    assert!(ecmascript.validate(&json!("123456")).is_err());

    // Catastrophic backtracking must be bounded by backtrack_limit instead of hanging.
    let redos = OutputSchema::compile(json!({
        "type": "string",
        "pattern": "^(a+)+$"
    }))
    .unwrap();
    let evil = format!("{}!", "a".repeat(30));
    assert!(redos.validate(&json!(evil)).is_err());
}

#[test]
fn validation_bounds_quadratic_schema_times_instance_steps() {
    // 100-branch oneOf schema (~200 schema nodes, well under MAX_NODES=10_000)
    // validated against a 1,000-item array (~1,001 instance nodes, depth 2).
    // Both pass individual node/depth caps, yet 100 * 1,000 > MAX_VALIDATION_STEPS.
    let branches: Vec<Value> = (0..100).map(|i| json!({ "const": i })).collect();
    let output_schema = OutputSchema::compile(json!({
        "$defs": {
            "item": {
                "oneOf": branches
            }
        },
        "type": "array",
        "items": { "$ref": "#/$defs/item" }
    }))
    .unwrap();

    let small_valid = Value::Array((0..10).map(|i| json!(i)).collect());
    assert!(output_schema.validate(&small_valid).is_ok());

    let large_instance = Value::Array((0..1_000).map(|i| json!(i % 100)).collect());
    let err = output_schema.validate(&large_instance).unwrap_err();
    assert!(err.contains("resource limit exceeded"), "{err}");

    // `not` must not invert a budget-exhausted failure into a pass.
    let negated = OutputSchema::compile(json!({
        "not": {
            "type": "array",
            "items": { "oneOf": (0..100).map(|i| json!({ "type": "integer", "const": i })).collect::<Vec<_>>() }
        }
    }))
    .unwrap();
    let neg_err = negated.validate(&large_instance).unwrap_err();
    assert!(neg_err.contains("resource limit exceeded"), "{neg_err}");

    // Tool `Schema` must also enforce the step budget.
    let tool_schema = Schema::compile(json!({
        "type": "object",
        "properties": {
            "list": {
                "type": "array",
                "items": {
                    "oneOf": (0..100).map(|i| json!({ "const": i })).collect::<Vec<_>>()
                }
            }
        }
    }))
    .unwrap();
    assert!(tool_schema.validate(&json!({ "list": [0, 1, 2] })).is_ok());
    let tool_err = tool_schema.validate(&json!({ "list": large_instance })).unwrap_err();
    assert!(tool_err.contains("resource limit exceeded"), "{tool_err}");
}

#[test]
fn validation_truncates_adversarial_any_of_without_collecting_all_branch_errors() {
    // 20 outer anyOf branches, each wrapping items -> 100-branch anyOf (~2,000 expanded nodes).
    // Validating a 1,000-item array has 2,000,000 potential branch/item evaluations.
    let inner_branches: Vec<Value> = (0..100).map(|i| json!({ "const": i })).collect();
    let outer_branches: Vec<Value> =
        (0..20).map(|_| json!({ "type": "array", "items": { "$ref": "#/$defs/item" } })).collect();
    let schema = OutputSchema::compile(json!({
        "$defs": {
            "item": { "anyOf": inner_branches }
        },
        "anyOf": outer_branches
    }))
    .unwrap();

    // Case 1: Budget exhausted mid-validation (items 0..500 match last inner branch, 501 fails):
    // must abort without invoking `collect_errors` across all 20 outer branches x 1,000 items.
    let mut late_fail: Vec<Value> = (0..1_000).map(|_| json!(99)).collect();
    late_fail[501] = json!("bad");
    STEP_KEYWORD_CALLS.with(|c| c.set(0));
    let err = schema.validate(&Value::Array(late_fail)).unwrap_err();
    let calls = STEP_KEYWORD_CALLS.with(|c| c.get());
    assert!(err.contains("resource limit exceeded"), "{err}");
    assert!(calls < MAX_VALIDATION_STEPS, "expected early truncation, got {calls} keyword calls");

    // Case 2: Fails at item 0 (is_valid returns false under budget), yet schema * instance >
    // MAX_VALIDATION_STEPS so error collection must not traverse all 1,000 items x 20 branches.
    let mut early_fail: Vec<Value> = (0..1_000).map(|_| json!(99)).collect();
    early_fail[0] = json!("bad");
    STEP_KEYWORD_CALLS.with(|c| c.set(0));
    let err = schema.validate(&Value::Array(early_fail)).unwrap_err();
    let calls = STEP_KEYWORD_CALLS.with(|c| c.get());
    assert!(err.contains("does not match schema"), "{err}");
    assert!(calls < 5_000, "expected collect_errors to be skipped, got {calls} keyword calls");
}

#[test]
fn numeric_bounds_compare_large_integers_and_fractional_boundaries_exactly() {
    let min_schema = OutputSchema::compile(json!({
        "type": "number",
        "minimum": 9_007_199_254_740_993_u64
    }))
    .unwrap();
    assert!(min_schema.validate(&json!(9_007_199_254_740_993_u64)).is_ok());
    assert!(min_schema.validate(&json!(9_007_199_254_740_992_u64)).is_err());
    assert!(min_schema.validate_json_text("9007199254740992.5").is_err());

    let max_schema = OutputSchema::compile(json!({
        "type": "number",
        "maximum": 9_007_199_254_740_992_u64
    }))
    .unwrap();
    assert!(max_schema.validate(&json!(9_007_199_254_740_992_u64)).is_ok());
    assert!(max_schema.validate(&json!(9_007_199_254_740_993_u64)).is_err());

    let ex_bounds = OutputSchema::compile(json!({
        "type": "integer",
        "exclusiveMinimum": 9_007_199_254_740_992_u64,
        "exclusiveMaximum": 9_007_199_254_740_994_u64
    }))
    .unwrap();
    assert!(ex_bounds.validate(&json!(9_007_199_254_740_993_u64)).is_ok());
    assert!(ex_bounds.validate(&json!(9_007_199_254_740_992_u64)).is_err());
    assert!(ex_bounds.validate(&json!(9_007_199_254_740_994_u64)).is_err());

    // Draft-04 boolean exclusiveMinimum/exclusiveMaximum.
    let draft4 = OutputSchema::compile(json!({
        "$schema": "http://json-schema.org/draft-04/schema#",
        "type": "integer",
        "minimum": 9_007_199_254_740_992_u64,
        "exclusiveMinimum": true
    }))
    .unwrap();
    assert!(draft4.validate(&json!(9_007_199_254_740_992_u64)).is_err());
    assert!(draft4.validate(&json!(9_007_199_254_740_993_u64)).is_ok());

    // u64::MAX and i64::MIN boundaries (including f64-rounded boundaries).
    let u64_max_schema = OutputSchema::compile(json!({
        "type": "integer",
        "minimum": u64::MAX,
        "maximum": u64::MAX
    }))
    .unwrap();
    assert!(u64_max_schema.validate(&json!(u64::MAX)).is_ok());
    assert!(u64_max_schema.validate(&json!(u64::MAX - 1)).is_err());
    assert!(u64_max_schema.validate(&json!(u64::MAX as f64)).is_err());

    let i64_min_schema = OutputSchema::compile(json!({
        "type": "integer",
        "minimum": i64::MIN,
        "maximum": i64::MIN + 1
    }))
    .unwrap();
    assert!(i64_min_schema.validate(&json!(i64::MIN)).is_ok());
    assert!(i64_min_schema.validate(&json!(i64::MIN + 1)).is_ok());
    assert!(i64_min_schema.validate(&json!(i64::MIN + 2)).is_err());

    // Fractional boundaries (both positive and negative).
    let frac_schema = OutputSchema::compile(json!({
        "type": "number",
        "minimum": -1.5,
        "maximum": 4_503_599_627_370_495.5_f64
    }))
    .unwrap();
    assert!(frac_schema.validate(&json!(-1)).is_ok());
    assert!(frac_schema.validate(&json!(-2)).is_err());
    assert!(frac_schema.validate(&json!(4_503_599_627_370_495_u64)).is_ok());
    assert!(frac_schema.validate(&json!(4_503_599_627_370_496_u64)).is_err());

    // multipleOf with large integers and decimals.
    let mult_two = OutputSchema::compile(json!({
        "type": "integer",
        "multipleOf": 2
    }))
    .unwrap();
    assert!(mult_two.validate(&json!(9_007_199_254_740_992_u64)).is_ok());
    assert!(mult_two.validate(&json!(9_007_199_254_740_993_u64)).is_err());

    let mult_dec = OutputSchema::compile(json!({
        "type": "number",
        "multipleOf": 0.1
    }))
    .unwrap();
    assert!(mult_dec.validate(&json!(0.3)).is_ok());
    assert!(mult_dec.validate(&json!(0.35)).is_err());
    assert!(mult_dec.validate(&json!(0.30000000001)).is_err());

    let mult_one = OutputSchema::compile(json!({
        "type": "number",
        "multipleOf": 1
    }))
    .unwrap();
    assert!(mult_one.validate(&json!(1e-10)).is_err());
    assert!(mult_one.validate(&json!(1.0000000001)).is_err());

    let mult_cent = OutputSchema::compile(json!({
        "type": "number",
        "multipleOf": 0.01
    }))
    .unwrap();
    assert!(mult_cent.validate(&json!(0.03)).is_ok());
    assert!(mult_cent.validate(&json!(0.03000000001)).is_err());

    // Decimal precision beyond f64: 1.00000000000000001 must not round to 1.0.
    let parsed_beyond_f64: Value = serde_json::from_str("1.00000000000000001").unwrap();
    assert!(mult_one.validate(&parsed_beyond_f64).is_err());
    let parsed_exact_cent: Value = serde_json::from_str("0.10000000000000001").unwrap();
    assert!(mult_cent.validate(&parsed_exact_cent).is_err());

    // Large divisors beyond u128::MAX / 2: 2e38 + 3 must not be accepted as a multiple of 2e38 + 4.
    let large_divisor_schema: Value =
        serde_json::from_str(r#"{"type":"number","multipleOf":200000000000000000000000000000000000004}"#).unwrap();
    let mult_large = OutputSchema::compile(large_divisor_schema).unwrap();
    let large_invalid: Value = serde_json::from_str("200000000000000000000000000000000000003").unwrap();
    assert!(mult_large.validate(&large_invalid).is_err());
    let large_exact_one: Value = serde_json::from_str("200000000000000000000000000000000000004").unwrap();
    assert!(mult_large.validate(&large_exact_one).is_ok());
    let large_exact_two: Value = serde_json::from_str("400000000000000000000000000000000000008").unwrap();
    assert!(mult_large.validate(&large_exact_two).is_ok());

    // Extreme exponents beyond f64 range.
    let extreme_large_schema: Value = serde_json::from_str(r#"{"type":"number","multipleOf":1e308}"#).unwrap();
    let mult_extreme_large = OutputSchema::compile(extreme_large_schema).unwrap();
    let extreme_large_ok: Value = serde_json::from_str("1e309").unwrap();
    assert!(mult_extreme_large.validate(&extreme_large_ok).is_ok());
    let extreme_large_err: Value = serde_json::from_str("1.5e308").unwrap();
    assert!(mult_extreme_large.validate(&extreme_large_err).is_err());

    let extreme_small_schema: Value = serde_json::from_str(r#"{"type":"number","multipleOf":1e-400}"#).unwrap();
    let mult_extreme_small = OutputSchema::compile(extreme_small_schema).unwrap();
    let extreme_small_ok: Value = serde_json::from_str("2e-400").unwrap();
    assert!(mult_extreme_small.validate(&extreme_small_ok).is_ok());
    let extreme_small_err: Value = serde_json::from_str("1.5e-400").unwrap();
    assert!(mult_extreme_small.validate(&extreme_small_err).is_err());
}

#[cfg(test)]
mod value_tests {
    #[test]
    fn scalar_output_schema_does_not_weaken_argument_schemas() {
        let schema = super::Schema::compile_value(serde_json::json!({"type":"boolean"})).unwrap();
        assert!(schema.validate(&serde_json::json!(false)).is_ok());
        assert!(schema.validate(&serde_json::json!("false")).is_err());
        assert!(super::Schema::compile(serde_json::json!({"type":"boolean"})).is_err());

        let raw = serde_json::json!({"type": "string", "minLength": 1});
        let out = super::OutputSchema::compile(raw.clone()).unwrap();
        assert_eq!(out.value(), &raw);
    }
}

/// An empty container takes the shape its schema position declares; open positions and
/// containers with content keep theirs.
#[test]
fn empty_containers_take_the_declared_shape() {
    use serde_json::json;
    let array = json!({ "type": "array" });
    let object = json!({ "type": "object" });
    let tagged = json!({ "oneOf": [
        { "type": "object", "properties": { "kind": { "const": "a" }, "x": { "type": "array" } },
          "required": ["kind", "x"], "additionalProperties": false },
        { "type": "object", "properties": { "kind": { "const": "b" }, "x": { "type": "object" } },
          "required": ["kind", "x"], "additionalProperties": false },
    ] });
    let cases = [
        (json!({}), array.clone(), json!([])),
        (json!([]), object.clone(), json!({})),
        (json!({}), json!({ "type": ["array", "null"] }), json!([])),
        // contracts accept oneOf only (the schema subset has no anyOf)
        (json!({}), json!({ "oneOf": [{ "type": "array" }, { "type": "string" }] }), json!([])),
        (json!({}), json!({ "type": ["array", "object"] }), json!({})),
        (json!({}), json!({}), json!({})),
        (json!([]), json!({}), json!([])),
        (json!({ "a": 1 }), array.clone(), json!({ "a": 1 })),
        (
            json!({ "data": {}, "meta": [], "rows": [{ "tags": {} }] }),
            json!({ "type": "object", "properties": {
                "data": array,
                "meta": { "type": "object", "additionalProperties": { "type": "string" } },
                "rows": { "type": "array", "items": { "type": "object", "properties": { "tags": { "type": "array" } } } },
            } }),
            json!({ "data": [], "meta": {}, "rows": [{ "tags": [] }] }),
        ),
        (
            json!({ "x": {} }),
            json!({ "type": "object", "additionalProperties": { "type": "array" } }),
            json!({ "x": [] }),
        ),
        // oneOf object branches told apart by a const tag: the value's own fields pick the branch
        (json!({ "kind": "b", "x": {} }), tagged.clone(), json!({ "kind": "b", "x": {} })),
        (json!({ "kind": "a", "x": {} }), tagged.clone(), json!({ "kind": "a", "x": [] })),
        // the parent's own properties and the branch's both apply
        (
            json!({ "kind": "b", "x": {}, "tags": {} }),
            json!({ "type": "object", "properties": { "tags": { "type": "array" } }, "oneOf": [
                { "type": "object", "properties": { "kind": { "const": "a" }, "x": { "type": "array" } }, "required": ["kind"] },
                { "type": "object", "properties": { "kind": { "const": "b" }, "x": { "type": "object" } }, "required": ["kind"] },
            ] }),
            json!({ "kind": "b", "x": {}, "tags": [] }),
        ),
        // a nested open branch leaves an empty table as it is
        (
            json!({ "v": {} }),
            json!({ "type": "object", "properties": { "v": { "oneOf": [{}, { "type": "array" }] } } }),
            json!({ "v": {} }),
        ),
        // no branch satisfied: the value keeps its shape (validation reports it)
        (json!({ "kind": "c", "x": {} }), tagged.clone(), json!({ "kind": "c", "x": {} })),
    ];
    for (mut value, schema, want) in cases {
        let given = value.clone();
        super::shape_empty(&mut value, &schema);
        assert_eq!(value, want, "{given} under {schema}");
    }
}

/// Shaping has a step budget: a wide oneOf over many empty values stops early (what is not
/// reached keeps its encoding) instead of trying every branch for every value.
#[test]
fn shaping_stops_when_its_step_budget_is_spent() {
    use serde_json::json;
    let branches: Vec<_> = (0..1000)
        .map(|i| json!({ "type": "object", "properties": { "k": { "const": i }, "x": { "type": "array" } }, "required": ["k"] }))
        .collect();
    let schema = json!({ "type": "array", "items": { "oneOf": branches } });
    let mut value = serde_json::Value::Array((0..30_000).map(|_| json!({ "x": {} })).collect());
    assert!(!super::shape_empty(&mut value, &schema), "the budget ran out");
    let mut small = json!([{ "k": 3, "x": {} }]);
    assert!(super::shape_empty(&mut small, &schema));
    assert_eq!(small, json!([{ "k": 3, "x": [] }]));
}

/// Ambiguity keeps the shape (an overlapping oneOf matches two branches): two array branches; a
/// value already valid; two object branches whose `x` schemas both accept the original.
#[test]
fn ambiguous_or_already_valid_values_keep_their_shape() {
    use serde_json::json;
    let cases = [
        (json!({}), json!({ "oneOf": [{ "type": "array" }, { "type": "array", "items": { "type": "string" } }] })),
        (
            json!({ "v": {} }),
            json!({ "type": "object", "properties": {
            "v": { "oneOf": [{ "oneOf": [{}] }, { "type": "array", "const": [1] }] } } }),
        ),
        (
            json!({ "x": {} }),
            json!({ "oneOf": [
            { "type": "object", "properties": { "x": { "oneOf": [{ "oneOf": [{}] }, { "type": "array", "const": [1] }] } } },
            { "type": "object", "properties": { "x": { "oneOf": [{ "oneOf": [{}] }, { "type": "array" }] } } },
        ] }),
        ),
    ];
    for (value, schema) in cases {
        let mut shaped = value.clone();
        super::shape_empty(&mut shaped, &schema);
        assert_eq!(shaped, value, "{value} under {schema}");
    }
}

#[test]
fn contract_schema_supports_scalar_bounds_while_tool_schema_rejects_them() {
    let raw = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "cols": { "type": "integer", "minimum": 1, "maximum": 500 },
            "name": { "type": "string", "minLength": 1, "maxLength": 128 }
        }
    });
    assert!(Schema::compile(raw.clone()).is_err());
    let contract = Schema::compile_contract(raw).unwrap();
    assert!(contract.validate(&json!({ "cols": 80, "name": "dev" })).is_ok());
    assert!(contract.validate(&json!({ "cols": 0 })).is_err());
    assert!(contract.validate(&json!({ "cols": 501 })).is_err());
    assert!(contract.validate(&json!({ "name": "" })).is_err());
    assert!(contract.validate(&json!({ "name": "a".repeat(129) })).is_err());
    assert!(
        Schema::compile_contract(json!({
            "type": "object",
            "properties": { "bad": { "type": "string", "minLength": -1 } }
        }))
        .is_err()
    );
}

#[test]
fn compiled_schema_defers_validator_build_until_first_validate_and_shares_across_clones() {
    let schema = Schema::compile_contract(json!({
        "type": "object",
        "required": ["name"],
        "additionalProperties": false,
        "properties": {
            "name": { "type": "string", "minLength": 1 }
        }
    }))
    .unwrap();
    assert!(!schema.is_validator_built(), "compile_contract should defer building the jsonschema Validator");
    let cloned = schema.clone();
    assert!(cloned.validate(&json!({ "name": "ok" })).is_ok());
    assert!(cloned.validate(&json!({ "name": "" })).is_err());
    assert!(cloned.is_validator_built(), "first validate should build the validator");
    assert!(schema.is_validator_built(), "clones should share the lazily built validator");
}

#[test]
fn validate_contract_result_schema_matches_wrapped_compile_contract() {
    let wrap = |inner: &serde_json::Value| {
        json!({
            "type": "object",
            "properties": { "value": inner },
            "required": ["value"],
            "additionalProperties": false
        })
    };
    let cases = [
        json!({ "type": "boolean" }),
        json!({ "type": "string", "minLength": 1, "maxLength": 64 }),
        json!({
            "type": "object",
            "properties": { "a/b~c": { "type": ["string", "null"] } },
            "required": ["a/b~c"],
            "additionalProperties": false
        }),
        json!({ "type": "object", "properties": { "bad": { "unknownKeyword": true } } }),
        json!({ "type": ["string", "string"] }),
        json!({ "type": "object", "required": ["x", "x"] }),
        json!({ "type": "string", "minLength": -1 }),
    ];
    for case in cases {
        let wrapped = Schema::compile_contract(wrap(&case)).map(|_| ());
        let direct = Schema::validate_contract_result_schema(&case);
        assert_eq!(direct, wrapped, "parity mismatch for {case}");
    }
}

#[test]
fn shape_and_validate_reuses_cached_validator_and_tracks_reshaping() {
    let params_schema = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "meta": { "type": "object", "additionalProperties": { "type": "string" } }
        },
        "required": ["name"],
        "additionalProperties": false
    });
    let compiled = Schema::compile_contract(params_schema).unwrap();

    // Already valid with no empty containers -> reshaped = false.
    let mut plain = json!({ "name": "ok", "tags": ["a"] });
    assert_eq!(compiled.shape_and_validate(&mut plain), Ok(false));

    // Empty object where schema requires array -> reshaped = true and value becomes [].
    let mut needs_shape = json!({ "name": "ok", "tags": {} });
    assert_eq!(compiled.shape_and_validate(&mut needs_shape), Ok(true));
    assert_eq!(needs_shape, json!({ "name": "ok", "tags": [] }));

    // Empty array already marked as [] -> validates on first pass, reshaped = false.
    let mut already_shaped = json!({ "name": "ok", "tags": [], "meta": {} });
    assert_eq!(compiled.shape_and_validate(&mut already_shaped), Ok(false));

    // Invalid value without empty containers -> fails cleanly.
    let mut invalid = json!({ "name": 42 });
    assert!(compiled.shape_and_validate(&mut invalid).is_err());

    // Wrapped result variant mirrors validate_service_result without cloning.
    let result_schema = json!({
        "type": "object",
        "properties": {
            "bindings": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["bindings"],
        "additionalProperties": false
    });
    let wrapped_schema = Schema::compile_contract(json!({
        "type": "object",
        "properties": { "value": result_schema.clone() },
        "required": ["value"],
        "additionalProperties": false
    }))
    .unwrap();
    let mut res_ok = json!({ "bindings": [] });
    assert_eq!(wrapped_schema.shape_and_validate_wrapped_result(&mut res_ok, &result_schema, 10_000), Ok(false));
    let mut res_shape = json!({ "bindings": {} });
    assert_eq!(wrapped_schema.shape_and_validate_wrapped_result(&mut res_shape, &result_schema, 10_000), Ok(true));
    assert_eq!(res_shape, json!({ "bindings": [] }));
    let mut res_bad = json!({ "bindings": "nope" });
    assert!(wrapped_schema.shape_and_validate_wrapped_result(&mut res_bad, &result_schema, 10_000).is_err());
}
