//! A bounded, single-use Lua capability VM in a dedicated process. The private JSON pipe
//! exposes only named host callbacks. Worker death cannot imply rollback of host effects.
mod bridge;
mod completion;
mod execution;
mod limits;
pub(crate) mod local;
mod output;
mod process;
#[cfg(test)]
mod tests;
mod timers;
mod wire;
mod worker;

pub const WORKER_ARG: &str = "__lua-vm-worker";
/// Runtime entry used by the mare executable before loading config or plugins.
pub fn worker_main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let result = tokio::task::LocalSet::new().block_on(&rt, worker::run());
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    result
}
pub fn install(lua: &mlua::Lua, mare: &mlua::Table) -> mlua::Result<()> {
    process::install(lua, mare)
}
