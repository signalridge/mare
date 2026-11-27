//! Shared bounded read path. Sanitizer state spans chunks, including UTF-8 and OSC/DCS.
use super::*;

impl Pty {
    fn accept_chunk(&self, lua: &Lua, chunk: Option<Vec<u8>>) -> mlua::Result<Option<Vec<u8>>> {
        if let Some(bytes) = &chunk
            && self.view_enabled
        {
            let replies = self.view.lock().unwrap().feed(bytes);
            for reply in replies {
                self.lease.check(lua)?;
                self.write(&reply)?;
            }
        }
        Ok(chunk)
    }

    async fn next(&self, lua: &Lua) -> mlua::Result<Option<Vec<u8>>> {
        crate::exec_lifetime::check_mutation(lua, "pty read")?;
        self.lease.check(lua)?;
        self.accept_chunk(lua, self.read_chunk().await)
    }
}

pub(super) fn methods<M: UserDataMethods<Pty>>(m: &mut M) {
    m.add_method("view", |lua, this, opts: Option<Table>| {
        this.lease.check(lua)?;
        if !this.view_enabled {
            return Err(rt_err("PTY_VIEW_DISABLED: spawn with emulate=true"));
        }
        let max_bytes =
            opts.as_ref().map(|o| o.get::<Option<usize>>("max_bytes")).transpose()?.flatten().unwrap_or(256 * 1024);
        if max_bytes == 0 || max_bytes > 4 * 1024 * 1024 {
            return Err(rt_err("PTY_VIEW_LIMIT: invalid view budget"));
        }
        let include_screen =
            opts.as_ref().map(|o| o.get::<Option<bool>>("screen")).transpose()?.flatten().unwrap_or(true);
        this.view.lock().unwrap().snapshot(lua, max_bytes, include_screen)
    });
    m.add_method("page", |lua, this, opts: Table| {
        this.lease.check(lua)?;
        if !this.view_enabled {
            return Err(rt_err("PTY_VIEW_DISABLED: spawn with emulate=true"));
        }
        let offset = opts.get::<Option<usize>>("offset")?.unwrap_or(0);
        let count = opts.get::<Option<usize>>("count")?.unwrap_or(500);
        let max_bytes = opts.get::<Option<usize>>("max_bytes")?.unwrap_or(256 * 1024);
        if count == 0 || count > 100000 || max_bytes == 0 || max_bytes > 4 * 1024 * 1024 {
            return Err(rt_err("PTY_VIEW_LIMIT: invalid page budget"));
        }
        this.view.lock().unwrap().page(lua, offset, count, max_bytes)
    });
    // Raw bytes are retained for terminal emulators; never display them as trusted UI text.
    m.add_async_method("read", |lua, this, ()| async move {
        this.next(&lua).await?.map(|b| lua.create_string(b)).transpose()
    });
    m.add_async_method("read_text", |lua, this, ()| async move {
        loop {
            let gen_before = this.view.lock().unwrap().prompt_generation();
            let next = this.next(&lua).await?;
            let eof = next.is_none();
            let mut out = Vec::new();
            {
                let mut filter = this.filter.lock().unwrap();
                if let Some(b) = next {
                    filter.feed(&b, &mut out);
                } else {
                    filter.finish(&mut out);
                }
            }
            if !out.is_empty() || this.view.lock().unwrap().prompt_generation() != gen_before {
                return Ok(Some(lua.create_string(out)?));
            }
            if eof {
                return Ok(None);
            }
        }
    });
}
