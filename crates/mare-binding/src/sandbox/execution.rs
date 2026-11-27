//! Process ownership and multiplexed execution. The parent deadline does not depend on Lua.
use super::{
    process::Settings,
    wire::{self, HostFrame, Reply, Response, Start},
};
use crate::platform::{self, ProcessBackend, Signal};
use futures_util::StreamExt;
use mare_shared::lua::{json_to_lua_lossless, strict_lua_to_json};
use mlua::{Lua, MultiValue, Value};
use std::{
    cell::Cell,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, ChildStdin},
    sync::mpsc,
};
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        crate::proc::kill_group(self.0.id(), Signal::Kill);
        let _ = self.0.start_kill();
    }
}
struct Reader(tokio::task::AbortHandle);
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}
pub(super) struct Session {
    worker: Worker,
    input: ChildStdin,
    frames: mpsc::Receiver<std::io::Result<Response>>,
    _reader: Reader,
    diagnostics: Arc<Mutex<String>>,
    diagnostic_limit: Arc<std::sync::atomic::AtomicUsize>,
    stderr_task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    _stderr_reader: Reader,
}
impl Session {
    pub fn spawn(executable: &std::path::Path) -> mlua::Result<Self> {
        let mut cmd = tokio::process::Command::new(executable);
        cmd.arg(super::WORKER_ARG)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        <platform::Process as ProcessBackend>::isolate(&mut cmd);
        let mut worker = Worker(platform::spawn(&mut cmd).map_err(mlua::Error::external)?);
        let input = worker.0.stdin.take().ok_or_else(|| mlua::Error::runtime("sandbox stdin unavailable"))?;
        let mut output =
            BufReader::new(worker.0.stdout.take().ok_or_else(|| mlua::Error::runtime("sandbox stdout unavailable"))?);
        let (tx, frames) = mpsc::channel(1);
        let reader = mare_shared::txn_scope::spawn_local(async move {
            let mut buf = wire::ReadBuf::new();
            loop {
                let frame = wire::read_with::<Response>(&mut output, 128 * 1024 * 1024, &mut buf).await;
                let ended = frame.is_err();
                if tx.send(frame).await.is_err() || ended {
                    break;
                }
            }
        });
        let mut stderr = worker.0.stderr.take().ok_or_else(|| mlua::Error::runtime("sandbox stderr unavailable"))?;
        let diagnostics = Arc::new(Mutex::new(String::new()));
        let diagnostic_limit = Arc::new(std::sync::atomic::AtomicUsize::new(1024 * 1024));
        let captured = diagnostics.clone();
        let limit = diagnostic_limit.clone();
        let stderr_task = mare_shared::txn_scope::spawn_local(async move {
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stderr.read(&mut buffer).await?;
                if count == 0 {
                    return Ok(());
                }
                let mut text = captured.lock().unwrap();
                text.push_str(&String::from_utf8_lossy(&buffer[..count]));
                let max = limit.load(std::sync::atomic::Ordering::Relaxed);
                if text.len() > max {
                    let mut from = text.len() - max;
                    while !text.is_char_boundary(from) {
                        from += 1;
                    }
                    text.drain(..from);
                }
            }
        });
        let stderr_abort = stderr_task.abort_handle();
        Ok(Self {
            worker,
            input,
            frames,
            _reader: Reader(reader.abort_handle()),
            diagnostics,
            diagnostic_limit,
            stderr_task: Some(stderr_task),
            _stderr_reader: Reader(stderr_abort),
        })
    }
    pub fn pid(&self) -> Option<u32> {
        self.worker.0.id()
    }
    pub fn diagnostic(&self) -> String {
        self.diagnostics.lock().unwrap().clone()
    }
    pub async fn close(&mut self, grace: std::time::Duration, drain: std::time::Duration) -> (bool, bool) {
        let pid = self.worker.0.id();
        let stopped = crate::proc::stop_tree(&mut self.worker.0, pid, grace).await.1;
        let drained = if let Some(mut task) = self.stderr_task.take() {
            match tokio::time::timeout(drain, &mut task).await {
                Ok(Ok(Ok(()))) => true,
                _ => {
                    task.abort();
                    false
                }
            }
        } else {
            true
        };
        (stopped, drained)
    }
    pub async fn run(
        &mut self,
        lua: &Lua,
        settings: &Settings,
        code: String,
        output: &Arc<Mutex<String>>,
        ledger: &Arc<Mutex<super::output::Ledger>>,
    ) -> (mlua::Result<Value>, bool) {
        self.diagnostic_limit.store(settings.limits.output_bytes, std::sync::atomic::Ordering::Relaxed);
        self.diagnostics.lock().unwrap().clear();
        let completed = Cell::new(false);
        let request = Start {
            version: 4,
            generation: settings.generation,
            persistent: settings.persistent,
            code,
            timeout_ms: settings.limits.deadline_enabled.then_some(settings.limits.timeout.as_millis() as u64),
            memory_bytes: settings.limits.memory,
            max_bytes: settings.limits.bytes,
            max_message_bytes: settings.limits.message_bytes,
            max_output_bytes: settings.limits.output_bytes,
            max_pending_calls: settings.limits.pending_calls,
            max_instructions: settings.limits.instructions,
            expose: settings.expose.clone(),
        };
        let max =
            settings.limits.output_bytes.max(settings.limits.message_bytes).saturating_mul(6).saturating_add(65536);
        let work = async {
            wire::write(&mut self.input, &HostFrame::Start { request }, 128 * 1024 * 1024)
                .await
                .map_err(mlua::Error::external)?;
            let mut pending = futures_util::stream::FuturesUnordered::new();
            let mut pending_bytes = 0_usize;
            loop {
                tokio::select! {biased;
                    frame=self.frames.recv()=>{
                        let response=frame.ok_or_else(||mlua::Error::runtime("sandbox worker output closed"))?.map_err(mlua::Error::external)?;
                        match response{
                            Response::Output{text}=>{
                                {if !ledger.lock().unwrap().admit(serde_json::Value::String(text.clone())){return Err(mlua::Error::runtime("sandbox output byte limit exceeded"));}let mut output=output.lock().unwrap();output.push_str(&text);}
                                if let Some(callback)=&settings.on_output{mare_shared::exec_ctx::call_in::<()>(lua,settings.context.clone(),callback.clone(),text).await?;}
                            }
                            Response::Done{value,error,terminated}=>{
                                let aborted = terminated
                                    || settings.owner_cancel.is_cancelled()
                                    || settings.limits.cancel.is_cancelled()
                                    || settings.limits.stopped.is_cancelled();
                                if !aborted && (value.is_some() || error.is_some()) {
                                    completed.set(true);
                                }
                                if settings.owner_cancel.is_cancelled() || settings.limits.cancel.is_cancelled() {
                                    return Err(mlua::Error::runtime("sandbox: cancelled"));
                                }
                                if settings.limits.stopped.is_cancelled() {
                                    return Err(mlua::Error::runtime(settings.limits.message()));
                                }
                                return match(error,value){(Some(error),_)=>Err(mlua::Error::runtime(error)),(_,Some(value))=>json_to_lua_lossless(lua,&value),_=>Err(mlua::Error::runtime("sandbox worker returned no result"))};
                            }
                            Response::Call{generation,call_id,id,args}=>{
                                settings.limits.check()?;
                                if generation!=settings.generation{
                                    wire::write(&mut self.input,&HostFrame::Reply{reply:Reply{call_id,complete:false,values:vec![],error:Some("capability belongs to an expired execution".into())}},max).await.map_err(mlua::Error::external)?;continue;
                                }
                                let bytes=super::output::json_size(&args).map_err(mlua::Error::external)?;
                                settings.limits.message_size(pending_bytes.saturating_add(bytes))?;pending_bytes=pending_bytes.saturating_add(bytes);
                                if pending.len()>=settings.limits.pending_calls{return Err(mlua::Error::runtime("sandbox pending callback limit exceeded"));}
                                let function=settings.callbacks.get(id).ok_or_else(||mlua::Error::runtime("sandbox invalid callback"))?.clone();
                                let lua=lua.clone();let context=settings.context.clone();let limits=settings.limits.clone();
                                pending.push(async move{
                                    let result=async{
                                        let args=args.iter().map(|v|json_to_lua_lossless(&lua,v)).collect::<mlua::Result<MultiValue>>()?;
                                        let values=mare_shared::exec_ctx::call_in::<MultiValue>(&lua,context,function,args).await?;
                                        if super::completion::requested(&values){return Ok((vec![],true));}
                                        let values=values.into_iter().map(|v|strict_lua_to_json(&lua,v)).collect::<mlua::Result<Vec<_>>>()?;
                                        limits.message_size(super::output::json_size(&values).map_err(mlua::Error::external)?)?;
                                        Ok::<_,mlua::Error>((values,false))
                                    }.await;
                                    (match result{Ok((values,complete))=>Reply{call_id,complete,values,error:None},Err(error)=>Reply{call_id,complete:false,values:vec![],error:Some(error.to_string())}},bytes)
                                });
                            }
                        }
                    }
                    Some((reply,bytes))=pending.next(),if !pending.is_empty()=>{pending_bytes=pending_bytes.saturating_sub(bytes);wire::write(&mut self.input,&HostFrame::Reply{reply},max).await.map_err(mlua::Error::external)?;}
                }
            }
        };
        let deadline = async {
            if settings.limits.deadline_enabled {
                tokio::time::sleep(settings.limits.timeout).await
            } else {
                std::future::pending::<()>().await
            }
        };
        let result = tokio::select! {biased;
            _=settings.owner_cancel.cancelled()=>Err(mlua::Error::runtime("sandbox: cancelled")),
            _=settings.limits.cancel.cancelled()=>Err(mlua::Error::runtime("sandbox: cancelled")),
            _=settings.limits.stopped.cancelled()=>Err(mlua::Error::runtime(settings.limits.message())),
            _=deadline=>Err(mlua::Error::runtime("sandbox: time limit exceeded")),
            result=work=>result,
        };
        let aborted = settings.owner_cancel.is_cancelled()
            || settings.limits.cancel.is_cancelled()
            || settings.limits.stopped.is_cancelled();
        (result, completed.get() && !aborted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{limits::Limits, output::Ledger, wire::Capability};

    #[tokio::test]
    async fn worker_timeout_done_frame_does_not_mark_completed() {
        let lua = Lua::new();
        let mut child = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let (tx, frames) = mpsc::channel(1);
        let reader = tokio::spawn(async { std::future::pending::<()>().await });
        let stderr_reader = tokio::spawn(async { std::future::pending::<()>().await });
        let mut session = Session {
            worker: Worker(child),
            input,
            frames,
            _reader: Reader(reader.abort_handle()),
            diagnostics: Arc::new(Mutex::new(String::new())),
            diagnostic_limit: Arc::new(std::sync::atomic::AtomicUsize::new(1024)),
            stderr_task: None,
            _stderr_reader: Reader(stderr_reader.abort_handle()),
        };
        tx.send(Ok(Response::Done {
            value: None,
            error: Some("sandbox: time limit exceeded".into()),
            terminated: true,
        }))
        .await
        .unwrap();
        let settings = Settings {
            limits: Limits::new(&None).unwrap(),
            owner_cancel: tokio_util::sync::CancellationToken::new(),
            expose: Capability::Table { entries: vec![] },
            callbacks: vec![],
            on_output: None,
            context: None,
            generation: 1,
            persistent: true,
        };
        let output = Arc::new(Mutex::new(String::new()));
        let ledger = Arc::new(Mutex::new(Ledger::new(1024)));
        let (result, completed) = session.run(&lua, &settings, "while true do end".into(), &output, &ledger).await;
        assert!(result.is_err(), "expected worker timeout error");
        assert!(!completed, "worker-reported timeout Done{{error}} must leave completed=false so close() runs");

        // Normal Lua script error without abort leaves persistent VM reusable.
        tx.send(Ok(Response::Done {
            value: None,
            error: Some("[string \"=sandbox\"]:1: expected".into()),
            terminated: false,
        }))
        .await
        .unwrap();
        let (script_res, completed_script_err) =
            session.run(&lua, &settings, "error('expected')".into(), &output, &ledger).await;
        assert!(script_res.is_err());
        assert!(completed_script_err, "recoverable Lua error should keep persistent worker alive");
        let _ = session.close(std::time::Duration::from_secs(2), std::time::Duration::from_millis(50)).await;
    }

    fn spawn_idle_child() -> tokio::process::Child {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/c", "findstr", ".*"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap()
    }

    #[tokio::test]
    async fn streamed_done_without_output_respects_ledger_budget_truncation_and_cancel() {
        let lua = Lua::new();
        let mut child = spawn_idle_child();
        let input = child.stdin.take().unwrap();
        let (tx, frames) = mpsc::channel(8);
        let reader = tokio::spawn(async { std::future::pending::<()>().await });
        let stderr_reader = tokio::spawn(async { std::future::pending::<()>().await });
        let mut session = Session {
            worker: Worker(child),
            input,
            frames,
            _reader: Reader(reader.abort_handle()),
            diagnostics: Arc::new(Mutex::new(String::new())),
            diagnostic_limit: Arc::new(std::sync::atomic::AtomicUsize::new(1024)),
            stderr_task: None,
            _stderr_reader: Reader(stderr_reader.abort_handle()),
        };
        let output = Arc::new(Mutex::new(String::new()));

        let make_settings = |output_bytes: usize, cancelled: bool| {
            let mut limits = Limits::new(&None).unwrap();
            limits.output_bytes = output_bytes;
            let owner_cancel = tokio_util::sync::CancellationToken::new();
            if cancelled {
                owner_cancel.cancel();
            }
            Settings {
                limits,
                owner_cancel,
                expose: Capability::Table { entries: vec![] },
                callbacks: vec![],
                on_output: None,
                context: None,
                generation: 1,
                persistent: true,
            }
        };

        // Streamed Output chunks + Done without duplicated `output` field feeds Ledger accurately.
        let small_settings = make_settings(32, false);
        let ledger2 = Arc::new(Mutex::new(Ledger::new(32)));
        tx.send(Ok(Response::Output { text: "hello\n".into() })).await.unwrap();
        tx.send(Ok(Response::Output { text: "world\n".into() })).await.unwrap();
        tx.send(Ok(Response::Done {
            value: Some(serde_json::json!({ "result": 42 })),
            error: None,
            terminated: false,
        }))
        .await
        .unwrap();
        let (done_res, done_completed) = session
            .run(&lua, &small_settings, "print('hello'); print('world'); return 42".into(), &output, &ledger2)
            .await;
        assert!(done_completed);
        let tbl = match done_res.unwrap() {
            Value::Table(t) => t,
            other => panic!("expected table, got {other:?}"),
        };
        assert_eq!(tbl.get::<i64>("result").unwrap(), 42);
        let val = strict_lua_to_json(&lua, tbl.get::<Value>("result").unwrap()).unwrap();
        let finished = ledger2.lock().unwrap().finish(Some(val), None);
        assert!(!finished.limited);
        let retained: String = finished.items.iter().filter_map(serde_json::Value::as_str).collect();
        assert_eq!(retained, "hello\nworld\n");

        // Exceeding ledger output budget aborts run and finish() produces bounded prefix.
        let ledger_over = Arc::new(Mutex::new(Ledger::new(45)));
        tx.send(Ok(Response::Output { text: "aaaa\n".into() })).await.unwrap();
        tx.send(Ok(Response::Output { text: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n".into() })).await.unwrap();
        let budget_settings = make_settings(45, false);
        let (over_res, over_completed) =
            session.run(&lua, &budget_settings, "print('over')".into(), &output, &ledger_over).await;
        assert!(over_res.is_err());
        assert!(!over_completed);
        let settled_over = ledger_over.lock().unwrap().finish(None, Some(over_res.unwrap_err().to_string()));
        assert!(settled_over.limited);
        assert!(settled_over.bytes <= 45);

        // Cancellation aborts session run cleanly even if Done arrives.
        let cancel_settings = make_settings(1024, true);
        let ledger_cancel = Arc::new(Mutex::new(Ledger::new(1024)));
        let (cancel_res, cancel_completed) =
            session.run(&lua, &cancel_settings, "return 1".into(), &output, &ledger_cancel).await;
        assert!(cancel_res.unwrap_err().to_string().contains("cancelled"));
        assert!(!cancel_completed);

        let _ = session.close(std::time::Duration::from_secs(2), std::time::Duration::from_millis(50)).await;
    }
}
