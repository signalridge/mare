use mlua::{Lua, Value};

pub const COV_TARGET_REGISTRY: &str = "mare.lua_cov_target";

pub type CovLineCallback = fn(*mut std::ffi::c_void, &str, usize);

#[repr(C)]
struct CovTarget {
    cb: CovLineCallback,
    ctx: *mut std::ffi::c_void,
}

/// Returns true if `COV_TARGET_REGISTRY` is installed in `l`'s registry.
///
/// # Safety
/// `l` must be a valid Lua state pointer.
pub unsafe fn has_cov_hook_raw(l: *mut mlua::ffi::lua_State) -> bool {
    unsafe {
        mlua::ffi::lua_getfield(l, mlua::ffi::LUA_REGISTRYINDEX, c"mare.lua_cov_target".as_ptr());
        let ptr = mlua::ffi::lua_touserdata(l, -1);
        mlua::ffi::lua_pop(l, 1);
        !ptr.is_null()
    }
}

/// Dispatch a `LUA_HOOKLINE` event to the installed `CovTarget` (if any).
///
/// # Safety
/// `l` and `ar` must be valid pointers provided by a Lua debug hook.
pub unsafe fn dispatch_cov_line_hook(l: *mut mlua::ffi::lua_State, ar: *mut mlua::ffi::lua_Debug) {
    unsafe {
        if mlua::ffi::lua_getinfo(l, c"Sl".as_ptr(), ar) == 0 {
            return;
        }
        let line = (*ar).currentline;
        if line <= 0 {
            return;
        }
        let src_ptr = (*ar).source;
        if src_ptr.is_null() {
            return;
        }
        let Ok(source) = std::ffi::CStr::from_ptr(src_ptr).to_str() else {
            return;
        };
        mlua::ffi::lua_getfield(l, mlua::ffi::LUA_REGISTRYINDEX, c"mare.lua_cov_target".as_ptr());
        let ptr = mlua::ffi::lua_touserdata(l, -1) as *const CovTarget;
        mlua::ffi::lua_pop(l, 1);
        if !ptr.is_null() {
            let target = &*ptr;
            (target.cb)(target.ctx, source, line as usize);
        }
    }
}

unsafe extern "C-unwind" fn cov_c_line_hook(l: *mut mlua::ffi::lua_State, ar: *mut mlua::ffi::lua_Debug) {
    unsafe {
        dispatch_cov_line_hook(l, ar);
    }
}

struct TargetDrop(*mut CovTarget);
impl Drop for TargetDrop {
    fn drop(&mut self) {
        unsafe { drop(Box::from_raw(self.0)) };
    }
}

/// Fast O(1) check for whether a Lua coverage hook is installed on `lua`.
pub fn has_cov_hook(lua: &Lua) -> bool {
    lua.app_data_ref::<TargetDrop>().is_some()
}

/// Install a C-level Lua line hook on the main thread and register it for child threads.
///
/// # Safety
/// `ctx` must remain valid until the `Lua` instance is dropped.
pub unsafe fn install_cov_hook(lua: &Lua, cb: CovLineCallback, ctx: *mut std::ffi::c_void) -> mlua::Result<()> {
    let target = Box::into_raw(Box::new(CovTarget { cb, ctx }));
    let ud = Value::LightUserData(mlua::LightUserData(target.cast()));
    lua.set_named_registry_value(COV_TARGET_REGISTRY, ud)?;
    // Free the boxed CovTarget when the Lua VM is dropped.
    lua.set_app_data(TargetDrop(target));
    let main_th = lua.current_thread();
    unsafe {
        mlua::ffi::lua_sethook(
            main_th.to_pointer() as *mut mlua::ffi::lua_State,
            Some(cov_c_line_hook),
            mlua::ffi::LUA_MASKLINE as _,
            0,
        );
    }
    Ok(())
}

/// Attach the active Lua coverage line hook (if any) to a newly created thread/coroutine.
pub fn attach_cov_hook(lua: &Lua, th: &mlua::Thread) {
    if !has_cov_hook(lua) {
        return;
    }
    unsafe {
        let state = th.to_pointer() as *mut mlua::ffi::lua_State;
        if let Some(existing) = mlua::ffi::lua_gethook(state) {
            let mask = mlua::ffi::lua_gethookmask(state);
            let count = mlua::ffi::lua_gethookcount(state);
            if (mask & mlua::ffi::LUA_MASKLINE as std::ffi::c_int) == 0 {
                mlua::ffi::lua_sethook(state, Some(existing), mask | mlua::ffi::LUA_MASKLINE as std::ffi::c_int, count);
            }
        } else {
            mlua::ffi::lua_sethook(state, Some(cov_c_line_hook), mlua::ffi::LUA_MASKLINE as _, 0);
        }
    }
}

/// Call `f(args)` asynchronously, attaching the Lua coverage hook if active.
pub async fn call_async<R: mlua::FromLuaMulti>(
    lua: &Lua,
    f: &mlua::Function,
    args: impl mlua::IntoLuaMulti,
) -> mlua::Result<R> {
    if has_cov_hook(lua) {
        const KEY: &str = "mare.cov_call_wrapper";
        let wrapper = match lua.named_registry_value::<mlua::Function>(KEY) {
            Ok(wrapper) => wrapper,
            Err(_) => {
                let attach = lua.create_function(|lua, ()| {
                    attach_cov_hook(lua, &lua.current_thread());
                    Ok(())
                })?;
                let wrapper: mlua::Function = lua
                    .load("local attach = ...; return function(f, ...) attach(); return f(...) end")
                    .set_name("=mare.cov_call_wrapper")
                    .call(attach)?;
                lua.set_named_registry_value(KEY, &wrapper)?;
                wrapper
            }
        };
        wrapper.call_async::<R>((f.clone(), args)).await
    } else {
        f.call_async::<R>(args).await
    }
}
