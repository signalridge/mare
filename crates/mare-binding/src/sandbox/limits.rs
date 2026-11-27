//! Execution limits shared by the VM and its host bridges.
use mlua::{Table, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Limits {
    pub timeout: Duration,
    pub deadline_enabled: bool,
    pub memory: usize,
    pub bytes: usize,
    pub message_bytes: usize,
    pub output_bytes: usize,
    pub pending_calls: usize,
    pub stop_grace: Duration,
    pub drain_grace: Duration,
    pub instructions: u64,
    pub cancel: CancellationToken,
    pub stopped: CancellationToken,
    pub reason: Arc<Mutex<Option<&'static str>>>,
}
impl Limits {
    pub fn new(opts: &Option<Table>) -> mlua::Result<Self> {
        fn number(opts: &Option<Table>, key: &str, default: u64, max: u64) -> mlua::Result<u64> {
            let v = opts.as_ref().map(|t| t.get::<Value>(key)).transpose()?.unwrap_or(Value::Nil);
            let n = match v {
                Value::Nil => default,
                Value::Integer(n) if n > 0 => n as u64,
                _ => return Err(mlua::Error::runtime(format!("sandbox: {key} must be a positive integer"))),
            };
            if n > max {
                return Err(mlua::Error::runtime(format!("sandbox: {key} exceeds maximum {max}")));
            }
            Ok(n)
        }
        let cancel = crate::task::opt_token(opts).unwrap_or_default();
        if let Some(o) = opts
            && !matches!(o.get::<Value>("cancel")?, Value::Nil)
            && crate::task::opt_token(opts).is_none()
        {
            return Err(mlua::Error::runtime("sandbox: cancel must be a cancellation token"));
        }
        let disabled = opts
            .as_ref()
            .map(|table| table.get::<Value>("timeout_ms"))
            .transpose()?
            .is_some_and(|value| matches!(value, Value::Boolean(false)));
        let bytes = number(opts, "max_bytes", 1024 * 1024, 16 * 1024 * 1024)?;
        let output_bytes = number(opts, "max_output_bytes", bytes, 16 * 1024 * 1024)? as usize;
        if output_bytes < 4 {
            return Err(mlua::Error::runtime("sandbox: max_output_bytes must be at least 4"));
        }
        Ok(Self {
            timeout: Duration::from_millis(if disabled { 60000 } else { number(opts, "timeout_ms", 60_000, 600_000)? }),
            deadline_enabled: !disabled,
            memory: number(opts, "memory_bytes", 32 * 1024 * 1024, 256 * 1024 * 1024)? as usize,
            bytes: bytes as usize,
            message_bytes: number(opts, "max_message_bytes", bytes, 16 * 1024 * 1024)? as usize,
            output_bytes,
            pending_calls: number(opts, "max_pending_calls", 128, 1024)? as usize,
            stop_grace: Duration::from_millis(number(opts, "stop_grace_ms", 3000, 600000)?),
            drain_grace: Duration::from_millis(number(opts, "grace_ms", 3000, 600000)?),
            instructions: number(opts, "max_instructions", 10_000_000, 100_000_000)?,
            cancel,
            stopped: CancellationToken::new(),
            reason: Arc::new(Mutex::new(None)),
        })
    }
    pub fn stop(&self, reason: &'static str) {
        self.reason.lock().unwrap().get_or_insert(reason);
        self.stopped.cancel();
    }
    pub fn check(&self) -> mlua::Result<()> {
        if self.cancel.is_cancelled() {
            self.stop("cancelled");
        }
        if self.stopped.is_cancelled() {
            return Err(mlua::Error::runtime(self.message()));
        }
        Ok(())
    }
    pub fn message(&self) -> String {
        format!("sandbox: {}", self.reason.lock().unwrap().unwrap_or("cancelled"))
    }
    fn bound(&self, n: usize, max: usize) -> mlua::Result<()> {
        self.check()?;
        if n > max {
            self.stop("byte limit exceeded");
            return Err(mlua::Error::runtime(self.message()));
        }
        Ok(())
    }
    pub fn size(&self, n: usize) -> mlua::Result<()> {
        self.bound(n, self.bytes)
    }
    pub fn message_size(&self, n: usize) -> mlua::Result<()> {
        self.bound(n, self.message_bytes)
    }
    pub fn output_size(&self, n: usize) -> mlua::Result<()> {
        self.bound(n, self.output_bytes)
    }
}
