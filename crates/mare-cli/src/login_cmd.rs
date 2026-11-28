//! `mare login [provider] [--with-api-key] [--device-auth]`, `mare login status`,
//! `mare logout [provider]`, `mare model refresh`:
//! a headless host whose `llm` plugin answers the `login/cli` event. The CLI gives it `say`
//! (print a line) and `ask` (read a line from a terminal; on a pipe it never answers, so a browser
//! login waits for its callback); an API key only ever comes from stdin (`--with-api-key`).

use std::io::{IsTerminal, Read, Write};

use anyhow::Result;
use mare_host::{Host, HostOptions};

pub enum Request {
    Login { provider: Option<String>, with_api_key: bool, device_auth: bool, oauth: bool },
    Status,
    Logout(Option<String>),
    ModelsRefresh,
}

pub async fn run(opts: HostOptions, req: Request) -> Result<i32> {
    let mut key = None;
    let mut with_api_key_tty = false;
    if let Request::Login { with_api_key: true, .. } = &req {
        if std::io::stdin().is_terminal() {
            with_api_key_tty = true;
        } else {
            let mut s = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut s) {
                eprintln!("mare: --with-api-key: could not read stdin: {e}");
                return Ok(2);
            }
            key = Some(s.trim().to_string());
        }
    }
    let host = match Host::new(opts).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("mare: {e:#}");
            return Ok(2);
        }
    };
    let lua = host.lua();
    let t = lua.create_table()?;
    match &req {
        Request::Login { provider, device_auth, oauth, .. } => {
            t.set("action", "login")?;
            t.set("provider", provider.clone())?;
            t.set("key", key)?;
            t.set("tty", std::io::stdin().is_terminal() && std::io::stdout().is_terminal())?;
            if with_api_key_tty {
                t.set("with_api_key_tty", true)?;
            }
            if *oauth {
                t.set("method", "oauth")?;
            } else if *device_auth {
                t.set("method", "device")?;
            }
        }
        Request::Status => t.set("action", "status")?,
        Request::Logout(p) => {
            t.set("action", "logout")?;
            t.set("provider", p.clone())?;
        }
        Request::ModelsRefresh => t.set("action", "models-refresh")?,
    }
    t.set(
        "say",
        lua.create_function(|_, text: String| {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
            Ok(())
        })?,
    )?;
    t.set(
        "ask",
        lua.create_async_function(|_, (title, opts): (String, Option<mlua::Table>)| async move {
            let secret = opts.as_ref().and_then(|o| o.get::<Option<bool>>("secret").ok().flatten()).unwrap_or(false);
            if secret {
                // never echo a secret: keys come from stdin with --with-api-key
                return Ok(None);
            }
            if !std::io::stdin().is_terminal() {
                std::future::pending::<()>().await;
            }
            eprint!("{title}: ");
            let _ = std::io::stderr().flush();
            let line = tokio::task::spawn_blocking(|| {
                let mut s = String::new();
                std::io::stdin().read_line(&mut s).ok().filter(|n| *n > 0).map(|_| s.trim().to_string())
            })
            .await
            .ok()
            .flatten();
            Ok(line)
        })?,
    )?;
    t.set("code", 0)?;
    let emit_res =
        host.kernel.emit_checked("login/cli", mlua::MultiValue::from_vec(vec![mlua::Value::Table(t.clone())])).await;
    if let Ok(Some(hint)) = t.get::<Option<String>>("tty_hint") {
        eprintln!("{hint}");
    }
    let code: i32 = match emit_res {
        Ok(()) => t.get("code")?,
        Err(e) => {
            eprintln!("mare: {e}");
            let c: i32 = t.get("code")?;
            if c == 0 { 1 } else { c }
        }
    };
    host.shutdown().await;
    Ok(code)
}
