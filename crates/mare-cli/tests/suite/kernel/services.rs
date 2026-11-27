use super::{kernel, lua, src};

// The old "follow a service by name" tests went with that ctx method (its native form is
// ctx:inject({ name }, fn)). Stack-only cases (moves to a shadowing provider and back, returns to
// the original provider, rebinds without restarting, a rejected replacement before watchers) have
// no single-slot counterpart. Following the provider is covered by
// ports.rs a_replaced_provider_disposes_its_dependents_first_and_restarts_them_on_the_new_one;
// rapid replacements, in-flight cleanup and a failed restart under churn are to be ported as
// inject-shaped dependent tests (T3b).

#[test]
fn service_contract_declarations_are_strict() {
    let k = kernel();
    for code in [
        r#"return { name = "bad", contracts = { x = { required = "run" } } }"#,
        r#"return { name = "bad", contracts = { x = { required = { [2] = "run" } } } }"#,
        r#"return { name = "bad", contracts = { x = { required = { "run" }, optional = { "run" } } } }"#,
        r#"return { name = "bad", contracts = { x = { requred = { "run" } } } }"#,
    ] {
        let err = k.load_def(&src("bad", code)).err();
        assert!(err.is_some(), "must reject invalid contract: {code}");
    }
}

#[test]
fn builtin_llm_contract_covers_session_resume_fallbacks() {
    let lua = mlua::Lua::new();
    let contracts = lua.load(lua::service_contracts_lua().as_ref()).eval::<mlua::Table>().unwrap();
    let llm: mlua::Table = contracts.get("llm").unwrap();
    let methods: mlua::Table = llm.get("methods").unwrap();
    for method in ["get_selection", "restore_selection", "reset_session_model", "reset_session_effort"] {
        let spec: Option<mlua::Table> = methods.get(method).unwrap();
        let spec = spec.unwrap_or_else(|| panic!("llm service contract must require {method}"));
        assert!(!spec.get::<Option<bool>>("optional").unwrap().unwrap_or(false), "{method} must be required");
    }
}
