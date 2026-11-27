//! Configuration provenance belongs to the coroutine evaluating that layer, not a mutable
//! global seen by unrelated runtime work while init.lua yields.

use std::collections::HashMap;
use std::path::Path;

use mlua::{Lua, Table, Thread, Value};

#[derive(Clone)]
struct Origin {
    layer: String,
    file: String,
}

#[derive(Default)]
struct Origins(HashMap<usize, Origin>);

pub(crate) struct Scope {
    lua: Lua,
    thread: usize,
    previous: Option<Origin>,
    previous_thread: Value,
}

impl Scope {
    pub(crate) fn enter(lua: &Lua, thread: &Thread, layer: &str, file: &Path) -> mlua::Result<Self> {
        let previous_thread = mare_shared::lua::core::config::config_coroutine(lua)?;
        mare_shared::lua::core::config::set_config_coroutine(lua, Value::Thread(thread.clone()))?;
        let thread = thread.to_pointer() as usize;
        if lua.app_data_ref::<Origins>().is_none() {
            lua.set_app_data(Origins::default());
        }
        let previous = lua
            .app_data_mut::<Origins>()
            .expect("origin map installed")
            .0
            .insert(thread, Origin { layer: layer.to_owned(), file: file.to_string_lossy().into_owned() });
        Ok(Self { lua: lua.clone(), thread, previous, previous_thread })
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let (empty, previous_thread) = {
            let Some(mut origins) = self.lua.app_data_mut::<Origins>() else { return };
            origins.0.remove(&self.thread);
            if let Some(previous) = self.previous.take() {
                origins.0.insert(self.thread, previous);
            }
            let previous_thread = match &self.previous_thread {
                Value::Thread(previous) if origins.0.contains_key(&(previous.to_pointer() as usize)) => {
                    self.previous_thread.clone()
                }
                _ => Value::Nil,
            };
            (origins.0.is_empty(), previous_thread)
        };
        if empty {
            self.lua.remove_app_data::<Origins>();
        }
        // A non-LIFO completion must neither clear another active coroutine's marker nor restore a
        // coroutine whose scope has already ended.
        if mare_shared::lua::core::config::config_coroutine(&self.lua)
            .is_ok_and(|value| matches!(value, Value::Thread(thread) if thread.to_pointer() as usize == self.thread))
        {
            let _ = mare_shared::lua::core::config::set_config_coroutine(&self.lua, previous_thread);
        }
    }
}

pub(crate) fn current_layer(lua: &Lua) -> String {
    lua.app_data_ref::<Origins>()
        .and_then(|origins| origins.0.get(&(lua.current_thread().to_pointer() as usize)).map(|o| o.layer.clone()))
        .unwrap_or_else(|| "runtime".into())
}

pub(crate) fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    mare.set(
        "config_origin",
        lua.create_function(|lua, ()| {
            let origin = lua
                .app_data_ref::<Origins>()
                .and_then(|origins| origins.0.get(&(lua.current_thread().to_pointer() as usize)).cloned());
            let out = lua.create_table()?;
            out.set("layer", origin.as_ref().map_or("runtime", |origin| origin.layer.as_str()))?;
            if let Some(origin) = origin {
                out.set("file", origin.file)?;
            }
            Ok(out)
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_is_scoped_to_the_config_coroutine_and_restored_on_drop() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        install(&lua, &mare).unwrap();
        let read: mlua::Function = lua.load("return function() return mare.config_origin() end").eval().unwrap();
        let config = lua.create_thread(read.clone()).unwrap();
        let runtime = lua.create_thread(read.clone()).unwrap();
        {
            mare_shared::lua::core::config::set_in_config(&lua, true).unwrap();
            let _scope = Scope::enter(&lua, &config, "project", Path::new(".mare/init.lua")).unwrap();
            assert!(mare.raw_get::<Value>("_config_coroutine").unwrap().is_nil());
            // A forged field on `mare` cannot change host-owned provenance.
            mare.set("_layer", "user").unwrap();
            let unrelated: Table = runtime.resume(()).unwrap();
            assert_eq!(unrelated.get::<String>("layer").unwrap(), "runtime");
            assert!(unrelated.get::<Value>("file").unwrap().is_nil());
            let current: Table = config.resume(()).unwrap();
            assert_eq!(current.get::<String>("layer").unwrap(), "project");
            assert_eq!(current.get::<String>("file").unwrap(), ".mare/init.lua");
            mare_shared::lua::core::config::set_in_config(&lua, false).unwrap();
        }
        assert!(mare_shared::lua::core::config::config_coroutine(&lua).unwrap().is_nil());
        assert!(mare.raw_get::<Value>("_config_coroutine").unwrap().is_nil());
        assert!(lua.app_data_ref::<Origins>().is_none());
        let after: Table = read.call(()).unwrap();
        assert_eq!(after.get::<String>("layer").unwrap(), "runtime");
    }

    #[test]
    fn overlapping_config_coroutines_keep_their_origins_when_scopes_end_out_of_order() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        install(&lua, &mare).unwrap();
        let read: mlua::Function = lua
            .load("return function() coroutine.yield(mare.config_origin()); return mare.config_origin() end")
            .eval()
            .unwrap();
        let user = lua.create_thread(read.clone()).unwrap();
        let project = lua.create_thread(read).unwrap();
        let user_scope = Scope::enter(&lua, &user, "user", Path::new("user/init.lua")).unwrap();
        let project_scope = Scope::enter(&lua, &project, "project", Path::new("project/init.lua")).unwrap();
        let observed_user: Table = user.resume(()).unwrap();
        let observed_project: Table = project.resume(()).unwrap();
        assert_eq!(observed_user.get::<String>("layer").unwrap(), "user");
        assert_eq!(observed_project.get::<String>("layer").unwrap(), "project");
        drop(user_scope);
        let ended_user: Table = user.resume(()).unwrap();
        assert_eq!(ended_user.get::<String>("layer").unwrap(), "runtime");
        let active_project: Table = project.resume(()).unwrap();
        assert_eq!(active_project.get::<String>("layer").unwrap(), "project");
        assert_eq!(active_project.get::<String>("file").unwrap(), "project/init.lua");
        drop(project_scope);
        assert!(lua.app_data_ref::<Origins>().is_none());
        assert!(mare.get::<Value>("_config_coroutine").unwrap().is_nil());
    }
}
