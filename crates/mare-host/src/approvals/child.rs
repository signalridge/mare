//! Sub-agent events in the parent: child lifecycle events, and a child's approvals answered by
//! the parent's dialog one at a time. The dialog itself is the `agent-approvals` plugin's
//! (`agent/approval-dialog`); the queue, the fail-closed answers, the rule snapshot and the
//! report stay here.

use std::sync::Arc;

use mlua::{MultiValue, Value};

use crate::Host;
use crate::approvals::{approval_queue, child_events, proposed_rule, shared_rule};

const PROGRESS_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApprovalSource {
    Rule,
    User,
    Policy,
    None,
    Error,
    Reviewer,
    Cancelled,
    ApprovalPolicy,
}

impl ApprovalSource {
    fn from_wire(raw: Option<&str>) -> Self {
        match raw {
            Some("rule") => Self::Rule,
            Some("user") => Self::User,
            Some("policy") => Self::Policy,
            Some("none") => Self::None,
            Some("error") => Self::Error,
            Some("reviewer") => Self::Reviewer,
            Some("cancelled") => Self::Cancelled,
            _ => Self::ApprovalPolicy,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::User => "user",
            Self::Policy => "policy",
            Self::None => "none",
            Self::Error => "error",
            Self::Reviewer => "reviewer",
            Self::Cancelled => "cancelled",
            Self::ApprovalPolicy => "approval_policy",
        }
    }
}

impl Host {
    /// Child-agent lifecycle (`mare.subagent.start`) → `subagent/started` / `subagent/progress` /
    /// `subagent/completed` on this host's kernel.
    pub(crate) fn forward_child_events(self: &Arc<Self>) {
        let rx = self
            .lua()
            .app_data_ref::<child_events::ChildEventsRx>()
            .and_then(|r| r.0.lock().ok().and_then(|mut g| g.take()));
        let Some(mut rx) = rx else { return };
        // child approvals: answered one at a time, in arrival order (queued, never dropped)
        // (the waiting ones are announced as `agent/approval-queue`, see approval_queue.rs)
        let (atx, mut arx) = tokio::sync::mpsc::unbounded_channel::<(u64, child_events::ChildApproval)>();
        let queue = std::rc::Rc::new(approval_queue::ApprovalQueue::default());
        let weak = Arc::downgrade(self);
        let q = queue.clone();
        tokio::task::spawn_local(async move {
            while let Some((ticket, a)) = arx.recv().await {
                q.remove(ticket);
                match weak.upgrade() {
                    Some(h) => {
                        h.kernel.emit_json("agent/approval-queue", q.event()).await;
                        h.answer_child_approval(ticket, a).await
                    }
                    None => {
                        let _ = a.reply.send(child_events::ChildApprovalReply {
                            vote: child_unavailable("the parent session ended"),
                            human_approved: false,
                        });
                    }
                }
            }
        });
        let weak = Arc::downgrade(self);
        tokio::task::spawn_local(async move {
            let mut deferred = None;
            loop {
                let ev = match deferred.take() {
                    Some(ev) => ev,
                    None => match rx.recv().await {
                        Some(ev) => ev,
                        None => break,
                    },
                };
                let Some(h) = weak.upgrade() else { break };
                match ev {
                    child_events::ChildEvent::Progress { subagent_id, parent_session, progress } => {
                        // Only progress snapshots are replaceable. Flush before any lifecycle,
                        // approval, rule or barrier event so their ordering and delivery remain intact.
                        let mut latest = std::collections::BTreeMap::new();
                        latest.insert(subagent_id, (parent_session, progress));
                        let deadline = tokio::time::Instant::now() + PROGRESS_FLUSH_INTERVAL;
                        loop {
                            if tokio::time::Instant::now() >= deadline {
                                break;
                            }
                            match tokio::time::timeout_at(deadline, rx.recv()).await {
                                Ok(Some(child_events::ChildEvent::Progress {
                                    subagent_id,
                                    parent_session,
                                    progress,
                                })) => {
                                    latest.insert(subagent_id, (parent_session, progress));
                                }
                                Ok(Some(other)) => {
                                    deferred = Some(other);
                                    break;
                                }
                                Ok(None) | Err(_) => break,
                            }
                        }
                        for (subagent_id, (parent_session, progress)) in latest {
                            let (session, session_epoch) = parent_session.unzip();
                            h.kernel
                                .emit_json(
                                    "subagent/progress",
                                    serde_json::json!({
                                        "subagent_id": subagent_id,
                                        "parent_session_id": session,
                                        "parent_session_epoch": session_epoch,
                                        "progress": progress,
                                    }),
                                )
                                .await;
                        }
                    }
                    child_events::ChildEvent::Barrier(reply) => {
                        let _ = reply.send(());
                    }
                    child_events::ChildEvent::Start {
                        subagent_id,
                        agent_id,
                        name,
                        label,
                        run_id,
                        seq,
                        task,
                        parent_session,
                    } => {
                        let (session, session_epoch) = parent_session.unzip();
                        h.kernel
                            .emit_json(
                                "subagent/started",
                                serde_json::json!({
                                    "subagent_id": subagent_id,
                                    "agent_id": agent_id,
                                    "name": name,
                                    "label": label,
                                    "run_id": run_id,
                                    "seq": seq,
                                    "task": task,
                                    "parent_session_id": session,
                                    "parent_session_epoch": session_epoch,
                                }),
                            )
                            .await
                    }
                    child_events::ChildEvent::End(end) => {
                        let child_events::ChildEnd {
                            subagent_id,
                            agent_id,
                            name,
                            label,
                            ok,
                            status,
                            run_id,
                            seq,
                            parent_session,
                            text,
                            error,
                            error_kind,
                            usage,
                        } = *end;
                        let (session, session_epoch) = parent_session.unzip();
                        let status = if status.is_empty() {
                            if ok { "completed".to_string() } else { "failed".to_string() }
                        } else {
                            status
                        };
                        let mut completed = serde_json::json!({
                            "subagent_id": subagent_id,
                            "agent_id": agent_id,
                            "name": name,
                            "label": label,
                            "run_id": run_id,
                            "seq": seq,
                            "parent_session_id": session,
                            "parent_session_epoch": session_epoch,
                            "text": text,
                            "error": error,
                            "error_kind": error_kind,
                        });
                        if let Some(obj) = completed.as_object_mut() {
                            obj.insert(mare_shared::STATUS_KEY.into(), serde_json::json!(status));
                            obj.insert(mare_shared::USAGE_KEY.into(), serde_json::json!(usage));
                        }
                        h.kernel.emit_json("subagent/completed", completed).await
                    }
                    child_events::ChildEvent::RuleCheck(r) => {
                        tokio::task::spawn_local(async move {
                            let reply = match shared_rule::evaluate(&h, &r.call, &r.info).await {
                                Ok(v) => v.unwrap_or_default(),
                                Err(e) => serde_json::json!({
                                    "decision": "deny",
                                    "unavailable": true,
                                    "reason": format!("parent permission rule check failed: {e}"),
                                }),
                            };
                            let _ = r.reply.send(reply);
                        });
                    }
                    child_events::ChildEvent::Approval(a) => {
                        // announced before it can be dequeued: the answering side's removal is
                        // always the later event
                        let ticket = queue.push(&a.origin);
                        h.kernel.emit_json("agent/approval-queue", queue.event()).await;
                        if let Err(e) = atx.send((ticket, *a)) {
                            queue.remove(ticket);
                            let _ = e.0.1.reply.send(child_events::ChildApprovalReply {
                                vote: child_unavailable("the parent session ended"),
                                human_approved: false,
                            });
                        }
                    }
                }
            }
        });
    }

    /// A sub-agent's approval request, answered with the parent's own rules:
    /// its `tool/approval` listeners (RPC host, plugins) vote; without any, the interactive UI
    /// asks the user (the dialog names the sub-agent and its task); otherwise (headless) it is
    /// `unavailable`, i.e. denied. The call and info carry `origin`
    /// (`{agent_id, id, label?, task?, run_id?, phase?}`); `agent/child-approval` announces the
    /// request and the parent's `approval/requested` + `approval/resolved` report the lifecycle
    /// and outcome (with `child_approval_id`), so `/permission log`, status integrations and
    /// headless exit codes see it like the parent's own approvals:
    /// the root approval runtime answers a subagent's requests, tagged
    /// with its source.
    async fn answer_child_approval(self: &Arc<Self>, ticket: u64, a: child_events::ChildApproval) {
        let child_events::ChildApproval {
            origin,
            mut call,
            mut info,
            verdict,
            shares_rules,
            inherits_parent_preset,
            review_required,
            mut reply,
        } = a;
        if reply.is_closed() {
            return; // the child stopped (cancelled) while queued
        }
        for v in [&mut call, &mut info] {
            if !v.is_object() {
                *v = serde_json::json!({});
            }
            v["origin"] = origin.clone();
        }
        let args = call.get("arguments").cloned().unwrap_or_default();
        let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let asked = serde_json::json!({
            "origin": origin,
            "request_id": ticket,
            "tool": call.get("name"),
            "command": s(&args, "command"),
            "path": s(&args, "path"),
            "reason": verdict.get("reason"),
        });
        let asked = match mare_shared::lua::json_to_lua(self.lua(), &asked) {
            Ok(value) => value,
            Err(_) => {
                let _ = reply.send(child_events::ChildApprovalReply {
                    vote: child_unavailable("approval details could not be published"),
                    human_approved: false,
                });
                return;
            }
        };
        self.kernel.emit("agent/child-approval", MultiValue::from_vec(vec![asked])).await;
        let approval_id = mare_binding::codec::random_hex(16);
        let req_opts = self.lua().create_table().ok();
        if let Some(opts) = &req_opts {
            let _ = opts.set("approval_id", approval_id.as_str());
            let _ = opts.set("child_approval_id", ticket);
            let _ = opts.set("emit_resolved", false);
        }
        // a rule saved since the child asked (or one only the parent has) decides it like a fresh
        // call; otherwise the parent's approval path answers (see shared_rule.rs)
        let decision = async {
            if self.service_table(mare_shared::APPROVALS_SERVICE_NAME).is_some() {
                return self
                    .review_child_approval(
                        shares_rules,
                        inherits_parent_preset,
                        review_required,
                        &call,
                        &info,
                        &verdict,
                        req_opts.as_ref(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        (serde_json::json!({"decision":"deny","reason":e.to_string()}), "error", false)
                    });
            }
            (child_unavailable("approval service unavailable"), "none", false)
        };
        let (mut vote, mut source, mut human_approved) = tokio::select! {
            biased;
            _ = reply.closed() => (serde_json::json!({ "decision": "deny", "reason": "sub-agent cancelled" }), ApprovalSource::Cancelled.as_str(), false),
            decision = decision => if reply.is_closed() {
                (serde_json::json!({ "decision": "deny", "reason": "sub-agent cancelled" }), ApprovalSource::Cancelled.as_str(), false)
            } else { decision },
        };
        // Rules and mandatory policy listeners may have changed while waiting. A newly observed
        // deny or failed check still wins, and cancellation during recheck prevents persistence.
        if vote.get("decision").and_then(|d| d.as_str()) == Some("allow") {
            for attempt in 0..4 {
                let check_epoch = self.kernel.policy_epoch();
                let recheck = tokio::select! {
                    biased;
                    _ = reply.closed() => Err("cancelled".to_string()),
                    res = shared_rule::resolve_request(self, shares_rules, &call, &info) => Ok(res),
                };
                match recheck {
                    Err(_) => {
                        vote = serde_json::json!({ "decision": "deny", "reason": "sub-agent cancelled" });
                        source = "cancelled";
                        human_approved = false;
                        break;
                    }
                    Ok(Err(e)) => {
                        vote = child_unavailable(&format!("parent permission rule check failed: {e}"));
                        source = "error";
                        human_approved = false;
                        break;
                    }
                    Ok(Ok(Some(deny))) if deny.get("decision").and_then(|d| d.as_str()) == Some("deny") => {
                        vote = deny;
                        source = "rule";
                        human_approved = false;
                        break;
                    }
                    Ok(Ok(_)) => {
                        let post_policy = tokio::select! {
                            biased;
                            _ = reply.closed() => Err("cancelled".to_string()),
                            res = self.child_policy_vote("tool/approval", &call, &info, &verdict) => Ok(res),
                        };
                        match post_policy {
                            Err(_) => {
                                vote = serde_json::json!({ "decision": "deny", "reason": "sub-agent cancelled" });
                                source = "cancelled";
                                human_approved = false;
                                break;
                            }
                            Ok(Err(e)) => {
                                vote = child_unavailable(&format!("parent policy recheck failed: {e}"));
                                source = "error";
                                human_approved = false;
                                break;
                            }
                            Ok(Ok(Some(post))) if post.get("decision").and_then(|d| d.as_str()) == Some("deny") => {
                                vote = post;
                                source = "policy";
                                human_approved = false;
                                break;
                            }
                            Ok(Ok(maybe_post)) => {
                                if self.kernel.policy_epoch() != check_epoch {
                                    if attempt == 3 {
                                        vote = child_unavailable("parent policy changed during approval recheck");
                                        source = "error";
                                        human_approved = false;
                                        break;
                                    }
                                    continue;
                                }
                                if let Some(post) = maybe_post {
                                    for field in ["sandbox_constraints", "permission_constraints"] {
                                        if let Some(items) =
                                            post.get(field).and_then(|v| v.as_array()).filter(|a| !a.is_empty())
                                        {
                                            let entry = vote.as_object_mut().and_then(|o| {
                                                o.entry(field).or_insert_with(|| serde_json::json!([])).as_array_mut()
                                            });
                                            if let Some(arr) = entry {
                                                arr.extend(items.iter().cloned());
                                            }
                                        }
                                    }
                                    if post.get("scope").and_then(|v| v.as_str()) == Some("turn") {
                                        vote["scope"] = "turn".into();
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        if reply.is_closed() {
            vote = serde_json::json!({ "decision": "deny", "reason": "sub-agent cancelled" });
            source = "cancelled";
            human_approved = false;
        }
        if call["protected_arguments"] == true && vote["decision"] == "allow" {
            vote["grant"] = "allow".into();
            if let Some(object) = vote.as_object_mut() {
                object.remove("amendment");
            }
            if vote["scope"] == mare_shared::SESSION_SCOPE {
                vote["scope"] = "turn".into();
            }
        }
        // "remember": the rule the dialog showed, handed to the persisting side (proposed_rule.rs)
        let rule_token = if source == "cancelled" {
            None
        } else {
            vote.get("amendment").and_then(|r| proposed_rule::approve(self, r, &call, &info))
        };
        let decision = match (s(&vote, "decision").as_deref(), vote.get("unavailable"), vote.get("abort")) {
            (Some("allow"), _, _) => s(&vote, "grant").unwrap_or_else(|| "allow".into()),
            (_, Some(serde_json::Value::Bool(true)), _) => "unavailable".into(),
            (_, _, Some(serde_json::Value::Bool(true))) => "abort".into(),
            _ => "deny".into(),
        };
        let mut result_event = serde_json::json!({
            "call": call,
            "info": info,
            "child_approval_id": ticket,
            "decision": decision,
            "amendment": vote.get("amendment"),
            "permission_constraints": vote.get("permission_constraints"),
            "scope": vote.get("scope"),
            "rule_token": rule_token,
            "source": source,
            "reason": vote.get("reason"),
            "origin": origin,
        });
        // The public requested event can yield; recheck cancellation and the frozen access
        // afterward, before publishing the one terminal result or a reusable rule.
        let requested_emitted =
            req_opts.as_ref().and_then(|t| t.get::<Option<bool>>("requested").ok().flatten()).unwrap_or(false);
        let call_id = call.get("id").and_then(|v| v.as_str()).map(str::to_string);
        if !requested_emitted {
            self.emit_approval_requested(
                &approval_id,
                "tool",
                call_id.as_deref(),
                call.get("snapshot").cloned(),
                Some(ticket),
            )
            .await;
        }
        let invalid_access = if vote.get("decision").and_then(|v| v.as_str()) == Some("allow") {
            if reply.is_closed() {
                Some("sub-agent cancelled".to_owned())
            } else {
                shared_rule::validate_access(self, &call, &info).err()
            }
        } else {
            None
        };
        if let Some(err) = invalid_access {
            if let Some(token) = rule_token {
                proposed_rule::revoke(self.lua(), token);
            }
            let cancelled = reply.is_closed();
            vote = serde_json::json!({ "decision": "deny", "reason": err });
            source = if cancelled { "cancelled" } else { "policy" };
            human_approved = false;
            result_event["decision"] = "deny".into();
            result_event["source"] = source.into();
            result_event["reason"] =
                if cancelled { "sub-agent cancelled" } else { "child access changed before approval" }.into();
            result_event["amendment"] = serde_json::Value::Null;
            result_event["rule_token"] = serde_json::Value::Null;
        }
        let local_result = match mare_shared::lua::json_to_lua(self.lua(), &result_event) {
            Ok(value) => value,
            Err(_) => {
                let _ = reply.send(child_events::ChildApprovalReply {
                    vote: child_unavailable("approval audit could not be published"),
                    human_approved: false,
                });
                return;
            }
        };
        self.kernel.emit("pipeline/approval-decided", MultiValue::from_vec(vec![local_result])).await;
        // A rule no listener took is withdrawn; a taken one was persisted as approved.
        if let Some(token) = rule_token {
            proposed_rule::revoke(self.lua(), token);
        }
        let mut public_input = result_event;
        public_input["call"] = serde_json::json!({
            "tool_call_id": call_id.as_deref().unwrap_or(""),
            "name": call.get("name").and_then(|v| v.as_str()).unwrap_or("tool"),
        });
        self.emit_approval_resolved(&approval_id, "tool", call_id.as_deref(), &public_input).await;
        vote["source"] = source.into();
        let _ = reply.send(child_events::ChildApprovalReply { vote, human_approved });
    }

    #[allow(clippy::too_many_arguments)]
    async fn review_child_approval(
        self: &Arc<Self>,
        shares_rules: bool,
        inherits_parent_preset: bool,
        child_review_required: bool,
        call: &serde_json::Value,
        info: &serde_json::Value,
        verdict: &serde_json::Value,
        req_opts: Option<&mlua::Table>,
    ) -> mlua::Result<(serde_json::Value, &'static str, bool)> {
        let lua = self.lua();
        let service = self
            .service_table(mare_shared::APPROVALS_SERVICE_NAME)
            .ok_or_else(|| mlua::Error::runtime("approval service disappeared"))?;
        let parent_review_required = match service.get::<mlua::Function>("review_required") {
            Ok(f) => f.call_async::<bool>(service.clone()).await.unwrap_or(true),
            Err(_) => true,
        };
        let effective_review_required = child_review_required || parent_review_required;
        let policy = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (policy_host, captured_policy) = (self.clone(), policy.clone());
        let (policy_call, policy_info, policy_verdict) = (call.clone(), info.clone(), verdict.clone());
        let check_policy = lua.create_async_function(move |lua, ()| {
            let (host, captured) = (policy_host.clone(), captured_policy.clone());
            let (call, info, verdict) = (policy_call.clone(), policy_info.clone(), policy_verdict.clone());
            mare_kernel::as_host(async move {
                let raw = host
                    .child_policy_vote("tool/approval", &call, &info, &verdict)
                    .await?
                    .unwrap_or_else(|| serde_json::json!({"decision":"ask"}));
                *captured.borrow_mut() = Some(raw.clone());
                let mut restrictions = raw;
                if let Some(constraints) = restrictions.as_object_mut().and_then(|o| o.remove("sandbox_constraints")) {
                    restrictions["sandboxes"] = constraints;
                }
                mare_shared::lua::json_to_lua(&lua, &restrictions)
            })
        })?;
        let host = self.clone();
        let answer_policy = policy.clone();
        let (c, i, v) = (call.clone(), info.clone(), verdict.clone());
        let answer = lua.create_async_function(move |lua, binding: mlua::Table| {
            let host = host.clone();
            let (mut c, i, v) = (c.clone(), i.clone(), v.clone());
            let answer_policy = answer_policy.borrow().clone();
            mare_kernel::as_host(async move {
                c["approval_binding"] = mare_shared::lua::lua_to_json(&lua, Value::Table(binding))?;
                // Shared rules answer before the policy's allow can: a shared deny or a
                // failed rule check is the answer, so the approval audit never records an allow
                // that the post-answer re-check then has to overturn. The re-check stays.
                let rule_vote = if c["protected_arguments"] == true {
                    Ok(None)
                } else {
                    shared_rule::resolve_request(&host, shares_rules, &c, &i).await
                };
                let maybe_rule_allow = match rule_vote {
                    Ok(Some(mut vote)) if vote["decision"] != "allow" => {
                        vote["source"] = "rule".into();
                        return mare_shared::lua::json_to_lua(&lua, &vote);
                    }
                    Ok(maybe_rule_allow) => maybe_rule_allow,
                    Err(e) => {
                        let mut vote = child_unavailable(&format!("parent permission rule check failed: {e}"));
                        vote["source"] = "error".into();
                        return mare_shared::lua::json_to_lua(&lua, &vote);
                    }
                };
                if !effective_review_required
                    && c["protected_arguments"] != true
                    && inherits_parent_preset
                    && maybe_rule_allow.is_none()
                {
                    match shared_rule::resolve_parent_preset(&host, &c, &i).await {
                        Ok(Some(mut vote)) => {
                            vote["source"] = "policy".into();
                            return mare_shared::lua::json_to_lua(&lua, &vote);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let mut vote = child_unavailable(&format!("parent permission preset check failed: {e}"));
                            vote["source"] = "error".into();
                            return mare_shared::lua::json_to_lua(&lua, &vote);
                        }
                    }
                }
                let has_host = host.kernel.voter_count("tool/approval-host") > 0;
                if !has_host
                    && !effective_review_required
                    && c["protected_arguments"] != true
                    && let Some(mut vote) = answer_policy.filter(|v| v["decision"] == "allow")
                {
                    vote["source"] = "policy".into();
                    if let Some(object) = vote.as_object_mut() {
                        object.remove("permission_constraints");
                        object.remove("sandbox_constraints");
                    }
                    return mare_shared::lua::json_to_lua(&lua, &vote);
                }
                let (mut vote, source) =
                    host.decide_child_approval(&c, &i, &v, maybe_rule_allow, effective_review_required).await?;
                // The shared gate preserves the answerer's provenance unless it replaces
                // that answer with a policy, cancellation or reviewer decision.
                vote["source"] = source.into();
                mare_shared::lua::json_to_lua(&lua, &vote)
            })
        })?;
        let history = if self.kernel.service(crate::frontend::AGENT_SERVICE).is_some() {
            let value = self
                .call_service(crate::frontend::AGENT_SERVICE, "messages", MultiValue::new())
                .await?
                .into_iter()
                .next()
                .unwrap_or(Value::Nil);
            // Strict and bounded while walking the Lua value: an oversized history or a
            // non-JSON field fails this review (deny) instead of allocating or dropping.
            mare_shared::lua::strict_lua_to_json_bounded(lua, value, mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES)?
        } else {
            serde_json::Value::Null
        };
        let mut parent_verdict = verdict.clone();
        if let Some(object) = parent_verdict.as_object_mut() {
            match proposed_rule::snapshot(verdict) {
                Some(mut rule) => {
                    if let Some(map) = rule.as_object_mut() {
                        let desc = mare_binding::approval::describe_rule(&serde_json::Value::Object(map.clone()));
                        map.insert("description".into(), serde_json::Value::String(desc));
                    }
                    object.insert("amendment".into(), rule);
                }
                None => {
                    object.remove("amendment");
                }
            }
        }
        let action = lua.create_table()?;
        action.set("call", mare_shared::lua::json_to_lua_lossless(lua, call)?)?;
        action.set("info", mare_shared::lua::json_to_lua_lossless(lua, info)?)?;
        action.set("verdict", mare_shared::lua::json_to_lua_lossless(lua, &parent_verdict)?)?;
        if let Some(schema) = call.get("schema") {
            action.set("schema", mare_shared::lua::json_to_lua_lossless(lua, schema)?)?;
        }
        action.set(
            "history",
            mare_shared::lua::json_to_lua_lossless_bounded(
                lua,
                &history,
                mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES,
            )?,
        )?;
        let request: mlua::Function = service.get("request")?;
        let (result, receipt): (Value, Value) = match req_opts {
            Some(opts) => request.call_async((service, "tool", action, check_policy, answer, Value::Nil, opts)).await?,
            None => request.call_async((service, "tool", action, check_policy, answer)).await?,
        };
        let mut vote = mare_shared::lua::lua_to_json(lua, result)?;
        if let Some(d @ ("allow_session" | "allow_amend")) =
            vote.get("decision").and_then(|v| v.as_str()).map(str::to_owned).as_deref()
        {
            let grant = d.to_string();
            vote["decision"] = "allow".into();
            vote["grant"] = grant.into();
        }
        let mut human_approved = false;
        if vote["decision"] == "allow" {
            let receipt = mare_shared::lua::lua_to_json(lua, receipt)?;
            human_approved = mare_binding::approval::consume_receipt(
                lua,
                &receipt,
                call.get("snapshot").and_then(|v| v.as_str()),
                call.get("arguments"),
            );
            if vote["source"] == "user" && !human_approved {
                vote["source"] = "policy".into();
            }
            if call["protected_arguments"] == true && !human_approved {
                vote = serde_json::json!({"decision":"deny","reason":"hidden values require a native human response","source":"approval_policy"});
            }
        }
        if vote["decision"] == "unavailable" {
            vote["decision"] = "deny".into();
            vote["unavailable"] = true.into();
        }
        let source = ApprovalSource::from_wire(vote["source"].as_str()).as_str();
        if let Some(object) = vote.as_object_mut() {
            object.remove("source");
            object.remove("outcome");
        }
        Ok((vote, source, human_approved))
    }

    async fn child_policy_vote(
        &self,
        event: &str,
        call: &serde_json::Value,
        info: &serde_json::Value,
        verdict: &serde_json::Value,
    ) -> mlua::Result<Option<serde_json::Value>> {
        let lua = self.lua();
        let j = |v: &serde_json::Value| mare_shared::lua::json_to_lua_lossless(lua, v);
        if self.kernel.voter_count(event) > 0 {
            let v = self
                .kernel
                .policy_with(
                    event,
                    MultiValue::from_vec(vec![j(call)?, j(info)?, j(verdict)?]),
                    mare_kernel::PolicyOptions { require_explicit_approval: event == "tool/approval-host" },
                )
                .await?;
            let mut out = serde_json::json!({ "decision": v.get::<String>("decision")? });
            for k in ["grant", "reason", "scope", "source", "human_approval"] {
                if let Some(x) = v.get::<Option<String>>(k)? {
                    out[k] = x.into();
                }
            }
            for k in ["abort", "unavailable"] {
                if v.get::<Option<bool>>(k)?.unwrap_or(false) {
                    out[k] = true.into();
                }
            }
            if let Value::Table(constraints) = v.get::<Value>("permission_constraints")? {
                // Child policy votes accept one profile; retain all constraints by forwarding
                // their intersection in the child approval adapter below.
                out["permission_constraints"] = mare_shared::lua::lua_to_json(lua, Value::Table(constraints))?;
            }
            if let Value::Table(constraints) = v.get::<Value>("sandboxes")? {
                out["sandbox_constraints"] = mare_shared::lua::lua_to_json(lua, Value::Table(constraints))?;
            }
            // a remembered rule is the one proposed to (and shown by) the listener, never another
            if out["decision"] == "allow"
                && let Value::Table(a) = v.get::<Value>("amendment")?
                && let Some(proposed) = proposed_rule::snapshot(verdict)
                && proposed_rule::is(&proposed, &mare_shared::lua::lua_to_json(lua, Value::Table(a))?)
            {
                out["amendment"] = proposed;
            }
            return Ok(Some(out));
        }
        Ok(None)
    }

    /// -> (policy vote for the child, who answered: "policy" | "user" | "rule" | "none")
    async fn decide_child_approval(
        self: &Arc<Self>,
        call: &serde_json::Value,
        info: &serde_json::Value,
        verdict: &serde_json::Value,
        maybe_rule_allow: Option<serde_json::Value>,
        review_required: bool,
    ) -> mlua::Result<(serde_json::Value, &'static str)> {
        let lua = self.lua();
        let mut forwarded_verdict = verdict.clone();
        if review_required {
            if !forwarded_verdict.is_object() {
                forwarded_verdict = serde_json::json!({});
            }
            forwarded_verdict["review_required"] = true.into();
        }
        if let Some(mut vote) = self.child_policy_vote("tool/approval-host", call, info, &forwarded_verdict).await?
            && vote["decision"] != "ask"
        {
            if vote["decision"] == "allow"
                && vote.get("rule_lift").is_none()
                && let Some(lift) = maybe_rule_allow.and_then(|r| r.get("rule_lift").cloned())
            {
                vote["rule_lift"] = lift;
            }
            return Ok((vote, "user"));
        }
        if !review_required && let Some(rule_allow) = maybe_rule_allow {
            return Ok((rule_allow, "rule"));
        }
        let connected: bool = lua.load("return mare.ui.connected()").set_name("=child_approval").eval()?;
        if !connected {
            // nobody can answer → `unavailable` (a recorded deny), same as the parent
            return Ok((child_unavailable("approval required but nobody can answer (headless)"), "none"));
        }
        // the proposed rule, fixed now: the dialog gets a copy to show and "remember" keeps this
        let proposed = proposed_rule::snapshot(verdict);
        let mut shown = verdict.clone();
        if let Some(v) = shown.as_object_mut() {
            v.remove("amendment"); // the child's own text never reaches the dialog
        }
        let ev = serde_json::json!({ "call": call, "info": info, "verdict": shown, "rule": proposed });
        // one dialog at a time: wait for open dialogs to close; if ours is replaced by another
        // one (it ends without an answer while another started), ask again afterwards
        for _ in 0..10 {
            self.waits.until_idle().await;
            let before = self.waits.started.get();
            // the presentation plugin (agent-approvals) answers; nobody, an error or anything
            // outside the closed set of choices is `unavailable`, never an approval
            let Some(answer) = self.kernel.bail_json("agent/approval-dialog", ev.clone()).await else {
                return Ok((child_unavailable(NO_DIALOG), "none"));
            };
            let Ok(choice) = dialog_choice(&answer, proposed.is_some()) else {
                return Ok((child_unavailable(INVALID_ANSWER), "error"));
            };
            if choice.is_none() && self.waits.started.get() > before + 1 {
                continue; // replaced by another dialog, not answered
            }
            let mut vote = match choice {
                Some(d @ ("allow" | "allow_session" | "allow_amend")) => {
                    let mut v = serde_json::json!({ "decision": "allow", "grant": d });
                    // "remember" keeps the host's snapshot, whatever the answer carries
                    if d == "allow_amend"
                        && let Some(a) = &proposed
                    {
                        v["amendment"] = a.clone();
                    }
                    v
                }
                Some("abort") => {
                    serde_json::json!({ "decision": "deny", "abort": true, "reason": "declined by the user" })
                }
                _ => {
                    // an explicit Deny may say why (the child's model reads it); dismissed: no reason
                    let why = answer.get("reason").and_then(|r| r.as_str()).filter(|_| choice.is_some());
                    let reason = match why {
                        Some(w) => format!("declined by the user: {w}"),
                        None => "declined by the user".to_string(),
                    };
                    serde_json::json!({ "decision": "deny", "reason": reason })
                }
            };
            if let Some(token) = answer.get("human_approval").and_then(|v| v.as_str()) {
                vote["human_approval"] = token.into();
            }
            if vote["decision"] == "allow"
                && vote.get("rule_lift").is_none()
                && let Some(lift) = maybe_rule_allow.and_then(|r| r.get("rule_lift").cloned())
            {
                vote["rule_lift"] = lift;
            }
            return Ok((vote, "user"));
        }
        Ok((serde_json::json!({ "decision": "deny", "reason": "the approval dialog kept being replaced" }), "user"))
    }
}

/// The vote a child gets when nobody can answer.
fn child_unavailable(reason: &str) -> serde_json::Value {
    serde_json::json!({ "decision": "deny", "unavailable": true, "reason": reason })
}

const NO_DIALOG: &str = "approval required but no approval dialog answered (agent-approvals is off or failed)";
const INVALID_ANSWER: &str = "approval required but the approval dialog returned an invalid answer";

/// The dialog's answer `{choice?, reason?}` → its choice (`None`: dismissed without one), or
/// `Err` for anything else: not a table, a choice outside the set, "remember" with no rule shown.
fn dialog_choice(answer: &serde_json::Value, has_rule: bool) -> Result<Option<&'static str>, ()> {
    let fields = match answer {
        serde_json::Value::Object(o) => o,
        serde_json::Value::Array(a) if a.is_empty() => return Ok(None), // `{}` from Lua
        _ => return Err(()),
    };
    let choice = match fields.get("choice") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(c) => c.as_str().ok_or(())?,
    };
    ["allow", "allow_session", "allow_amend", "deny", "abort"]
        .into_iter()
        .find(|c| *c == choice && (*c != "allow_amend" || has_rule))
        .map(Some)
        .ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parent_child_approval_and_shared_rule_preserve_json_null_fields() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = std::env::temp_dir().join(format!("mare-child-null-test-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let host = Host::new(crate::HostOptions {
                    cwd: dir.clone(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let source = mare_kernel::PluginSource {
                    name_hint: "child-null-permissions".into(),
                    origin: mare_kernel::Origin::Builtin,
                    path: None,
                    code: Some(
                        br#"
                    return { name = "child-null-permissions", provide = { "permissions" },
                        apply = function(ctx)
                            local Permissions = require("cordis").class("ChildNullPermissions", mare.Service)
                            function Permissions:constructor(c)
                                Permissions.super.constructor(self, c, "permissions")
                            end
                            function Permissions:preset() return "ask" end
                            function Permissions:get() return {} end
                            function Permissions:list() return { data = {} } end
                            function Permissions:presets() return {} end
                            function Permissions:set() end
                            function Permissions:rules() return {} end
                            function Permissions:check() return {} end
                            function Permissions:describe() return {} end
                            function Permissions:add_rules() end
                            function Permissions:request() return {} end
                            function Permissions:grants() return {} end
                            function Permissions:describe_request() return {} end
                            function Permissions:resolve_request() return {} end
                            function Permissions:commit_request() end
                            function Permissions:policy_revision() return 0 end
                            function Permissions:is_partial() return false end
                            function Permissions:rule_verdict(call)
                                if call and call.arguments and call.arguments.mode == mare.json.null then
                                    return { decision = "allow" }
                                end
                                return { decision = "deny", reason = "mode=null was dropped in rule_verdict" }
                            end
                            ctx:plugin(Permissions)
                        end }
                    "#,
                    ),
                };
                let def = host.kernel.load_def(&source).unwrap();
                let (replaced, refused) = host.kernel.merge_defs(vec![def]);
                assert!(replaced.is_empty() && refused.is_empty());
                host.kernel.start().await.unwrap();
                assert!(host.service_table("permissions").is_some());
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                lua.globals().set("child_null_ctx", ctx).unwrap();
                lua.load(
                    r#"
                    child_null_ctx:get("policy"):voter("tool/approval", function(call)
                        if call and call.arguments and call.arguments.mode == mare.json.null then
                            return { decision = "allow" }
                        end
                        return { decision = "deny", reason = "mode=null was dropped in tool/approval" }
                    end)
                    child_null_ctx:get("policy"):voter("tool/approval-host", function(call)
                        if call and call.arguments and call.arguments.mode == mare.json.null then
                            return { decision = "allow" }
                        end
                        return { decision = "deny", reason = "mode=null was dropped in tool/approval-host" }
                    end)
                    "#,
                )
                .exec()
                .unwrap();

                let call = serde_json::json!({"id":"c1","name":"bash","arguments":{"mode":null},
                    "snapshot":"snapshot", "approval_key":"key", "access":{"state":"unknown"}});
                let info = serde_json::json!({"access_state":"unknown", "access_version":1, "tool_version":1});
                let verdict = serde_json::json!({"decision":"ask"});

                let policy_vote =
                    host.child_policy_vote("tool/approval", &call, &info, &verdict).await.unwrap().unwrap();
                assert_eq!(policy_vote["decision"], "allow", "child_policy_vote dropped null: {policy_vote}");

                let (vote, _) = host.decide_child_approval(&call, &info, &verdict, None, false).await.unwrap();
                assert_eq!(vote["decision"], "allow", "decide_child_approval dropped null: {vote}");

                let rule = shared_rule::evaluate(&host, &call, &info).await.unwrap().unwrap();
                assert_eq!(rule["decision"], "allow", "shared_rule::evaluate dropped null: {rule}");

                host.shutdown().await;
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test]
    async fn review_child_approval_handles_history_exceeding_10k_nodes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = std::env::temp_dir().join(format!("mare-child-history-test-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let host = Host::new(crate::HostOptions {
                    cwd: dir.clone(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "approvals": { "reviewer": "fixture" }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                lua.globals().set("child_hist_ctx", ctx).unwrap();
                lua.load(
                    r#"
                    -- > 10,000 JSON nodes (2,500 messages * ~7 nodes = ~17,500 nodes)
                    local msgs = {}
                    for i = 1, 2500 do
                        msgs[i] = { role = "user", content = { { type = "text", text = "msg " .. i } } }
                    end
                    local agent = assert(child_hist_ctx:get("agent"))
                    agent:set_messages(msgs, { notify = false })
                    local approvals = assert(child_hist_ctx:get("approvals"))
                    approvals:register_reviewer("fixture", function(action)
                        assert(#action.history == 2500, "expected 2500 history entries, got " .. tostring(#action.history))
                        return { decision = "allow" }
                    end)
                    "#,
                )
                .exec()
                .unwrap();

                let call = serde_json::json!({"id":"c1","name":"bash","arguments":{"command":"echo hi"}});
                let info = serde_json::json!({});
                let verdict = serde_json::json!({"decision":"ask"});

                let (vote, source, _) =
                    host.review_child_approval(false, false, false, &call, &info, &verdict, None).await.unwrap();
                assert_eq!(vote["decision"], "allow", "unexpected vote: {vote}");
                assert_eq!(source, "reviewer");

                host.shutdown().await;
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    /// History beyond the lossless node budget, or carrying a non-JSON field, fails the review
    /// (the caller turns it into a deny) before the reviewer sees a partial or lossy copy.
    #[tokio::test]
    async fn review_child_approval_rejects_over_budget_or_non_json_history() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = std::env::temp_dir().join(format!("mare-child-history-budget-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let host = Host::new(crate::HostOptions {
                    cwd: dir.clone(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "approvals": { "reviewer": "fixture" }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let lua = host.lua();
                let ctx = host.kernel.host_ctx().unwrap();
                lua.globals().set("child_budget_ctx", ctx).unwrap();
                lua.globals().set("MAX_NODES", mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES).unwrap();
                lua.load(
                    r#"
                    REVIEWED = 0
                    local approvals = assert(child_budget_ctx:get("approvals"))
                    approvals:register_reviewer("fixture", function()
                        REVIEWED = REVIEWED + 1
                        return { decision = "allow" }
                    end)
                    function SET_HISTORY(kind)
                        local agent = assert(child_budget_ctx:get("agent"))
                        if kind == "big" then
                            -- each text part is 3 nodes: just over the budget in one message
                            local parts = {}
                            for i = 1, MAX_NODES // 3 + 1 do
                                parts[i] = { type = "text", text = "x" }
                            end
                            agent:set_messages({ { role = "user", content = parts } }, { notify = false })
                        else
                            agent:set_messages({ { role = "user", content = { { type = "text", text = "hi" } },
                                extra = function() end } }, { notify = false })
                        end
                    end
                    "#,
                )
                .exec()
                .unwrap();

                let call = serde_json::json!({"id":"c1","name":"bash","arguments":{"command":"echo hi"}});
                let info = serde_json::json!({});
                let verdict = serde_json::json!({"decision":"ask"});
                for kind in ["big", "function"] {
                    lua.load(format!("SET_HISTORY('{kind}')")).exec_async().await.unwrap();
                    let error = host
                        .review_child_approval(false, false, false, &call, &info, &verdict, None)
                        .await
                        .unwrap_err()
                        .to_string();
                    let expected = if kind == "big" { "exceeds resource limit" } else { "unsupported JSON value" };
                    assert!(error.contains(expected), "{kind}: {error}");
                }
                assert_eq!(lua.load("return REVIEWED").eval::<i64>().unwrap(), 0);

                host.shutdown().await;
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    /// Lossless numeric round-trip on the child-approval path: integers and decimals in the projected history and in the
    /// child's call reach the reviewer exactly; a call number Lua cannot hold fails the review
    /// (deny) instead of being rounded or exposed as serde_json's private number map.
    #[tokio::test]
    async fn review_child_approval_numbers_are_exact_or_refused() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = std::env::temp_dir().join(format!("mare-child-numbers-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let host = Host::new(crate::HostOptions {
                    cwd: dir.clone(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "approvals": { "reviewer": "fixture" }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let lua = host.lua();
                lua.globals().set("child_num_ctx", host.kernel.host_ctx().unwrap()).unwrap();
                lua.load(
                    r#"
                    REVIEWED = 0
                    local function show(n)
                        if type(n) ~= "number" then return type(n) end
                        return math.type(n) .. ":" .. (math.type(n) == "integer" and tostring(n) or ("%.17g"):format(n))
                    end
                    local approvals = assert(child_num_ctx:get("approvals"))
                    approvals:register_reviewer("fixture", function(action)
                        REVIEWED = REVIEWED + 1
                        local meta = action.history[1].meta
                        SEEN = {
                            hist_int = show(meta.i), hist_neg = show(meta.neg), hist_dec = show(meta.f),
                            call = show(action.call.arguments.n),
                        }
                        return { decision = "allow" }
                    end)
                    local agent = assert(child_num_ctx:get("agent"))
                    agent:set_messages({ { role = "user", content = { { type = "text", text = "hi" } },
                        meta = { i = 9007199254740993, neg = -42, f = 0.1 } } }, { notify = false })
                    "#,
                )
                .exec_async()
                .await
                .unwrap();

                let info = serde_json::json!({});
                let verdict = serde_json::json!({"decision":"ask"});
                let call_with = |raw: &str| -> serde_json::Value {
                    let n: serde_json::Value = serde_json::from_str(raw).unwrap();
                    assert_eq!(n.to_string(), raw, "the call must carry the exact number text");
                    serde_json::json!({"id":"c1","name":"bash","arguments":{"command":"echo hi","n":n}})
                };

                for (raw, want) in [("9007199254740993", "integer:9007199254740993"), ("0.1", "float:0.1")] {
                    let (vote, _, _) = host
                        .review_child_approval(false, false, false, &call_with(raw), &info, &verdict, None)
                        .await
                        .unwrap();
                    assert_eq!(vote["decision"], "allow", "{raw}: {vote}");
                    let seen: std::collections::HashMap<String, String> = lua.load("return SEEN").eval().unwrap();
                    assert_eq!(seen["hist_int"], "integer:9007199254740993", "{seen:?}");
                    assert_eq!(seen["hist_neg"], "integer:-42", "{seen:?}");
                    let float = |s: &str| s.strip_prefix("float:").map(|v| v.parse::<f64>().unwrap());
                    assert_eq!(float(&seen["hist_dec"]), Some(0.1), "{seen:?}");
                    match float(want) {
                        Some(f) => assert_eq!(float(&seen["call"]), Some(f), "{raw}: {seen:?}"),
                        None => assert_eq!(seen["call"], want, "{raw}: {seen:?}"),
                    }
                }
                let reviewed_ok = lua.load("return REVIEWED").eval::<i64>().unwrap();
                assert_eq!(reviewed_ok, 2);

                for raw in ["1.00000000000000001", "18446744073709551616"] {
                    let error = host
                        .review_child_approval(false, false, false, &call_with(raw), &info, &verdict, None)
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(error.contains("cannot be represented losslessly"), "{raw}: {error}");
                    assert!(!error.contains("$serde_json"), "{raw}: private number map leaked: {error}");
                }
                assert_eq!(
                    lua.load("return REVIEWED").eval::<i64>().unwrap(),
                    reviewed_ok,
                    "refused calls reach no reviewer"
                );

                host.shutdown().await;
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[test]
    fn approval_source_roundtrips_cancelled_variant() {
        for (wire, expected) in [
            (Some("rule"), ApprovalSource::Rule),
            (Some("user"), ApprovalSource::User),
            (Some("policy"), ApprovalSource::Policy),
            (Some("none"), ApprovalSource::None),
            (Some("error"), ApprovalSource::Error),
            (Some("reviewer"), ApprovalSource::Reviewer),
            (Some("cancelled"), ApprovalSource::Cancelled),
            (Some("other"), ApprovalSource::ApprovalPolicy),
            (None, ApprovalSource::ApprovalPolicy),
        ] {
            let parsed = ApprovalSource::from_wire(wire);
            assert_eq!(parsed, expected);
            assert!(!parsed.as_str().is_empty());
        }
        let unavail = child_unavailable("parent ended");
        assert_eq!(unavail["decision"], "deny");
        assert_eq!(unavail["unavailable"], true);
    }
}
