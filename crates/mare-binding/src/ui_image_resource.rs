//! Scoped image resolvers. The native image workers send bounded requests; callbacks run on
//! Lua's async host thread in a fresh read-only execution, never in the renderer.
use mare_shared::{
    image_resource::{Reference, Resolver},
    lua::rt_err,
};
use mlua::{Function, Lua, Table};
use std::{cell::RefCell, collections::HashMap, sync::Arc};
#[derive(Default)]
struct Resolvers(RefCell<HashMap<(u64, String), Arc<Resolver>>>);
pub(crate) fn reference(lua: &Lua, namespace: &str, key: String, width: u32, height: u32) -> mlua::Result<Table> {
    let scope = mare_shared::image_resource::current_scope();
    let id = lua
        .app_data_ref::<Resolvers>()
        .and_then(|r| r.0.borrow().get(&(scope, namespace.to_owned())).map(|r| r.id()))
        .ok_or_else(|| rt_err("image namespace unavailable"))?;
    Reference::new(id, key.clone(), width, height).map_err(rt_err)?;
    let t = lua.create_table()?;
    t.set("namespace", namespace)?;
    t.set("key", key)?;
    t.set("width", width)?;
    t.set("height", height)?;
    t.set("resolver_id", id)?;
    Ok(t)
}
pub(crate) fn install(lua: &Lua, ui: &Table) -> mlua::Result<()> {
    lua.set_app_data(Resolvers::default());
    ui.set(
        "image_cache_info",
        lua.create_function(|lua, ()| {
            let (bytes, entries) = mare_shared::image_resource::cache_info();
            let t = lua.create_table()?;
            t.set("materialized_bytes", bytes)?;
            t.set("entries", entries)?;
            t.set("budget_bytes", mare_shared::image_resource::CACHE_BYTES)?;
            t.set("max_image_bytes", mare_shared::image_resource::MAX_IMAGE_BYTES)?;
            t.set("max_pending_per_resolver", mare_shared::image_resource::MAX_PENDING)?;
            Ok(t)
        })?,
    )?;
    ui.set(
        "image_resource",
        lua.create_function(|lua, (name, key, width, height): (String, String, u32, u32)| {
            reference(lua, &name, key, width, height)
        })?,
    )?;
    ui.set("image_resolver", lua.create_function(register)?)?;
    Ok(())
}

fn register(lua: &Lua, (name, loader): (String, Function)) -> mlua::Result<Function> {
    mare_shared::exec_ctx::check_mutation(lua, "register image resolver")?;
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)) {
        return Err(rt_err("invalid image namespace"));
    }
    let scope = mare_shared::image_resource::current_scope();
    let (resolver, requests) = Resolver::register();
    {
        let registry = lua.app_data_ref::<Resolvers>().ok_or_else(|| rt_err("image registry unavailable"))?;
        let mut registry = registry.0.borrow_mut();
        if registry.len() >= 64 {
            return Err(rt_err("too many image resolver registrations"));
        }
        let key = (scope, name.clone());
        if registry.contains_key(&key) {
            return Err(rt_err("image namespace already registered"));
        }
        registry.insert(key, resolver.clone());
    }
    // Resolve the validated public gate at registration, not through mutable globals per load.
    let mare: Table = lua.globals().get("mare")?;
    let exec: Table = mare.get("exec")?;
    let run: Function = exec.get("run")?;
    mare_shared::txn_scope::spawn_local(resolve_requests(
        lua.clone(),
        name.clone(),
        resolver.clone(),
        requests,
        loader,
        run,
    ));
    lua.create_function(move |lua, ()| {
        mare_shared::exec_ctx::check_mutation(lua, "unregister image resolver")?;
        resolver.close();
        if let Some(registry) = lua.app_data_ref::<Resolvers>() {
            let mut map = registry.0.borrow_mut();
            let key = (scope, name.clone());
            if map.get(&key).is_some_and(|r| r.id() == resolver.id()) {
                map.remove(&key);
            }
        }
        Ok(())
    })
}

async fn resolve_requests(
    lua: Lua,
    name: String,
    resolver: Arc<Resolver>,
    mut requests: tokio::sync::mpsc::Receiver<mare_shared::image_resource::Request>,
    loader: Function,
    run: Function,
) {
    loop {
        let request = tokio::select! {
            biased;
            _ = resolver.cancelled() => break,
            request = requests.recv() => match request { Some(request) => request, None => break },
        };
        let result: mlua::Result<mlua::LuaString> = async {
            let options = lua.create_table()?;
            options.set("tool", "ui-image-resolver")?;
            options.set("lifetime", "ephemeral-read-only")?;
            let sandbox = lua.create_table()?;
            sandbox.set("level", "read-only")?;
            sandbox.set("network", false)?;
            options.set("sandbox", sandbox)?;
            tokio::select! {
                biased;
                _ = resolver.cancelled() => Err(rt_err("image resolver closed")),
                _ = request.cancelled() => Err(rt_err("image resource request cancelled")),
                result = tokio::time::timeout(std::time::Duration::from_secs(30),
                    run.call_async((options, loader.clone(), request.reference.key.to_string()))) =>
                    result.map_err(|_|rt_err("image resolver timeout"))?,
            }
        }
        .await;
        if let Err(error) = &result {
            tracing::warn!(namespace=%name,error=%error,"image resource load failed");
        }
        request.complete(result.map(|s| s.as_bytes().to_vec()).map_err(|e| e.to_string()));
    }
}
