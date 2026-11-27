//! `mare.net.unix`: a small line-oriented Unix socket client (connect, write a line, read a line,
//! with finite defaults and an explicit unbounded read) plus `listen` for the server side. Generic on purpose — integrations such
//! as status panes or peer sessions build on it.
//! Errors come back as `nil, err` (never raised) so callers can ignore failures cheaply.

use std::sync::Arc;
use std::time::Duration;

use mlua::{Lua, MultiValue, Table, UserData, UserDataMethods, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::{Mutex, watch};

use crate::platform::{self, IpcBackend};

/// The local IPC stream of this platform (Unix domain socket; named pipe on Windows).
type Stream = <platform::Ipc as IpcBackend>::Stream;

const DEFAULT_TIMEOUT_MS: u64 = 5000;
/// longest line read_line accepts (protects against a peer that never sends '\n')
const MAX_LINE: usize = 1 << 20;

struct Reader {
    stream: BufReader<ReadHalf<Stream>>,
    partial: Vec<u8>,
    failed: bool,
}

struct Connection {
    reader: Mutex<Option<Reader>>,
    writer: Mutex<Option<WriteHalf<Stream>>>,
    closed: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct UnixConn(Arc<Connection>);

fn timeout_of(opts: &Option<Table>, fallback: u64) -> Duration {
    let ms = opts.as_ref().and_then(|t| t.get::<Option<f64>>("timeout_ms").ok().flatten());
    Duration::from_millis(ms.map(|m| m.max(0.0) as u64).unwrap_or(fallback))
}

fn fail(lua: &Lua, msg: impl Into<String>) -> mlua::Result<MultiValue> {
    Ok(MultiValue::from_vec(vec![Value::Nil, Value::String(lua.create_string(msg.into())?)]))
}

pub(crate) async fn connect(path: &str, d: Duration) -> Result<UnixConn, String> {
    let s = tokio::time::timeout(d, <platform::Ipc as IpcBackend>::connect(path))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(conn_of(s))
}

impl UnixConn {
    pub(crate) async fn write_line(&self, line: &str, d: Duration) -> Result<(), String> {
        let mut closed = self.0.closed.subscribe();
        let mut g = self.0.writer.lock().await;
        if *closed.borrow() {
            return Err("closed".into());
        }
        let w = g.as_mut().ok_or("closed")?;
        let mut buf = line.as_bytes().to_vec();
        if buf.last() != Some(&b'\n') {
            buf.push(b'\n');
        }
        let write = tokio::time::timeout(d, async {
            w.write_all(&buf).await?;
            w.flush().await
        });
        tokio::select! {
            biased;
            _ = closed.changed() => Err("closed".into()),
            result = write => result.map_err(|_| "timeout".to_string())?.map_err(|e| e.to_string()),
        }
    }

    async fn read_line(&self, d: Duration) -> Result<String, String> {
        self.read_line_with_timeout(Some(d)).await
    }

    pub(crate) async fn read_line_with_timeout(&self, deadline: Option<Duration>) -> Result<String, String> {
        let mut closed = self.0.closed.subscribe();
        if *closed.borrow() {
            return Err("closed".into());
        }
        let read = async {
            let mut guard = self.0.reader.lock().await;
            if *self.0.closed.borrow() {
                return Err("closed".into());
            }
            let reader = guard.as_mut().ok_or("closed")?;
            if reader.failed {
                return Err("closed".into());
            }
            loop {
                let chunk = reader.stream.fill_buf().await.map_err(|e| e.to_string())?;
                if chunk.is_empty() {
                    reader.failed = true;
                    return Err(if reader.partial.is_empty() { "eof" } else { "incomplete frame" }.into());
                }
                let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (chunk.len(), false),
                };
                if reader.partial.len() + take > MAX_LINE {
                    reader.failed = true;
                    return Err("line too long".into());
                }
                reader.partial.extend_from_slice(&chunk[..take]);
                reader.stream.consume(take);
                if done {
                    let mut frame = std::mem::take(&mut reader.partial);
                    frame.pop();
                    if frame.last() == Some(&b'\r') {
                        frame.pop();
                    }
                    return String::from_utf8(frame).map_err(|_| "invalid utf-8".into());
                }
            }
        };
        let expiration = async {
            match deadline {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            _ = closed.changed() => Err("closed".into()),
            result = read => result,
            _ = expiration => Err("timeout".into()),
        }
    }

    pub(crate) async fn close(&self) {
        self.0.closed.send_replace(true);
        self.0.reader.lock().await.take();
        self.0.writer.lock().await.take();
    }
}

impl UserData for UnixConn {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // conn:write_line(s [, {timeout_ms}]) -> true | nil, err
        m.add_async_method("write_line", |lua, this, (line, opts): (mlua::LuaString, Option<Table>)| async move {
            let d = timeout_of(&opts, DEFAULT_TIMEOUT_MS);
            match this.write_line(&line.to_str()?, d).await {
                Ok(()) => Ok(MultiValue::from_vec(vec![Value::Boolean(true)])),
                Err(e) => fail(&lua, e),
            }
        });
        // conn:read_line([{timeout_ms}]) -> line | nil, err ("timeout" | "eof" | io error)
        m.add_async_method("read_line", |lua, this, opts: Option<Table>| async move {
            let unbounded =
                opts.as_ref().is_some_and(|t| matches!(t.get::<Value>("timeout_ms"), Ok(Value::Boolean(false))));
            let deadline = if unbounded { None } else { Some(timeout_of(&opts, DEFAULT_TIMEOUT_MS)) };
            match this.read_line_with_timeout(deadline).await {
                Ok(s) => Ok(MultiValue::from_vec(vec![Value::String(lua.create_string(s)?)])),
                Err(e) => fail(&lua, e),
            }
        });
        m.add_async_method("close", |_, this, ()| async move {
            this.close().await;
            Ok(())
        });
    }
}

fn conn_of(s: Stream) -> UnixConn {
    let (r, w) = tokio::io::split(s);
    let (closed, _) = watch::channel(false);
    UnixConn(Arc::new(Connection {
        reader: Mutex::new(Some(Reader { stream: BufReader::new(r), partial: Vec::new(), failed: false })),
        writer: Mutex::new(Some(w)),
        closed,
    }))
}

/// A listening socket: accepted connections are handed to the Lua handler, each in its own
/// local task. Closing (or dropping) it stops accepting and removes the socket file.
pub struct UnixServer {
    path: std::path::PathBuf,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl UnixServer {
    fn close(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = std::fs::remove_file(&self.path);
            #[cfg(windows)]
            let _ = std::fs::remove_file(platform::pipe::lock_path(&self.path));
        }
    }
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        self.close();
    }
}

impl UserData for UnixServer {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("path", |_, this, ()| Ok(this.path.to_string_lossy().into_owned()));
        m.add_method_mut("close", |_, this, ()| {
            this.close();
            Ok(())
        });
    }
}

fn listen(lua: &Lua, path: &str, handler: mlua::Function, mode: u32) -> Result<UnixServer, String> {
    // connections handled later stay in the context the listener was created in
    let ectx = mare_shared::exec_ctx::current(lua);
    let lua = lua.clone();
    let p = std::path::PathBuf::from(path);
    // a socket file left by a crashed process is replaced only if nobody answers on it
    let l = <platform::Ipc as IpcBackend>::bind(&p, mode).map_err(|e| e.to_string())?;
    let task = mare_shared::txn_scope::spawn_local(async move {
        while let Ok(s) = <platform::Ipc as IpcBackend>::accept(&l).await {
            let (h, lua, ectx) = (handler.clone(), lua.clone(), ectx.clone());
            mare_shared::txn_scope::spawn_local(async move {
                if let Err(e) = mare_shared::exec_ctx::call_in::<()>(&lua, ectx, h, conn_of(s)).await {
                    tracing::warn!("mare.net.unix listener handler: {e}");
                }
            });
        }
    });
    Ok(UnixServer { path: p, task: Some(task) })
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let net: Table = match mare.get::<Value>("net")? {
        Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            mare.set("net", t.clone())?;
            t
        }
    };
    let unix = lua.create_table()?;
    // connect(path [, {timeout_ms}]) -> conn | nil, err
    unix.set(
        "connect",
        lua.create_async_function(|lua, (path, opts): (String, Option<Table>)| {
            let pre = (|| {
                let path = crate::exec_ctx::process_path(&lua, &path)?;
                crate::exec_ctx::check(&lua, "network", &format!("unix:{}", path.display()))?;
                Ok::<_, mlua::Error>(path)
            })();
            async move {
                let path = match pre {
                    Ok(p) => p,
                    Err(e) => return fail(&lua, e.to_string()),
                };
                match connect(&path.to_string_lossy(), timeout_of(&opts, DEFAULT_TIMEOUT_MS)).await {
                    Ok(c) => Ok(MultiValue::from_vec(vec![Value::UserData(lua.create_userdata(c)?)])),
                    Err(e) => fail(&lua, e),
                }
            }
        })?,
    )?;
    // request(path, line [, {timeout_ms}]) -> response line | nil, err
    // one connection, one line out, one line back; timeout_ms bounds the whole exchange
    unix.set(
        "request",
        lua.create_async_function(|lua, (path, line, opts): (String, mlua::LuaString, Option<Table>)| {
            let pre = (|| {
                let path = crate::exec_ctx::process_path(&lua, &path)?;
                crate::exec_ctx::check(&lua, "network", &format!("unix:{}", path.display()))?;
                Ok::<_, mlua::Error>(path)
            })();
            async move {
                let path = match pre {
                    Ok(p) => p,
                    Err(e) => return fail(&lua, e.to_string()),
                };
                let d = timeout_of(&opts, DEFAULT_TIMEOUT_MS);
                let line = line.to_str()?.to_string();
                let r = tokio::time::timeout(d, async {
                    let c = connect(&path.to_string_lossy(), d).await?;
                    c.write_line(&line, d).await?;
                    c.read_line(d).await
                })
                .await
                .unwrap_or_else(|_| Err("timeout".into()));
                match r {
                    Ok(s) => Ok(MultiValue::from_vec(vec![Value::String(lua.create_string(s)?)])),
                    Err(e) => fail(&lua, e),
                }
            }
        })?,
    )?;
    // listen(path, handler(conn) [, {mode = 0o600}]) -> server | nil, err
    // handler runs (async) once per accepted connection; server:close() stops and unlinks
    unix.set(
        "listen",
        lua.create_function(|lua, (path, handler, opts): (String, mlua::Function, Option<Table>)| {
            // Binding can create parents and replace an existing file. Authorize the exact
            // process-cwd path before the platform layer touches it.
            let path = match crate::exec_ctx::checked_process_path(lua, "write", &path) {
                Ok(p) => p,
                Err(e) => return fail(lua, e.to_string()),
            };
            if let Err(e) = crate::exec_ctx::check(lua, "network", &format!("unix:{}", path.display())) {
                return fail(lua, e.to_string());
            }
            let mode = opts.as_ref().and_then(|t| t.get::<Option<u32>>("mode").ok().flatten()).unwrap_or(0o600);
            match listen(lua, &path.to_string_lossy(), handler, mode) {
                Ok(s) => Ok(MultiValue::from_vec(vec![Value::UserData(lua.create_userdata(s)?)])),
                Err(e) => fail(lua, e),
            }
        })?,
    )?;
    net.set("unix", unix)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lua_unbounded_read_can_be_cancelled_and_resumed() {
        let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let lua = Lua::new();
        lua.globals().set("conn", conn_of(a)).unwrap();
        b.write_all(b"par").await.unwrap();
        let read = lua.load("return conn:read_line({timeout_ms=false})").eval_async::<String>();
        assert!(tokio::time::timeout(Duration::from_millis(5), read).await.is_err());
        b.write_all(b"tial\n").await.unwrap();
        let line: String = lua.load("return conn:read_line({timeout_ms=false})").eval_async().await.unwrap();
        assert_eq!(line, "partial");
        lua.load("conn:close(); conn:close()").exec_async().await.unwrap();
    }

    #[tokio::test]
    async fn partial_frame_survives_timeout_and_cancel() {
        let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let c = conn_of(a);
        b.write_all(b"par").await.unwrap();
        assert_eq!(c.read_line(Duration::from_millis(5)).await.unwrap_err(), "timeout");
        b.write_all(b"ti").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(5), c.read_line(Duration::from_secs(1))).await.is_err());
        b.write_all(b"al\nnext\n").await.unwrap();
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap(), "partial");
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap(), "next");
    }

    #[tokio::test]
    async fn read_does_not_block_write_or_close() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let c = conn_of(a);
        let pending = c.read_line_with_timeout(None);
        let operation = async {
            c.write_line("request", Duration::from_secs(1)).await.unwrap();
            let mut b = BufReader::new(b);
            let mut line = String::new();
            b.read_line(&mut line).await.unwrap();
            assert_eq!(line, "request\n");
            c.close().await;
        };
        let (read, ()) = tokio::join!(pending, operation);
        assert_eq!(read.unwrap_err(), "closed");
        c.close().await;
    }

    #[tokio::test]
    async fn invalid_utf8_is_not_replaced_and_overflow_ends_reads() {
        let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let c = conn_of(a);
        b.write_all(b"\xff\nvalid\n").await.unwrap();
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap_err(), "invalid utf-8");
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap(), "valid");
        let oversized = vec![b'x'; MAX_LINE + 1];
        let producer = async {
            b.write_all(&oversized).await.unwrap();
        };
        let consumer = c.read_line(Duration::from_secs(1));
        let ((), result) = tokio::join!(producer, consumer);
        assert_eq!(result.unwrap_err(), "line too long");
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap_err(), "closed");
    }

    #[tokio::test]
    async fn eof_does_not_accept_partial_frame() {
        let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let c = conn_of(a);
        b.write_all(b"unfinished").await.unwrap();
        drop(b);
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap_err(), "incomplete frame");
        assert_eq!(c.read_line(Duration::from_secs(1)).await.unwrap_err(), "closed");
    }

    #[tokio::test]
    async fn crlf_closed_conn_lua_errors_and_listener_drop_clean_up() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
                let lua = Lua::new();
                lua.globals().set("conn", conn_of(a)).unwrap();
                b.write_all(b"hello\r\n").await.unwrap();
                let line: String = lua.load("return conn:read_line()").eval_async().await.unwrap();
                assert_eq!(line, "hello");
                lua.load("conn:close()").exec_async().await.unwrap();
                let (r_ok, w_ok): (bool, bool) = lua
                    .load(
                        r#"
                        local r = conn:read_line()
                        local w = conn:write_line("x")
                        return r == nil, w == nil
                        "#,
                    )
                    .eval_async()
                    .await
                    .unwrap();
                assert!(r_ok && w_ok);

                let sock = std::env::temp_dir().join(format!("mare-net-unix-drop-{}.sock", std::process::id()));
                let _ = std::fs::remove_file(&sock);
                let handler = lua.create_function(|_, _: Value| Ok(())).unwrap();
                let listener = listen(&lua, &sock.to_string_lossy(), handler, 0o600).unwrap();
                assert!(sock.exists());
                drop(listener);
                assert!(!sock.exists());
            })
            .await;
    }
}
