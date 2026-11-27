//! policy layer (invariant tests 1–2): a tool from any plugin can't bypass the sandbox
//! policy (direct, task, coroutine, io.open, timeout), plugins outside tool calls are trusted,
//! and policy hooks fail closed (timeout, error, invalid vote).

use crate::common::*;

mod exec_entries;
mod fs;
mod nested_denials;
mod rewrites_and_schemas;
mod sandbox;
mod votes_and_approvals;

pub(super) async fn policy_eval<T: mlua::FromLuaMulti>(code: &str) -> T {
    let env = TestEnv::new().await;
    let h = env.host().no_session().build().await;
    expose(&h);
    let v = eval::<T>(&h, code).await;
    h.shutdown().await;
    v
}

pub(super) async fn echo_host() -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    let env = TestEnv::new().await;
    let h = env.host().no_session().build().await;
    expose(&h);
    exec(
        &h,
        r#"T:register("echo", {
               description = "x",
               parameters = { type = "object", properties = { text = { type = "string" } }, required = { "text" } },
               execute = function(a) RAN = (RAN or 0) + 1 return "ran:" .. a.text end,
           })"#,
    )
    .await;
    (env, h)
}
