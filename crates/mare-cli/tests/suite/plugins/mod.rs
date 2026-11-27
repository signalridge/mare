//! User plugins through the host: a two-layer plugin (`init.lua` + `lua/` modules) reloads its
//! modules (Neovim runtimepath style).

mod api_surface;
mod builtin_apply_budgets;
mod builtin_lazy_modules;
mod declaration_checks;
mod lifecycle;
mod luac_cache;
mod mcp;
mod overrides;
mod private_surface;
mod require;
mod runtime_apis;
mod slots;
