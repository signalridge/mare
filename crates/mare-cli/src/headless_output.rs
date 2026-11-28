//! One bounded, ordered stdout writer outside the current-thread Lua/Tokio runtime.
//! Uses a bounded I/O thread with completion acknowledgement and flush ordering,
//! owning a platform stdout duplicate to flush complete lines and report I/O errors without logging.
//! Copyright 2025 OpenAI (Apache-2.0).

use mare_shared::wire_projection::WireProjector;
use serde_json::Value;
use std::io::{self, Write};
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

type Record = (String, oneshot::Sender<io::Result<()>>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StdoutError {
    BrokenPipe,
    Io(String),
}

pub(crate) struct Output {
    sender: mpsc::Sender<Record>,
    projector: Option<WireProjector>,
    first_error: Mutex<Option<StdoutError>>,
}

pub(crate) enum Line {
    Json(Value),
    Text(String),
    /// Events are already projected on entry to the session log.
    Event(String),
}

impl Output {
    pub fn new(projector: Option<WireProjector>) -> io::Result<Self> {
        // Use an owned duplicate, not stdout().lock(): a blocked writer must not hold the
        // process-wide stdio lock during shutdown, and must not occupy Tokio's blocking pool.
        let file = crate::clone_protocol_stdout()?;
        Self::from_writer(file, projector)
    }

    pub(crate) fn from_writer<W: Write + Send + 'static>(
        mut file: W,
        projector: Option<WireProjector>,
    ) -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<Record>(1);
        std::thread::Builder::new().name("mare-headless-stdout".into()).spawn(move || {
            while let Some((line, done)) = receiver.blocking_recv() {
                let result =
                    file.write_all(line.as_bytes()).and_then(|_| file.write_all(b"\n")).and_then(|_| file.flush());
                let failed = result.is_err();
                let _ = done.send(result);
                if failed {
                    break;
                }
            }
        })?;
        Ok(Self { sender, projector, first_error: Mutex::new(None) })
    }

    pub fn record_error(&self, err: &io::Error) {
        let mut slot = self.first_error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(if err.kind() == io::ErrorKind::BrokenPipe {
                StdoutError::BrokenPipe
            } else {
                StdoutError::Io(err.to_string())
            });
        }
    }

    pub fn record_broken_pipe(&self) {
        let mut slot = self.first_error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(StdoutError::BrokenPipe);
        }
    }

    pub fn first_error(&self) -> Option<StdoutError> {
        self.first_error.lock().unwrap().clone()
    }

    pub fn has_error(&self) -> bool {
        self.first_error.lock().unwrap().is_some()
    }

    pub async fn line(&self, line: Line) -> io::Result<()> {
        let line = match line {
            Line::Json(mut value) => {
                if let Some(projector) = &self.projector {
                    projector.project_json(&mut value);
                }
                value.to_string()
            }
            Line::Text(text) => self.project_text(&text),
            Line::Event(text) => text,
        };
        let (done, acknowledged) = oneshot::channel();
        // Normally one record is outstanding at a time, but cancelling `run_inner` on
        // `--timeout` can leave a record queued in the channel while the timeout path
        // emits `turn.failed` / `settled`. Await channel space rather than treating `Full`
        // as `BrokenPipe`.
        self.sender
            .send((line, done))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "headless writer unavailable"))?;
        acknowledged.await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "headless writer stopped"))?
    }

    fn project_text(&self, text: &str) -> String {
        self.projector.as_ref().map_or_else(|| text.to_owned(), |p| p.project_str(text))
    }

    pub fn stderr(&self, text: &str) {
        let _ = self.write_stderr(text, io::stderr().lock());
    }

    fn write_stderr(&self, text: &str, mut writer: impl Write) -> io::Result<()> {
        writeln!(writer, "{}", self.project_text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, Output};
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex, mpsc};
    use tokio::sync::oneshot;

    #[test]
    fn stderr_projects_registered_values_at_the_writer_boundary() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        mare_binding::projection::install(&lua, &mare).unwrap();
        let secret = "HEADLESS_STDERR_SECRET_42";
        lua.globals().set("SECRET", secret).unwrap();
        let _lease: mlua::Value = lua.load("return mare.projection.register(SECRET)").eval().unwrap();
        let projector = mare_shared::wire_projection::hook(&lua).unwrap();
        let output = Output::from_writer(io::sink(), Some(projector)).unwrap();
        let mut bytes = Vec::new();
        output.write_stderr(&format!("failure: {secret}"), &mut bytes).unwrap();
        let written = String::from_utf8(bytes).unwrap();
        assert!(!written.contains(secret), "stderr leaked registered text: {written}");
        assert!(written.contains("[[mare-value:"));
        assert!(written.ends_with('\n'));
    }

    struct GatedWriter {
        lines: Arc<Mutex<Vec<u8>>>,
        entered: Option<oneshot::Sender<()>>,
        release: Option<mpsc::Receiver<()>>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(tx) = self.entered.take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.take() {
                let _ = rx.recv();
            }
            self.lines.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_projects_json_and_text_at_the_writer_boundary() {
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        mare_binding::projection::install(&lua, &mare).unwrap();
        let secret = "HEADLESS_SECRET_42\nquoted\"value";
        lua.globals().set("SECRET", secret).unwrap();
        let _lease: mlua::Value = lua.load("return mare.projection.register(SECRET)").eval().unwrap();
        let projector = mare_shared::wire_projection::hook(&lua).unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output =
            Output::from_writer(GatedWriter { lines: bytes.clone(), entered: None, release: None }, Some(projector))
                .unwrap();

        output.line(Line::Json(serde_json::json!({ "type": "settled", "data": { "error": secret } }))).await.unwrap();
        output.line(Line::Text(format!("failure: {secret}"))).await.unwrap();
        output.line(Line::Event("already projected event".into())).await.unwrap();

        let written = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(!written.contains(secret), "headless output leaked registered text: {written}");
        let mut lines = written.lines();
        let record: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(record["data"]["error"].as_str().unwrap().contains("[[mare-value:"));
        assert!(lines.next().unwrap().contains("[[mare-value:"));
        assert_eq!(lines.next(), Some("already projected event"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_in_flight_write_does_not_drop_terminal_records() {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let output = Output::from_writer(
            GatedWriter { lines: buf.clone(), entered: Some(entered_tx), release: Some(release_rx) },
            None,
        )
        .unwrap();

        // First write enters the writer thread and blocks at the deterministic gate.
        let first = tokio::spawn({
            let sender = output.sender.clone();
            async move {
                let (done, ack) = oneshot::channel();
                sender.send(("event-1".into(), done)).await.unwrap();
                ack.await.unwrap().unwrap();
            }
        });
        entered_rx.await.unwrap();

        // Second write fills the capacity-1 channel and is then cancelled (simulating --timeout).
        let cancelled =
            tokio::time::timeout(std::time::Duration::ZERO, output.line(Line::Event("event-2".into()))).await;
        assert!(cancelled.is_err());

        // Unblock the writer thread, then emit the terminal records.
        release_tx.send(()).unwrap();
        first.await.unwrap();
        output.line(Line::Event("turn.failed".into())).await.unwrap();
        output.line(Line::Event("settled".into())).await.unwrap();

        let written = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines, ["event-1", "event-2", "turn.failed", "settled"]);
    }

    #[test]
    fn first_stdout_error_distinguishes_broken_pipe_from_other_io_errors() {
        let output = Output::from_writer(io::sink(), None).unwrap();
        assert!(!output.has_error());
        output.record_error(&io::Error::new(io::ErrorKind::StorageFull, "No space left on device"));
        // Subsequent BrokenPipe from the stopped writer thread must not overwrite the first error.
        output.record_broken_pipe();
        assert!(matches!(output.first_error(), Some(super::StdoutError::Io(msg)) if msg.contains("No space left")));

        let output_pipe = Output::from_writer(io::sink(), None).unwrap();
        output_pipe.record_error(&io::Error::new(io::ErrorKind::BrokenPipe, "Broken pipe"));
        assert_eq!(output_pipe.first_error(), Some(super::StdoutError::BrokenPipe));
    }
}
