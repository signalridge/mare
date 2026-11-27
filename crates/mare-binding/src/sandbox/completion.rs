//! Trusted callback control, separate from lossless JSON program values (exit flag plus sentinel).
use mlua::{UserData, Value};
use tokio_util::sync::CancellationToken;

pub(super) struct Complete;
impl UserData for Complete {}

#[derive(Clone, Default)]
pub(super) struct Signal(pub CancellationToken);

pub(super) fn requested(values: &mlua::MultiValue) -> bool {
    values.len() == 1 && matches!(values.front(), Some(Value::UserData(value)) if value.is::<Complete>())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_single_typed_host_control_requests_completion() {
        let lua = mlua::Lua::new();
        let marker = Value::UserData(lua.create_userdata(Complete).unwrap());
        assert!(requested(&mlua::MultiValue::from_vec(vec![marker.clone()])));
        assert!(!requested(&mlua::MultiValue::from_vec(vec![marker, Value::Nil])));
        assert!(!requested(&mlua::MultiValue::from_vec(vec![Value::String(
            lua.create_string("sandbox execution completed").unwrap(),
        )])));
        assert!(!requested(&mlua::MultiValue::from_vec(vec![Value::Table(
            lua.create_table_from([("complete", true)]).unwrap(),
        )])));
    }
}
