//! Embedding macros for the built-in plugin preset (Yazi `yazi-macro` `plugin_preset!` style).

/// Embed preset files of the calling crate: `preset!("plugins/agent.lua")` →
/// `include_str!("<crate>/preset/plugins/agent.lua")`. Release builds ship the embedded copy;
/// [`preset_live!`] lets debug builds read the file from disk instead.
#[macro_export]
macro_rules! preset {
    ($path:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/preset/", $path))
    };
}

/// A preset text file as `Cow<'static, str>`: embedded in release builds; read from the source
/// tree at runtime in debug builds, so editing a preset file never triggers a recompile (the
/// dev loop: edit Lua, `/reload`).
#[macro_export]
macro_rules! preset_text {
    ($path:literal) => {{
        #[cfg(debug_assertions)]
        let text: ::std::borrow::Cow<'static, str> = ::std::borrow::Cow::Owned(
            ::std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/preset/", $path))
                .unwrap_or_else(|e| panic!("reading preset/{}: {e}", $path)),
        );
        #[cfg(not(debug_assertions))]
        let text: ::std::borrow::Cow<'static, str> =
            ::std::borrow::Cow::Borrowed(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/preset/", $path)));
        text
    }};
}

/// Just the names (debug builds, which read sources live).
#[macro_export]
macro_rules! plugin_names {
    ($($name:literal),* $(,)?) => {
        &[$($name),*]
    };
}

/// A list of `(name, embedded source)` for `preset/plugins/<name>.lua`.
#[macro_export]
macro_rules! plugin_preset {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/preset/plugins/", $name, ".lua")))),*]
    };
}

/// Directory of the preset in the source tree (used by debug builds to read live files).
#[macro_export]
macro_rules! preset_dir {
    () => {
        concat!(env!("CARGO_MANIFEST_DIR"), "/preset")
    };
}

/// `(name, bytecode)` for built-in plugins precompiled by the calling crate's build.rs into
/// `$OUT_DIR/bytecode/<name>.luac`.
#[macro_export]
macro_rules! plugin_bytecode {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/", $name, ".luac")) as &[u8])),*]
    };
}
