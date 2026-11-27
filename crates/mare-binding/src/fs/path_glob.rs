//! Separator-aware file path globs shared by permission rules and reminders.

use std::path::{Component, Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use mlua::{Lua, MultiValue, Table, UserData, UserDataMethods, Value};

struct PathGlob {
    matcher: GlobMatcher,
    conservative: GlobMatcher,
    prefix: PathBuf,
    recursive_root: Option<PathBuf>,
    literal: Option<String>,
}

fn normalized_string(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) { s.replace('\\', "/") } else { s }
}

impl PathGlob {
    fn compile(pattern: &str, base: &Path) -> Result<Self, String> {
        if pattern.is_empty() || pattern.contains('\0') || pattern.len() > mare_store::bounded::MAX_BYTES as usize {
            return Err("invalid path pattern".into());
        }
        let expanded = mare_shared::paths::expand(pattern);
        let absolute = if expanded.is_absolute() { expanded } else { base.join(expanded) };
        if !absolute.is_absolute() {
            return Err("path pattern base must be absolute".into());
        }
        let mut normalized = PathBuf::new();
        let mut wildcard = false;
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir if wildcard => return Err("parent after a path glob is not allowed".into()),
                Component::ParentDir => {
                    normalized.pop();
                }
                part => {
                    wildcard |= part.as_os_str().to_string_lossy().chars().any(|c| matches!(c, '*' | '?' | '[' | '{'));
                    normalized.push(part.as_os_str());
                }
            }
        }
        let text = normalized_string(&normalized);
        let compile = |insensitive| {
            GlobBuilder::new(&text)
                .literal_separator(true)
                .case_insensitive(insensitive)
                .backslash_escape(!cfg!(windows))
                .build()
                .map(|g| g.compile_matcher())
                .map_err(|e| format!("invalid path glob: {e}"))
        };
        let matcher = compile(cfg!(windows))?;
        let conservative = compile(cfg!(any(windows, target_os = "macos")))?;
        let first_wild = text.find(['*', '?', '[', '{']);
        let prefix = match first_wild {
            Some(i) => PathBuf::from(
                text[..i].rsplit_once('/').map(|(s, _)| if s.is_empty() { "/" } else { s }).unwrap_or("/"),
            ),
            None => normalized.clone(),
        };
        let recursive_root = text.strip_suffix("/**").filter(|s| !s.contains(['*', '?', '[', '{'])).map(PathBuf::from);
        Ok(Self { matcher, conservative, prefix, recursive_root, literal: (!wildcard).then_some(text) })
    }
}

impl UserData for PathGlob {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("matches", |_, this, (path, conservative): (String, Option<bool>)| {
            let matcher = if conservative == Some(true) { &this.conservative } else { &this.matcher };
            Ok(matcher.is_match(normalized_string(Path::new(&path))))
        });
        methods.add_method("overlaps", |_, this, path: String| {
            let path = Path::new(&path);
            Ok(crate::realpath::path_within_with(path, &this.prefix, true)
                || crate::realpath::path_within_with(&this.prefix, path, true))
        });
        methods.add_method("covers_recursive", |_, this, (path, conservative): (String, Option<bool>)| {
            Ok(this.recursive_root.as_ref().is_some_and(|root| {
                crate::realpath::path_within_with(Path::new(&path), root, conservative == Some(true))
            }))
        });
        methods.add_method("literal", |_, this, ()| Ok(this.literal.clone()));
    }
}

pub(super) fn install(lua: &Lua, t: &Table, cwd: &Path) -> mlua::Result<()> {
    let cwd = cwd.to_path_buf();
    t.set(
        "path_within",
        lua.create_function(|_, (path, root, conservative): (String, String, Option<bool>)| {
            Ok(crate::realpath::path_within_with(Path::new(&path), Path::new(&root), conservative == Some(true)))
        })?,
    )?;
    t.set(
        "compile_path_glob",
        lua.create_function(move |lua, (pattern, base): (String, Option<String>)| {
            let base = base.as_deref().map(Path::new).unwrap_or(&cwd);
            match PathGlob::compile(&pattern, base) {
                Ok(glob) => Ok(MultiValue::from_vec(vec![Value::UserData(lua.create_userdata(glob)?)])),
                Err(err) => mare_shared::lua::nil_err(lua, err),
            }
        })?,
    )
}
