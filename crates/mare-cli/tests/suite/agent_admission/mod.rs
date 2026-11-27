//! Public whole-run idle admission: real service, faux provider, session generations, and RPC parity.
use crate::common::*;
use serde_json::json;

/// Where an Inbox reads its next message from; `deadline` bounds the whole wait.
pub(super) trait Source {
    fn next_msg(&mut self, deadline: std::time::Instant) -> serde_json::Value;
}

/// Lines from a child's stdout through a reader thread. A line that is not JSON fails the test
/// (as `RpcClient::read` does) instead of being skipped.
pub(super) struct TimedLines {
    rx: std::sync::mpsc::Receiver<Result<serde_json::Value, String>>,
}

impl TimedLines {
    pub(super) fn spawn(out: std::process::ChildStdout) -> Self {
        use std::io::BufRead;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let parsed = serde_json::from_str(&line).map_err(|e| format!("not JSON ({e}): {line}"));
                if tx.send(parsed).is_err() {
                    break;
                }
            }
        });
        Self { rx }
    }
}

impl Source for TimedLines {
    fn next_msg(&mut self, deadline: std::time::Instant) -> serde_json::Value {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => panic!("mare wrote a bad RPC line: {e}"),
            Err(e) => panic!("the awaited message never arrived before the deadline ({e})"),
        }
    }
}

/// A scripted stream: the awaited message must be in it, in some order.
impl Source for std::collections::VecDeque<serde_json::Value> {
    fn next_msg(&mut self, _deadline: std::time::Instant) -> serde_json::Value {
        self.pop_front().expect("the scripted stream ended before the awaited message")
    }
}

/// Messages read off an RPC connection and not yet asked for (replies by id, events by type).
/// Every wait has one deadline (`limit` from its start), however many other messages arrive.
pub(super) struct Inbox {
    kept: RpcMessageBuffer,
    limit: std::time::Duration,
}

impl Default for Inbox {
    fn default() -> Self {
        Self { kept: RpcMessageBuffer::default(), limit: std::time::Duration::from_secs(60) }
    }
}

impl Inbox {
    fn take_reply(&mut self, id: u64) -> Option<serde_json::Value> {
        self.kept.take_matching(|m| m.get("method").is_none() && m["id"] == id)
    }

    fn take_event(&mut self, ty: &str) -> Option<serde_json::Value> {
        self.kept.take_matching(|m| m["method"] == "event" && m["params"]["type"] == ty)
    }

    fn clear_events(&mut self) {
        self.kept.retain(|m| m["method"] != "event");
    }

    fn wait(
        &mut self,
        src: &mut impl Source,
        what: &str,
        mut take: impl FnMut(&mut Self) -> Option<serde_json::Value>,
    ) -> serde_json::Value {
        let deadline = std::time::Instant::now() + self.limit;
        loop {
            if let Some(m) = take(self) {
                return m;
            }
            assert!(std::time::Instant::now() < deadline, "{what} never arrived within {:?}", self.limit);
            self.kept.ensure_capacity(what);
            self.kept.push(src.next_msg(deadline), what);
        }
    }

    /// The reply to `id`, keeping everything else read meanwhile.
    pub(super) fn reply(&mut self, src: &mut impl Source, id: u64) -> serde_json::Value {
        self.wait(src, &format!("the reply to {id}"), |me| me.take_reply(id))
    }

    /// The next event of type `ty`, keeping everything else read meanwhile.
    pub(super) fn event(&mut self, src: &mut impl Source, ty: &str) -> serde_json::Value {
        self.wait(src, &format!("event {ty}"), |me| me.take_event(ty))
    }
}

/// An endless stream of unrelated events, like a turn that keeps streaming.
struct Chatter;

impl Source for Chatter {
    fn next_msg(&mut self, _deadline: std::time::Instant) -> serde_json::Value {
        json!({ "jsonrpc": "2.0", "method": "event", "params": { "type": "item/delta" } })
    }
}

/// A reply that never comes behind an endless stream of other messages fails at
/// the wait's deadline instead of re-arming forever.
#[test]
#[should_panic(expected = "never arrived")]
fn inbox_fails_when_the_reply_never_comes_behind_endless_chatter() {
    let mut inbox = Inbox { limit: std::time::Duration::from_millis(50), ..Inbox::default() };
    inbox.reply(&mut Chatter, 1);
}

/// Both orders the RPC compact test can see — `turn/completed` before or after the
/// abort reply, and the cancelled compact reply before or after the abort reply — are served
/// from a scripted stream. A reader that drops what it did not ask for runs the stream dry in the
/// "early" order.
#[test]
fn inbox_keeps_early_events_and_replies_in_either_order() {
    let end = json!({ "jsonrpc": "2.0", "method": "event", "params": { "type": "turn/completed" } });
    let abort = json!({ "jsonrpc": "2.0", "id": 7, "result": { "ok": true } });
    let compact = json!({ "jsonrpc": "2.0", "id": 5, "result": { "ok": false } });
    for order in [vec![end.clone(), abort.clone()], vec![abort.clone(), end.clone()]] {
        let mut src: std::collections::VecDeque<_> = order.into();
        let mut inbox = Inbox::default();
        assert_eq!(inbox.reply(&mut src, 7)["result"]["ok"], true);
        assert_eq!(inbox.event(&mut src, "turn/completed")["params"]["type"], "turn/completed");
    }
    for order in [vec![compact.clone(), abort.clone()], vec![abort.clone(), compact.clone()]] {
        let mut src: std::collections::VecDeque<_> = order.into();
        let mut inbox = Inbox::default();
        assert_eq!(inbox.reply(&mut src, 7)["result"]["ok"], true);
        assert_eq!(inbox.reply(&mut src, 5)["result"]["ok"], false);
    }
}

mod cleanup_and_ui_submit;
mod idle_and_compaction;
mod routed_steer;
mod rpc_cancel_and_retry;
