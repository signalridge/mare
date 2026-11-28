//! Bounded binary access to the same managed spill_ids as UTF-8 spill storage.
//! No second namespace, path scheme, persistence lifecycle, or permission bypass.
use super::*;
use std::io::Read;

pub const MAX_BYTES: usize = 8 * 1024 * 1024;
type BytePage = (Vec<u8>, bool, u64, u64);
type SpillErr = (&'static str, String);

fn page_bytes(path: &Path, cursor: u64, limit: usize) -> Result<BytePage, SpillErr> {
    if limit == 0 || limit > MAX_BYTES {
        return Err(("invalid_arguments", "spill byte limit must be 1..8388608".into()));
    }
    let mut file = open_read(path).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
    let size = file.metadata().map_err(|e| ("spill.unknown_spill", e.to_string()))?.len();
    if cursor > size {
        return Err(("invalid_arguments", "spill byte cursor exceeds output size".into()));
    }
    file.seek(SeekFrom::Start(cursor)).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
    let mut bytes = Vec::with_capacity((size - cursor).min(limit as u64) as usize);
    file.take(limit as u64).read_to_end(&mut bytes).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
    let next = cursor + bytes.len() as u64;
    Ok((bytes, next < size, next, size))
}

pub(super) fn install(lua: &Lua, api: &Table, store: std::sync::Arc<local::Store>) -> mlua::Result<()> {
    let save_store = store.clone();
    let save_bytes_raw =
        lua.create_async_function(move |lua, (owner, value, opts): (String, mlua::LuaString, Option<Table>)| {
            let store = save_store.clone();
            async move {
                if value.as_bytes().len() > MAX_BYTES {
                    return coded_result(&lua, "invalid_arguments", "spill binary output exceeds 8388608 bytes");
                }
                let save_opts = match parse_save_opts(opts.as_ref()) {
                    Ok(v) => v,
                    Err(e) => return coded_result(&lua, "invalid_arguments", &e.to_string()),
                };
                save_value(&lua, &store, owner, value.as_bytes().to_vec(), save_opts).await
            }
        })?;
    api.set("save_bytes", mare_shared::lua_error::wrap_coded(lua, "@spill/wrap", save_bytes_raw)?)?;
    let read_bytes_raw = lua.create_async_function(move |lua, args: mlua::MultiValue| {
        let store = store.clone();
        async move {
            let (spill_id, opts) = match parse_spill_args(args, "mare.spill.read_bytes", true) {
                Ok(v) => v,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if let Some(opts) = &opts {
                for item in opts.clone().pairs::<String, mlua::Value>() {
                    let (key, _) = item?;
                    if !matches!(key.as_str(), "cursor" | "limit" | "session_id") {
                        return coded_result(&lua, "invalid_arguments", &format!("unknown spill byte option: {key}"));
                    }
                }
            }
            let cursor = match opt_nonneg_u64(opts.as_ref(), "cursor") {
                Ok(v) => v.unwrap_or(0),
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let limit = match opt_nonneg_usize(opts.as_ref(), "limit") {
                Ok(v) => v.unwrap_or(1024 * 1024),
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if limit == 0 || limit > MAX_BYTES {
                return coded_result(&lua, "invalid_arguments", "spill byte limit must be 1..8388608");
            }
            let owner = match resolve_read_or_delete_owner(&lua, opts.as_ref()) {
                Ok(owner) => owner,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let _lease = store.lease().await?;
            let base = store.config.root.clone();
            let path = match path_for_spill_id(&base, &owner, &spill_id) {
                Ok(p) => p,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if let Err(e) = check(&lua, &path, true) {
                return coded_result(&lua, "denied", &e.to_string());
            }
            let paged = tokio::task::spawn_blocking(move || {
                validate(&base, &path).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
                page_bytes(&path, cursor, limit)
            })
            .await
            .map_err(rt_err)?;
            let (bytes, more, next, size) = match paged {
                Ok(v) => v,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let out = lua.create_table()?;
            out.set("bytes", lua.create_string(&bytes)?)?;
            out.set("more", more)?;
            out.set("next_byte", next)?;
            out.set("size", size)?;
            Ok((true, mlua::Value::Table(out)))
        }
    })?;
    api.set("read_bytes", mare_shared::lua_error::wrap_coded(lua, "@spill/wrap", read_bytes_raw)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_roundtrip_pages_and_text_compatibility() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("output");
        let data = [0, 255, 137, 80, 78, 71, 13, 10, 26, 10];
        let path = save(&base, "session", &data).unwrap();
        validate(&base, &path).unwrap();
        assert_eq!(page_bytes(&path, 0, 4).unwrap(), (data[..4].to_vec(), true, 4, 10));
        assert_eq!(page_bytes(&path, 4, 8).unwrap(), (data[4..].to_vec(), false, 10, 10));
        assert_eq!(page_bytes(&path, 10, 8).unwrap(), (vec![], false, 10, 10));
        assert!(page_bytes(&path, 11, 1).is_err());
        assert!(page_bytes(&path, 0, MAX_BYTES + 1).is_err());
        assert!(page_bytes(&path, 0, 0).is_err());
        assert!(page(&path, 1, 200, 0).is_err());
        let text = save(&base, "session", "中文\n".as_bytes()).unwrap();
        assert_eq!(page_bytes(&text, 0, 100).unwrap().0, "中文\n".as_bytes());
        assert_eq!(page(&text, 1, 200, 0).unwrap().0, "中文\n");
    }
}
