//! Host-owned child transcript. The caller never supplies its identity or writer.
use std::{cell::RefCell, collections::HashSet, path::Path, rc::Rc};

use crate::Host;
use mare_shared::lua::{lua_to_json, rt_err};
use mlua::Value;
use serde_json::{Value as Json, json};

pub struct Transcript {
    pub id: String,
    path: std::path::PathBuf,
    file: Option<mare_store::SessionFile>,
    last: Option<String>,
    seen_ids: HashSet<String>,
    wrote_new_message: bool,
}

impl Transcript {
    pub fn create(children_dir: &str, parent: &str, cwd: &Path, spec: &Json) -> mlua::Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let origin = spec
            .get("_fork_origin")
            .map(|value| {
                let source = value
                    .get("source_session_id")
                    .and_then(Json::as_str)
                    .filter(|s| *s == parent)
                    .ok_or_else(|| rt_err("fork origin does not match the parent session"))?;
                let leaf = value
                    .get("source_leaf_entry_id")
                    .and_then(Json::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| rt_err("fork origin requires a source leaf"))?;
                Ok::<_, mlua::Error>((source.to_string(), leaf.to_string()))
            })
            .transpose()?;
        let run = spec.get("run_id").and_then(Json::as_str).unwrap_or(&id);
        if run.is_empty() || !run.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(rt_err("child run_id must be a single safe name"));
        }
        let seq = match spec.get("seq") {
            None | Some(Json::Null) => 1,
            Some(v) => {
                v.as_u64().filter(|n| *n <= 999999).ok_or_else(|| rt_err("child seq must be a nonnegative integer"))?
            }
        };
        let path = Path::new(children_dir).join("agents").join(run).join(format!("{seq:03}-{id}.jsonl"));
        let messages = spec.get("messages").and_then(Json::as_array);
        let mut turn_ends = Vec::new();
        if let Some(indices) = spec.get("_history_turn_ends") {
            if origin.is_none() {
                return Err(rt_err("completed history boundaries require a fork origin"));
            }
            let indices = indices.as_array().ok_or_else(|| rt_err("invalid completed history boundaries"))?;
            let count = messages.map_or(0, Vec::len);
            for value in indices {
                let index = value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0 && *n <= count)
                    .ok_or_else(|| rt_err("invalid completed history boundary"))?;
                if turn_ends.last().is_some_and(|last| *last >= index) {
                    return Err(rt_err("completed history boundaries must be ordered and unique"));
                }
                turn_ends.push(index);
            }
        }
        // The UUID makes paths independent of labels and repeated run/seq metadata.
        if path.exists() {
            return Err(rt_err("child transcript identity collision"));
        }
        let mut file = mare_store::SessionFile::create_new(&path).map_err(rt_err)?;
        let now = mare_binding::task::now_ms();
        let seed_ids: Vec<_> = messages.into_iter().flatten().map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let completed_ids: Vec<_> = turn_ends.iter().map(|index| seed_ids[index - 1].clone()).collect();
        let mut entries = Vec::with_capacity(3 + seed_ids.len());
        entries.push(json!({"type": mare_store::record::CoreRecordType::Session.as_str(), "version":mare_store::CURRENT_VERSION, "session_id":id,
            "cwd":cwd, "created_at_ms":now as i64, "parent_session_id":parent, "run_id":run, "seq":seq,
            "label":spec.get("label").or_else(|| spec.get("name")),
            "meta":{"phase":spec.get("phase"), "task":spec.get("task").or_else(|| spec.get("description"))}}));
        entries.push(json!({"type":"state", "state":"running", "created_at_ms":now as i64}));
        if let Some((source_session_id, source_leaf_entry_id)) = origin {
            let mut data = json!({"source_session_id":source_session_id, "source_leaf_entry_id":source_leaf_entry_id});
            if let Some(last) = seed_ids.last() {
                data["inherited_through_entry_id"] = last.clone().into();
            }
            entries
                .push(json!({"type":"custom", "kind":"session/fork-origin", "provenance":"session-host", "data":data}));
        }
        if let Some(model) = spec.get("_history_model").and_then(Json::as_str).filter(|model| !model.is_empty()) {
            let mut record = json!({"type":"model", "model":model, "at_entry_id":false});
            if let Some(effort) = spec.get("_history_effort").and_then(Json::as_str).filter(|effort| !effort.is_empty())
            {
                record["effort"] = effort.into();
            }
            entries.push(record);
        }
        let mut last = None;
        let mut seen_ids = HashSet::new();
        if let Some(messages) = messages {
            for (message, msg_id) in messages.iter().zip(seed_ids) {
                seen_ids.insert(msg_id.clone());
                entries.push(message_entry(&msg_id, &last, message.clone()));
                last = Some(msg_id);
            }
        }
        for (i, leaf) in completed_ids.iter().enumerate() {
            entries.push(json!({"type":"custom", "kind":"agent/run-end", "data":{
                "session_id":id, "run_id":format!("inherited-{i}"), "leaf_entry_id":leaf
            }}));
        }
        if let Err(err) = file.append_batch(&entries, true) {
            drop(file);
            return match mare_store::delete_session(&path) {
                Ok(()) => Err(rt_err(err)),
                Err(cleanup) => Err(rt_err(format!("{err}; incomplete child {} could not be removed: {cleanup}", id))),
            };
        }
        Ok(Self { id, path, file: Some(file), last, seen_ids, wrote_new_message: false })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&mut self, message: Json, external_id: Option<String>) -> anyhow::Result<String> {
        let id = match external_id {
            Some(id) => {
                if id.is_empty() {
                    anyhow::bail!("child transcript entry id must be non-empty");
                }
                if self.seen_ids.contains(&id) {
                    anyhow::bail!("duplicate child transcript entry id: {id}");
                }
                id
            }
            None => loop {
                let cand = uuid::Uuid::new_v4().to_string();
                if !self.seen_ids.contains(&cand) {
                    break cand;
                }
            },
        };
        self.file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("child transcript is closed"))?
            .append(&message_entry(&id, &self.last, message), true)?;
        self.seen_ids.insert(id.clone());
        self.last = Some(id.clone());
        self.wrote_new_message = true;
        Ok(id)
    }

    fn finish_run(&mut self) -> anyhow::Result<()> {
        if self.wrote_new_message
            && let Some(leaf) = &self.last
        {
            self.file.as_mut().ok_or_else(|| anyhow::anyhow!("child transcript is closed"))?.append(
                &json!({"type":"custom", "kind":"agent/run-end", "data":{
                    "session_id":self.id, "run_id":"child", "leaf_entry_id":leaf
                }}),
                true,
            )?;
        }
        Ok(())
    }

    pub fn close(&mut self, ok: bool) -> anyhow::Result<()> {
        self.finish_run()?;
        let mut file = self.file.take().ok_or_else(|| anyhow::anyhow!("child transcript is closed"))?;
        file.append(
            &json!({"type":"state", "state":"closed", "ok":ok,
                "created_at_ms":mare_binding::task::now_ms() as i64}),
            true,
        )
    }
}

fn message_entry(id: &str, parent: &Option<String>, message: Json) -> Json {
    let mut map = serde_json::Map::new();
    map.insert("type".into(), Json::String(mare_shared::MESSAGE_KEY.into()));
    map.insert("entry_id".into(), Json::String(id.into()));
    map.insert("parent_entry_id".into(), parent.clone().map(Json::String).unwrap_or(Json::Null));
    map.insert("created_at_ms".into(), Json::from(mare_binding::task::now_ms() as i64));
    map.insert(mare_shared::MESSAGE_KEY.into(), message);
    Json::Object(map)
}

/// Install the child transcript writer on the host's `session` service (`persist_message` SPI),
/// overriding the child VM's own ephemeral recorder so child messages always land in the host
/// transcript instead of listening on deleted bus events.
/// This transcript does not register a durable-start journal or provide tool recovery.
pub fn install(host: &Host, transcript: Rc<RefCell<Transcript>>) -> mlua::Result<()> {
    let lua = host.lua();
    let f = lua.create_function(move |lua, args: mlua::Variadic<Value>| {
        mare_shared::session_barrier::barrier(lua).check()?;
        let ev = match args.len() {
            0 => Value::Nil,
            1 => args[0].clone(),
            _ => args[args.len() - 1].clone(),
        };
        let saved = (|| -> anyhow::Result<String> {
            let Value::Table(ev) = ev else { anyhow::bail!("record is not a table") };
            let external_id = match ev.raw_get::<Value>("entry_id")? {
                Value::Nil => None,
                Value::String(s) => Some(s.to_str()?.to_owned()),
                _ => anyhow::bail!("child transcript entry id must be a non-empty string"),
            };
            let message = lua_to_json(lua, ev.get::<Value>(mare_shared::MESSAGE_KEY)?)?;
            let mut transcript =
                transcript.try_borrow_mut().map_err(|_| anyhow::anyhow!("transcript writer is busy"))?;
            transcript.append(message, external_id)
        })();
        match saved {
            Ok(id) => Ok(Value::String(lua.create_string(&id)?)),
            Err(e) => Err(rt_err(format!("child transcript write failed: {e:#}"))),
        }
    })?;
    let Some(session_table) = host.service_table(mare_shared::SESSION_SERVICE_NAME) else {
        return Err(rt_err("child transcript requires the session service"));
    };
    session_table.raw_set("persist_message", f)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "mare-child-transcript-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn forked_transcript_persists_the_snapshot_model_and_effort_before_messages() {
        let dir = TempDir::new();
        let spec = json!({
            "_fork_origin":{"source_session_id":"parent", "source_leaf_entry_id":"leaf"},
            "_history_model":"faux/echo", "_history_effort":"high",
            "messages":[{"role":"user", "content":"inherited"}]
        });
        let mut transcript = Transcript::create(dir.path().to_str().unwrap(), "parent", dir.path(), &spec).unwrap();
        let entries = transcript.file.as_mut().unwrap().read_snapshot(None).unwrap();
        let (model_index, model) = entries
            .iter()
            .enumerate()
            .find(|(_, entry)| entry["type"] == "model")
            .expect("forked child must persist the snapshot model before its first response");
        assert_eq!(model["model"], "faux/echo");
        assert_eq!(model["effort"], "high");
        assert_eq!(model["at_entry_id"], false);
        assert!(model_index < entries.iter().position(|entry| entry["type"] == "message").unwrap());
    }

    #[test]
    fn create_batches_header_state_and_forked_messages_with_parent_chain() {
        let dir = TempDir::new();
        let spec = json!({
            "run_id": "run-1",
            "seq": 2,
            "name": "worker",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"}
            ]
        });
        let mut transcript = Transcript::create(dir.path().to_str().unwrap(), "parent-1", dir.path(), &spec).unwrap();
        assert_eq!(
            transcript.file.as_ref().unwrap().durable_syncs(),
            1,
            "child transcript startup must batch header, state, and forked messages into one durable sync"
        );
        let path = dir.path().join("agents/run-1").join(format!("002-{}.jsonl", transcript.id));
        let entries = transcript.file.as_mut().unwrap().read_snapshot(None).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["type"], "session");
        assert_eq!(entries[1]["type"], "state");
        assert!(entries[1]["created_at_ms"].is_i64());
        assert_eq!(entries[2]["type"], "message");
        assert!(entries[2]["parent_entry_id"].is_null());
        assert!(entries[2]["created_at_ms"].is_i64());
        assert_eq!(entries[2]["message"]["content"], "one");
        assert_eq!(entries[3]["type"], "message");
        assert_eq!(entries[3]["parent_entry_id"], entries[2]["entry_id"]);
        assert_eq!(entries[3]["message"]["content"], "two");
        assert_eq!(transcript.last.as_deref(), entries[3]["entry_id"].as_str());

        let three_id = transcript.append(json!({"role": "user", "content": "three"}), Some("ext-3".into())).unwrap();
        assert_eq!(three_id, "ext-3");
        assert!(transcript.append(json!({"role": "user", "content": "empty"}), Some(String::new())).is_err());
        assert!(transcript.append(json!({"role": "user", "content": "dup"}), Some("ext-3".into())).is_err());
        transcript.close(true).unwrap();

        let reopened = mare_store::read_fork_snapshot(&path, Some(&transcript.id)).unwrap();
        assert_eq!(reopened.len(), 7);
        assert_eq!(reopened[4]["entry_id"], "ext-3");
        assert_eq!(reopened[4]["parent_entry_id"], entries[3]["entry_id"]);
        assert_eq!(reopened[5]["kind"], "agent/run-end");
        assert_eq!(reopened[5]["data"]["leaf_entry_id"], "ext-3");
        assert_eq!(reopened[6]["type"], "state");
        assert_eq!(reopened[6]["state"], "closed");
        assert!(reopened[6]["created_at_ms"].is_i64());
    }
}
