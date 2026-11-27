//! Embed the kernel's Lua libraries (`lua/`: the vendored cordis-lua and `mare_kernel`) as
//! `(module name, bytecode)` pairs in `kernel_lua.rs`, served by the `require` searcher
//! (`lua_env/modules.rs`). Each file is compiled here with the same vendored Lua 5.5 the binary
//! uses, under the chunk name the searcher gives the module (`@kernel/lua/<module with / for .>.lua`), keeping
//! debug info (line numbers in errors and tracebacks): startup no longer compiles ~7k lines of
//! Lua. Every profile embeds bytecode, so tests run the
//! shipped path. Only `.lua` files are embedded (`VENDOR`, `LICENSE`, `NOTICE` are not); the
//! sources stay the vendored files (`scripts/vendor-cordis.sh --check`).

use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("lua");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut mods = vec![];
    collect(&root, &root, &mut mods);
    mods.sort();
    let out_dir = Path::new(&std::env::var("OUT_DIR").unwrap()).join("kernel_lua");
    std::fs::create_dir_all(&out_dir).unwrap();
    let lua = mlua::Lua::new();
    let mut list = String::from("&[\n");
    for (module, rel) in mods {
        let p = root.join(&rel);
        println!("cargo:rerun-if-changed={}", p.display());
        let src = std::fs::read_to_string(&p).unwrap();
        let f = lua
            .load(&src)
            .set_name(format!("@kernel/lua/{}.lua", module.replace('.', "/")))
            .into_function()
            .unwrap_or_else(|e| panic!("compiling lua/{rel}: {e}"));
        let luac = out_dir.join(format!("{module}.luac"));
        std::fs::write(&luac, f.dump(false)).unwrap();
        let _ = writeln!(list, "    ({module:?}, include_bytes!({:?}) as &[u8]),", luac.display().to_string());
    }
    list.push(']');
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("kernel_lua.rs"), list).unwrap();
}

/// `(module name, path relative to lua/)` for every `.lua` below `dir` (`a/b.lua` → `a.b`,
/// `a/init.lua` → `a`).
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            println!("cargo:rerun-if-changed={}", p.display());
            collect(root, &p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("lua") {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let module = rel.trim_end_matches(".lua").trim_end_matches("/init").replace('/', ".");
            out.push((module, rel));
        }
    }
}
