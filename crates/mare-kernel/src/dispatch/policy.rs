//! Fail-closed policy dispatch and strict vote validation.

use mlua::{Function, Lua, MultiValue, Table, Value};

use crate::dispatch::events::record_event;
use crate::{Kernel, PolicyFold, PolicyMetadata};

/// Producer-declared options for [`Kernel::policy_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyOptions {
    /// Require at least one explicit `"allow"` vote (abstaining `nil` votes or
    /// zero voters produce an `unavailable` `"deny"`).
    pub require_explicit_approval: bool,
}

impl Kernel {
    /// Security dispatch. Rules:
    /// * voters run in order (prepend, then registration); the **first `deny` stops** the
    ///   dispatch and is final; otherwise the result folds `ask` > `allow`;
    /// * an error, timeout (`voter(..., {timeout_ms})`), cancellation or invalid vote is a `deny`;
    /// * a vote is `nil` (no objection), `"allow"`, `"deny"`, `"ask"`, or a table with exactly
    ///   these fields ([`parse_vote`], strict validation — unknown decisions or
    ///   fields make the vote invalid): `decision` (required) · `reason` · `grant` (with allow:
    ///   `"allow"|"allow_session"|"allow_amend"`) · `abort` (with
    ///   deny: end the turn) · `unavailable` (with deny: nobody could answer) · `amendment` ·
    ///   `sandbox` (a restriction), `sandbox_constraints` (additional restrictions) and
    ///   `sandbox_grant` (one explicit tool/before escalation candidate). Votes never rewrite: arguments are frozen before voting (an `arguments`
    ///   field is an invalid vote, i.e. deny) — rewriting happens earlier in [`Kernel::rewrite`];
    /// * `net/approval` additionally requires an explicit `allow` vote: no voters or only
    ///   `nil` votes produce an unavailable denial; an `ask` never grants access;
    /// * stateful checks occupy one decision slot (`{slot = name}`): only the first registered
    ///   voter of a slot runs.
    ///
    /// Returns `{decision, reason?, reasons, grant?, abort?, unavailable?, amendment?, sandboxes,
    /// sandbox_grant?}`.
    pub async fn policy(&self, event: &str, args: MultiValue) -> mlua::Result<Table> {
        self.policy_with(event, args, PolicyOptions::default()).await
    }

    /// Like [`Kernel::policy`], with producer-declared [`PolicyOptions`].
    pub async fn policy_with(&self, event: &str, args: MultiValue, options: PolicyOptions) -> mlua::Result<Table> {
        self.validate_event_dispatch("policy", event, None)?;
        record_event(event);
        let metadata = self.policy_metadata(event);
        // a security vote is never dropped from a tap: the producer waits for room
        // Policy callback inputs are a decision capability, not an observer event payload.
        // All policy operations use the same audit envelope; authorized hooks still get args.
        let audit = self.lua().create_table()?;
        audit.set("dispatch", "policy")?;
        let mut ev = self.tap_event(event, &MultiValue::from_vec(vec![Value::Table(audit)])).await;
        if ev.turn_id.is_none() {
            ev.turn_id = crate::dispatch::tap::turn_id_from_args(&args);
        }
        let lua = self.lua().clone();
        let args: Vec<Value> =
            args.into_iter().map(|v| mare_shared::lua::deep_clone_value(&lua, v)).collect::<mlua::Result<_>>()?;
        let mut fold = PolicyFoldState::new(lua, metadata)?;
        // the policy service's voters of this point
        for voter in super::policy_prims::service_list(self, "voters", event)? {
            let vote = self.eval_voter(event, &voter, &args, &ev, &mut fold).await;
            if fold.apply_vote(&voter.owner, vote)? {
                break;
            }
        }
        let authority_provider = fold
            .metadata
            .authority_service
            .as_deref()
            .and_then(|service| self.service_provider(service).map(|(p, _)| p));
        fold.finalize(options.require_explicit_approval, authority_provider.as_deref())
    }

    /// Strict rewrite chain:
    /// rewriters run in order (prepend, then registration); each gets a copy of the call with
    /// the arguments so far and returns `nil` (keep) or a table of new arguments, which the next
    /// rewriter sees. An error, timeout or non-table result fails the whole rewrite (`Err`,
    /// the caller refuses the call) — a broken rewriter must not silently let the original run.
    pub async fn rewrite(&self, event: &str, call: Table) -> mlua::Result<std::result::Result<Value, String>> {
        if let Err(err) = self.validate_event_dispatch("rewrite", event, None) {
            return Ok(Err(err.to_string()));
        }
        record_event(event);
        let mut tapped = MultiValue::new();
        tapped.push_back(Value::Table(call.clone()));
        let ev = self.tap_event(event, &tapped).await;
        let rewriters = super::policy_prims::service_list(self, "rewriters", event)?;
        self.rewrite_chain(event, rewriters, call, &ev).await
    }
}

fn grant_rank(g: &str) -> u8 {
    match g {
        "allow_amend" => 2,
        "allow_session" => 1,
        _ => 0,
    }
}

fn freeze_lua_value(lua: &Lua, value: Value) -> mlua::Result<Value> {
    match value {
        Value::Table(_) => {
            let json = mare_shared::lua::lua_to_json(lua, value)?;
            mare_shared::lua::json_to_lua(lua, &json)
        }
        other => Ok(other),
    }
}

struct PolicyFoldState {
    lua: Lua,
    metadata: PolicyMetadata,
    decision: &'static str,
    explicitly_approved: bool,
    approved_by: Vec<String>,
    reason: Option<String>,
    grant: &'static str,
    abort: bool,
    unavailable: bool,
    source: Option<String>,
    human_approval: Option<String>,
    amendment: Value,
    ask_amendment: Value,
    ask_reasons: Vec<String>,
    categories: Vec<String>,
    sandboxes: Table,
    sandbox_grant: Option<Value>,
    permission_constraints: Vec<Value>,
    permission_scope: Option<&'static str>,
    permission_once: bool,
    reasons: Table,
}

impl PolicyFoldState {
    fn new(lua: Lua, metadata: PolicyMetadata) -> mlua::Result<Self> {
        // Every sandbox vote is a restriction; an explicit grant is a separate baseline.
        let sandboxes = lua.create_table()?;
        let reasons = lua.create_table()?;
        Ok(Self {
            lua,
            metadata,
            decision: "allow",
            explicitly_approved: false,
            approved_by: Vec::new(),
            reason: None,
            grant: "allow",
            abort: false,
            unavailable: false,
            source: None,
            human_approval: None,
            amendment: Value::Nil,
            // an `ask` vote may propose an amendment (a rule the approval dialog can offer to keep)
            ask_amendment: Value::Nil,
            ask_reasons: Vec::new(),
            categories: Vec::new(),
            sandboxes,
            sandbox_grant: None,
            permission_constraints: Vec::new(),
            permission_scope: None,
            permission_once: false,
            reasons,
        })
    }

    fn deny(&mut self, reason: impl Into<String>) -> bool {
        self.decision = "deny";
        self.reason = Some(reason.into());
        true
    }

    /// Returns `true` when folding must stop immediately (e.g. on `deny` or constraint error).
    fn apply_vote(&mut self, plugin: &str, vote: Vote) -> mlua::Result<bool> {
        if vote.decision != "allow" || vote.reason.is_some() {
            let e = self.lua.create_table()?;
            e.set("plugin", plugin)?;
            e.set("decision", vote.decision)?;
            e.set("reason", vote.reason.clone())?;
            // Preserve each proposal, even when an earlier ask owns the dialog's single
            // amendment. Security consumers must not infer a proposal from display text.
            e.set("amendment", vote.amendment.clone())?;
            self.reasons.push(e)?;
        }
        if vote.decision == "deny" {
            // the first deny is final: later listeners can't turn it into an allow
            self.decision = "deny";
            self.reason = vote.reason;
            self.abort = vote.abort;
            self.unavailable = vote.unavailable;
            self.source = vote.source;
            self.human_approval = vote.human_approval;
            return Ok(true);
        }
        self.permission_once |= (vote.permissions.is_some() || !vote.permission_constraints.is_empty())
            && vote.scope.is_none()
            && vote.grant == "allow";
        if !vote.permission_constraints.is_empty() && !self.metadata.permissions {
            return Ok(self.deny("permission_constraints is not allowed in this policy event"));
        }
        for profile in vote.permission_constraints {
            self.permission_constraints.push(freeze_lua_value(&self.lua, profile)?);
        }
        if let Some(profile) = vote.permissions {
            if !self.metadata.permissions {
                return Ok(self.deny("permissions is not allowed in this policy event"));
            }
            self.permission_constraints.push(freeze_lua_value(&self.lua, profile)?);
        }
        if let Some(scope) = vote.scope {
            if !self.metadata.permissions {
                return Ok(self.deny("scope is not allowed in this policy event"));
            }
            if self.permission_scope != Some("turn") {
                self.permission_scope = Some(scope);
            }
        }
        if let Some(sb) = vote.sandbox {
            self.sandboxes.push(freeze_lua_value(&self.lua, sb)?)?;
        }
        for sb in vote.sandbox_constraints {
            self.sandboxes.push(freeze_lua_value(&self.lua, sb)?)?;
        }
        if let Some(sb) = vote.sandbox_grant {
            if !self.metadata.sandbox_grant {
                return Ok(self.deny("sandbox_grant is not allowed in this policy event"));
            }
            if self.sandbox_grant.is_some() {
                return Ok(self.deny("conflicting policy sandbox grants"));
            }
            self.sandbox_grant = Some(freeze_lua_value(&self.lua, sb)?);
        }
        if self.metadata.join_ask_reasons && vote.decision == "ask" {
            self.ask_reasons.push(vote.reason.clone().unwrap_or_else(|| format!("{plugin} requires approval")));
        }
        self.fold_decision(
            vote.decision,
            vote.categories,
            vote.category,
            vote.reason,
            vote.amendment,
            vote.grant,
            vote.source,
            vote.human_approval,
        );
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn fold_decision(
        &mut self,
        vote_decision: &'static str,
        vote_categories: Vec<String>,
        vote_category: Option<String>,
        vote_reason: Option<String>,
        vote_amendment: Option<Value>,
        vote_grant: &'static str,
        vote_source: Option<String>,
        vote_human_approval: Option<String>,
    ) {
        match vote_decision {
            "ask" => {
                let mut required = vote_categories;
                if let Some(category) = vote_category {
                    required.push(category);
                }
                if required.is_empty() {
                    required.push("tool".into());
                }
                for category in required {
                    if !self.categories.contains(&category) {
                        self.categories.push(category);
                    }
                }
                if self.metadata.fold == PolicyFold::Approval {
                    // Tri-state approval: "ask" is an explicit abstain that never
                    // overrides an allow from another listener.
                    if self.reason.is_none() {
                        self.reason = vote_reason;
                    }
                } else if self.decision == "allow" {
                    self.decision = "ask";
                    self.reason = vote_reason;
                } else if self.reason.is_none() {
                    self.reason = vote_reason;
                }
                if self.ask_amendment.is_nil()
                    && let Some(a) = vote_amendment
                {
                    self.ask_amendment = a;
                }
            }
            _ => {
                if vote_human_approval.is_some() {
                    self.human_approval = vote_human_approval;
                }
                if self.metadata.fold == PolicyFold::Approval && vote_source.is_some() {
                    self.source = vote_source;
                }
                if grant_rank(vote_grant) > grant_rank(self.grant) {
                    self.grant = vote_grant;
                    if let Some(a) = vote_amendment {
                        self.amendment = a;
                    }
                }
                if self.decision == "allow" && self.reason.is_none() {
                    self.reason = vote_reason;
                }
            }
        }
    }

    fn finalize(mut self, require_explicit_approval: bool, authority_provider: Option<&str>) -> mlua::Result<Table> {
        // Approval is an affirmative grant, unlike ordinary policy's lack of objection.
        // Empty dispatches and abstaining observers must never authorize when the producer
        // requires explicit approval or the policy fold is affirmative.
        if (require_explicit_approval || self.metadata.fold == PolicyFold::Affirmative)
            && self.decision == "allow"
            && !self.explicitly_approved
        {
            self.decision = "deny";
            self.reason = Some("access requires explicit approval".into());
            self.unavailable = true;
        }
        // Tri-state tool approval: if no listener allowed or denied (all abstained),
        // return "ask" so normal TUI/host approval continues.
        if self.metadata.fold == PolicyFold::Approval && self.decision == "allow" && !self.explicitly_approved {
            self.decision = "ask";
        }
        if self.decision == "ask" && self.metadata.join_ask_reasons {
            self.reason = Some(self.ask_reasons.join(" · "));
        }
        let out = self.lua.create_table()?;
        out.set("decision", self.decision)?;
        out.set("categories", self.categories)?;
        out.set("reason", self.reason.clone())?;
        out.set("reasons", self.reasons)?;
        if self.metadata.authority_service.is_some() {
            let authority_approved = authority_provider.is_some_and(|p| self.approved_by.iter().any(|a| a == p));
            out.set("authority_approved", authority_approved)?;
        }
        out.set("approved_by", self.lua.create_sequence_from(self.approved_by)?)?;
        if let Some(src) = self.source {
            out.set("source", src)?;
        }
        if let Some(token) = self.human_approval {
            out.set("human_approval", token)?;
        }
        match self.decision {
            "deny" => {
                if self.abort {
                    out.set("abort", true)?;
                }
                if self.unavailable {
                    out.set("unavailable", true)?;
                }
            }
            d => {
                out.set("sandboxes", self.sandboxes.clone())?;
                if !self.permission_constraints.is_empty() {
                    out.set("permission_constraints", self.lua.create_sequence_from(self.permission_constraints)?)?;
                }
                out.set("scope", if self.permission_once { None } else { self.permission_scope })?;
                out.set("sandbox_grant", self.sandbox_grant)?;
                if d == "allow" {
                    let transient = self.permission_once || self.permission_scope == Some("turn");
                    out.set("grant", if transient { "allow" } else { self.grant })?;
                    out.set("amendment", if transient { Value::Nil } else { self.amendment })?;
                } else {
                    out.set("amendment", self.ask_amendment)?;
                }
            }
        }
        Ok(out)
    }
}

/// One validated policy vote.
struct Vote {
    decision: &'static str,
    category: Option<String>,
    categories: Vec<String>,
    reason: Option<String>,
    grant: &'static str,
    abort: bool,
    unavailable: bool,
    source: Option<String>,
    human_approval: Option<String>,
    amendment: Option<Value>,
    sandbox: Option<Value>,
    sandbox_constraints: Vec<Value>,
    sandbox_grant: Option<Value>,
    permission_constraints: Vec<Value>,
    permissions: Option<Value>,
    scope: Option<&'static str>,
}

impl Vote {
    fn of(decision: &'static str) -> Self {
        Self {
            decision,
            category: None,
            categories: Vec::new(),
            reason: None,
            grant: "allow",
            abort: false,
            unavailable: false,
            source: None,
            human_approval: None,
            amendment: None,
            sandbox: None,
            sandbox_constraints: vec![],
            sandbox_grant: None,
            permission_constraints: vec![],
            permissions: None,
            scope: None,
        }
    }
    fn deny(reason: String) -> Self {
        Self { reason: Some(reason), ..Self::of("deny") }
    }
}

fn parse_decision(s: &str) -> Result<&'static str, String> {
    match s {
        "allow" => Ok("allow"),
        "deny" => Ok("deny"),
        "ask" => Ok("ask"),
        other => Err(format!("unknown decision `{other}` (allow, deny or ask)")),
    }
}

fn parse_categories(list: &Table) -> Result<Vec<String>, String> {
    let len = list.raw_len();
    let mut count = 0;
    for pair in list.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let Value::Integer(key) = key else {
            return Err("categories must be a string array".into());
        };
        let Value::String(value) = value else {
            return Err("categories must be a string array".into());
        };
        if key < 1 || key as usize > len || value.as_bytes().is_empty() {
            return Err("categories must be a non-empty-string array".into());
        }
        count += 1;
    }
    if count != len {
        return Err("categories must be a dense array".into());
    }
    list.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().map_err(|e| e.to_string())
}

fn parse_permission_constraints(profiles: &Table) -> Result<Vec<Value>, String> {
    let len = profiles.raw_len();
    let mut out = Vec::with_capacity(len);
    for pair in profiles.clone().pairs::<Value, Value>() {
        let (key, profile) = pair.map_err(|e| e.to_string())?;
        if !matches!(key, Value::Integer(i) if i > 0 && i as usize <= len) {
            return Err("permission_constraints must be a dense list".into());
        }
        let Value::Table(ref table) = profile else {
            return Err("permission constraint must be an object".into());
        };
        validate_permissions(table)?;
        out.push(profile);
    }
    if out.len() != len {
        return Err("permission_constraints must not contain holes".into());
    }
    Ok(out)
}

fn parse_sandbox_constraints(list: &Table) -> Result<Vec<Value>, String> {
    let len = list.raw_len();
    for pair in list.clone().pairs::<Value, Value>() {
        let (key, val) = pair.map_err(|e| e.to_string())?;
        if !matches!(key, Value::Integer(i) if i > 0 && i as usize <= len)
            || !matches!(val, Value::Table(_) | Value::String(_))
        {
            return Err("sandbox_constraints must be a sequence of sandbox specs".into());
        }
    }
    let out = list.sequence_values::<Value>().collect::<mlua::Result<Vec<_>>>().map_err(|e| e.to_string())?;
    if out.len() != len {
        return Err("sandbox_constraints must not contain holes".into());
    }
    Ok(out)
}

fn parse_vote_table(t: &Table) -> Result<Vote, String> {
    let mut vote = None::<Vote>;
    let mut fields: Vec<(String, Value)> = vec![];
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, val) = pair.map_err(|e| e.to_string())?;
        let Value::String(k) = k else { return Err("non-string field".into()) };
        fields.push((k.to_string_lossy(), val));
    }
    if let Some((_, d)) = fields.iter().find(|(k, _)| k == "decision") {
        let Value::String(d) = d else { return Err("`decision` must be a string".into()) };
        vote = Some(Vote::of(parse_decision(&d.to_string_lossy())?));
    }
    let mut vote = vote.ok_or("missing `decision`")?;
    for (k, val) in fields {
        match (k.as_str(), val) {
            ("decision", _) => {}
            ("category", Value::String(s)) if vote.decision == "ask" && !s.as_bytes().is_empty() => {
                vote.category = Some(s.to_string_lossy());
            }
            ("category", Value::Nil) => {}
            ("categories", Value::Table(list)) if vote.decision == "ask" => {
                vote.categories = parse_categories(&list)?;
            }
            ("categories", Value::Nil) => {}
            ("reason", Value::String(s)) => vote.reason = Some(s.to_string_lossy()),
            ("reason", Value::Nil) => {}
            ("grant", Value::String(g)) if vote.decision == "allow" => {
                vote.grant = match &*g.to_string_lossy() {
                    "allow" => "allow",
                    "allow_session" => "allow_session",
                    "allow_amend" => "allow_amend",
                    other => return Err(format!("unknown grant `{other}`")),
                }
            }
            (mare_shared::PERMISSIONS_KEY, Value::Table(profile)) if vote.decision == "allow" => {
                validate_permissions(&profile)?;
                vote.permissions = Some(Value::Table(profile));
            }
            ("permission_constraints", Value::Table(profiles)) if vote.decision == "allow" => {
                vote.permission_constraints = parse_permission_constraints(&profiles)?;
            }
            ("scope", Value::String(scope)) if vote.decision == "allow" => {
                vote.scope = Some(match &*scope.to_string_lossy() {
                    "turn" => "turn",
                    mare_shared::SESSION_SCOPE => mare_shared::SESSION_SCOPE,
                    _ => return Err("scope must be turn or session".into()),
                });
            }
            ("abort", Value::Boolean(b)) if vote.decision == "deny" => vote.abort = b,
            ("unavailable", Value::Boolean(b)) if vote.decision == "deny" => vote.unavailable = b,
            ("human_approval", Value::String(s)) => vote.human_approval = Some(s.to_string_lossy()),
            ("source", Value::String(s)) => {
                let s = s.to_string_lossy();
                if s.is_empty() {
                    return Err("`source` must not be empty".into());
                }
                vote.source = Some(s);
            }
            ("amendment", a @ Value::Table(_)) => vote.amendment = Some(a),
            ("sandbox", sb @ (Value::Table(_) | Value::String(_))) => vote.sandbox = Some(sb),
            ("sandbox_grant", sb @ (Value::Table(_) | Value::String(_))) if vote.decision != "deny" => {
                vote.sandbox_grant = Some(sb);
            }
            ("sandbox_constraints", Value::Table(list)) => {
                vote.sandbox_constraints = parse_sandbox_constraints(&list)?;
            }
            ("arguments", _) => {
                return Err("votes can't rewrite arguments (they are frozen before voting)".into());
            }
            (k, val) => return Err(format!("field `{k}` ({}) not allowed here", val.type_name())),
        }
    }
    Ok(vote)
}

/// Strict vote validation: `nil` (no objection), one of the strings
/// `"allow" | "deny" | "ask"`, or a table whose fields are all known and well-typed. Anything
/// else — an unknown decision, an unknown field, a wrong type — is invalid (`Err(why)`), which
/// the dispatcher counts as `deny`.
fn parse_vote(v: &Value) -> Result<Vote, String> {
    match v {
        Value::Nil => Ok(Vote::of("allow")),
        Value::String(s) => parse_decision(&s.to_string_lossy()).map(Vote::of),
        Value::Table(t) => parse_vote_table(t),
        other => Err(format!("a {} is not a vote", other.type_name())),
    }
}

/// A structured permission answer is a constraint on the frozen request, never an escalation.
fn validate_permissions(profile: &Table) -> Result<(), String> {
    for pair in profile.clone().pairs::<String, Value>() {
        let (kind, value) = pair.map_err(|e| e.to_string())?;
        if !matches!(kind.as_str(), "read" | "write" | "network") {
            return Err(format!("unknown permission dimension `{kind}`"));
        }
        if kind == "network" && matches!(value, Value::Boolean(_)) {
            continue;
        }
        let Value::Table(list) = value else {
            return Err(format!("{kind} must be a list"));
        };
        let len = list.raw_len();
        for pair in list.clone().pairs::<Value, Value>() {
            let (key, value) = pair.map_err(|e| e.to_string())?;
            if !matches!(key, Value::Integer(i) if i > 0 && i as usize <= len)
                || !matches!(value, Value::String(ref s) if !s.to_string_lossy().trim().is_empty())
            {
                return Err(format!("{kind} must be a dense list of nonempty strings"));
            }
        }
        if list.clone().sequence_values::<String>().count() != len {
            return Err(format!("{kind} must not contain holes"));
        }
    }
    Ok(())
}

/// One voter or rewriter of the `policy` service, in the order the service keeps
/// them (registration order, `prepend` first; one per decision slot, chosen by the service).
pub(crate) struct Voter {
    pub(crate) f: Function,
    /// the registrant's name, for reasons and reports
    pub(crate) owner: String,
    pub(crate) timeout: Option<std::time::Duration>,
}

impl Kernel {
    /// One voter's (or rewriter's) call: within its timeout, its busy time recorded like any
    /// listener call (mare.profile callback timings).
    async fn call_voter<F>(&self, point: &str, voter: &Voter, call: F) -> mlua::Result<Value>
    where
        F: std::future::Future<Output = mlua::Result<Value>>,
    {
        let bounded = async {
            match voter.timeout {
                Some(d) => tokio::time::timeout(d, call)
                    .await
                    .unwrap_or_else(|_| Err(mlua::Error::runtime(format!("timed out after {}ms", d.as_millis())))),
                None => call.await,
            }
        };
        let (r, busy) = crate::dispatch::timing::timed(bounded).await;
        self.record_slow_callback(point, &voter.owner, busy);
        r
    }

    async fn eval_voter(
        &self,
        point: &str,
        voter: &Voter,
        args: &[Value],
        ev: &crate::dispatch::tap::EventEnvelope,
        fold: &mut PolicyFoldState,
    ) -> Vote {
        let fold_kind = fold.metadata.fold;
        let mut cloned = if args.is_empty() {
            vec![Value::Nil]
        } else {
            args.iter()
                .map(|v| mare_shared::lua::deep_clone_value(self.lua(), v.clone()))
                .collect::<mlua::Result<Vec<_>>>()
                .unwrap_or_else(|_| args.to_vec())
        };
        if let Ok(ev_tbl) = ev.to_lua(self.lua()) {
            cloned.push(Value::Table(ev_tbl));
        }
        let call = mare_shared::lua::call_async::<Value>(self.lua(), &voter.f, MultiValue::from_vec(cloned));
        match self.call_voter(point, voter, call).await {
            Ok(v) => {
                let vote = parse_vote(&v)
                    .unwrap_or_else(|why| Vote::deny(format!("invalid policy result from {}: {why}", voter.owner)));
                let abstained = v.is_nil() || (fold_kind == PolicyFold::Approval && vote.decision == "ask");
                if !abstained && vote.decision == "allow" {
                    fold.explicitly_approved = true;
                    fold.approved_by.push(voter.owner.clone());
                }
                vote
            }
            Err(e) => {
                self.report(point, &voter.owner, &e);
                Vote::deny(format!(
                    "policy check by {} failed: {}",
                    voter.owner,
                    mare_shared::errors::internal_message(&e.to_string())
                ))
            }
        }
    }

    /// The strict rewrite chain over the policy service's rewriters: each gets a copy of the call
    /// with the arguments so far and returns nil (keep) or a table of new arguments; an error,
    /// timeout or non-table result fails the whole rewrite (`Err`).
    async fn rewrite_chain(
        &self,
        point: &str,
        rewriters: Vec<Voter>,
        call: Table,
        ev: &crate::dispatch::tap::EventEnvelope,
    ) -> mlua::Result<std::result::Result<Value, String>> {
        let lua = self.lua().clone();
        let mut args: Value = call.get("arguments")?;
        for rewriter in rewriters {
            let copy = lua.create_table()?;
            for pair in call.pairs::<Value, Value>() {
                let (k, v) = pair?;
                copy.raw_set(k, v)?;
            }
            copy.raw_set("arguments", args.clone())?;
            let ev_tbl = ev.to_lua(&lua)?;
            let fut = rewriter.f.call_async::<Value>((copy, ev_tbl));
            match self.call_voter(point, &rewriter, fut).await {
                Ok(Value::Nil) => {}
                Ok(t @ Value::Table(_)) => args = t,
                Ok(other) => {
                    return Ok(Err(format!(
                        "rewrite by {} returned a {} (nil or an arguments table)",
                        rewriter.owner,
                        other.type_name()
                    )));
                }
                Err(e) => {
                    self.report(point, &rewriter.owner, &e);
                    return Ok(Err(format!("rewrite by {} failed: {e}", rewriter.owner)));
                }
            }
        }
        Ok(Ok(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_declared_policy_points_drive_fold_permissions_sandbox_grant_and_ask_join() {
        let kernel = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let lua = kernel.lua();
        let contracts: Table = lua
            .load(
                r#"return {
                    org = {
                        methods = {},
                        events = {
                            affirmative = { mode = "policy", affirmative = true },
                            approval = {
                                mode = "policy",
                                result = {
                                    type = "object",
                                    properties = {
                                        decision = { type = "string" },
                                        permissions = { type = "object" },
                                        permission_constraints = { type = "array" },
                                        scope = { type = "string" },
                                        source = { type = "string" },
                                    },
                                },
                            },
                            sandboxed = {
                                mode = "policy",
                                authority_service = "custom_auth",
                                result = {
                                    type = "object",
                                    properties = {
                                        decision = { type = "string" },
                                        sandbox_grant = { type = "string" },
                                    },
                                },
                            },
                            verdict = { mode = "policy", join_ask_reasons = true },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        kernel.install_service_contracts(contracts).unwrap();

        // 1. Affirmative fold: abstaining (no explicit allow) -> unavailable deny.
        let aff_meta = kernel.policy_metadata("org/affirmative");
        assert_eq!(aff_meta.fold, PolicyFold::Affirmative);
        assert!(!aff_meta.permissions);
        assert!(!aff_meta.sandbox_grant);
        let aff_empty = PolicyFoldState::new(lua.clone(), aff_meta.clone()).unwrap().finalize(false, None).unwrap();
        assert_eq!(aff_empty.get::<String>("decision").unwrap(), "deny");
        assert_eq!(aff_empty.get::<Option<bool>>("unavailable").unwrap(), Some(true));

        // 2. Approval fold: all abstain -> ask; permissions + scope allowed on approval, denied on affirmative.
        let app_meta = kernel.policy_metadata("org/approval");
        assert_eq!(app_meta.fold, PolicyFold::Approval);
        assert!(app_meta.permissions);
        let app_empty = PolicyFoldState::new(lua.clone(), app_meta.clone()).unwrap().finalize(false, None).unwrap();
        assert_eq!(app_empty.get::<String>("decision").unwrap(), "ask");

        let mut app_fold = PolicyFoldState::new(lua.clone(), app_meta).unwrap();
        let v_allow: Table = lua
            .load(
                "return { decision = 'allow', permissions = { read = {'/a'} }, scope = 'session', source = 'custom' }",
            )
            .eval()
            .unwrap();
        app_fold.explicitly_approved = true;
        assert!(!app_fold.apply_vote("p", parse_vote(&Value::Table(v_allow)).unwrap()).unwrap());
        let app_perm = app_fold.finalize(false, None).unwrap();
        assert_eq!(app_perm.get::<String>("decision").unwrap(), "allow");
        assert_eq!(app_perm.get::<Option<String>>("source").unwrap().as_deref(), Some("custom"));
        assert_eq!(app_perm.get::<Option<String>>("scope").unwrap().as_deref(), Some("session"));
        assert_eq!(
            app_perm.get::<Value>("authority_approved").unwrap(),
            Value::Nil,
            "policy points without authority_service omit authority_approved"
        );

        let mut aff_bad_perm = PolicyFoldState::new(lua.clone(), aff_meta.clone()).unwrap();
        let v_perm: Table = lua.load("return { decision = 'allow', permissions = { read = {'/a'} } }").eval().unwrap();
        assert!(aff_bad_perm.apply_vote("p", parse_vote(&Value::Table(v_perm)).unwrap()).unwrap());
        assert_eq!(aff_bad_perm.finalize(false, None).unwrap().get::<String>("decision").unwrap(), "deny");

        // 3. sandbox_grant allowed where result declares it (`org/sandboxed`), denied where omitted (`org/affirmative`);
        // folded verdicts never emit the removed `sandbox` compatibility alias; contract-declared
        // `authority_service` drives `authority_approved`.
        let sb_meta = kernel.policy_metadata("org/sandboxed");
        assert!(sb_meta.sandbox_grant);
        assert_eq!(sb_meta.authority_service.as_deref(), Some("custom_auth"));
        let mut sb_fold = PolicyFoldState::new(lua.clone(), sb_meta.clone()).unwrap();
        let v_sb: Table = lua.load("return { decision = 'allow', sandbox_grant = 'workspace-write' }").eval().unwrap();
        sb_fold.approved_by.push("auth_plug".into());
        assert!(!sb_fold.apply_vote("auth_plug", parse_vote(&Value::Table(v_sb)).unwrap()).unwrap());
        let sb_res = sb_fold.finalize(false, Some("auth_plug")).unwrap();
        assert_eq!(sb_res.get::<String>("decision").unwrap(), "allow");
        assert_eq!(sb_res.get::<Option<String>>("sandbox_grant").unwrap().as_deref(), Some("workspace-write"));
        assert_eq!(sb_res.get::<Value>("sandbox").unwrap(), Value::Nil, "verdict must not expose sandbox alias");
        assert_eq!(sb_res.get::<Option<bool>>("authority_approved").unwrap(), Some(true));

        let mut sb_restrict_fold = PolicyFoldState::new(lua.clone(), sb_meta).unwrap();
        let v_restr: Table = lua.load("return { decision = 'allow', sandbox = 'read-only' }").eval().unwrap();
        sb_restrict_fold.approved_by.push("other_plug".into());
        assert!(!sb_restrict_fold.apply_vote("other_plug", parse_vote(&Value::Table(v_restr)).unwrap()).unwrap());
        let sb_restr_res = sb_restrict_fold.finalize(false, Some("auth_plug")).unwrap();
        assert_eq!(sb_restr_res.get::<Value>("sandbox").unwrap(), Value::Nil);
        assert_eq!(sb_restr_res.get::<Option<bool>>("authority_approved").unwrap(), Some(false));

        let mut aff_bad_sb = PolicyFoldState::new(lua.clone(), aff_meta).unwrap();
        let v_sb2: Table = lua.load("return { decision = 'allow', sandbox_grant = 'workspace-write' }").eval().unwrap();
        assert!(aff_bad_sb.apply_vote("p", parse_vote(&Value::Table(v_sb2)).unwrap()).unwrap());
        assert_eq!(aff_bad_sb.finalize(false, None).unwrap().get::<String>("decision").unwrap(), "deny");

        // 4. join_ask_reasons joins multiple ask reasons with " · ".
        let verdict_meta = kernel.policy_metadata("org/verdict");
        assert!(verdict_meta.join_ask_reasons);
        let mut v_fold = PolicyFoldState::new(lua.clone(), verdict_meta).unwrap();
        let ask1: Table = lua.load("return { decision = 'ask', reason = 'first' }").eval().unwrap();
        let ask2: Table = lua.load("return { decision = 'ask', reason = 'second' }").eval().unwrap();
        assert!(!v_fold.apply_vote("p1", parse_vote(&Value::Table(ask1)).unwrap()).unwrap());
        assert!(!v_fold.apply_vote("p2", parse_vote(&Value::Table(ask2)).unwrap()).unwrap());
        let v_res = v_fold.finalize(false, None).unwrap();
        assert_eq!(v_res.get::<String>("decision").unwrap(), "ask");
        assert_eq!(v_res.get::<Option<String>>("reason").unwrap().as_deref(), Some("first · second"));
    }

    #[tokio::test]
    async fn eval_voter_and_rewrite_chain_pass_single_payload_and_event_envelope() {
        let kernel = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        kernel.set_session_identity(Some("sess-pol".into()));
        let (_, epoch) = kernel.0.event_log.session_identity();
        let lua = kernel.lua();
        let contracts: Table = lua
            .load(
                r#"return {
                    org = {
                        methods = {},
                        events = {
                            gate = { mode = "policy", doc = "gate" },
                            rw = { mode = "rewrite", doc = "rw" },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        kernel.install_service_contracts(contracts).unwrap();

        let captured: Table = lua.load("CAP = {}; return CAP").eval().unwrap();
        let voter_fn: Function = lua
            .load("return function(data, ev) CAP.voter = { data = data, ev = ev }; return 'allow' end")
            .eval()
            .unwrap();
        let voter = Voter { f: voter_fn, owner: "plug".into(), timeout: None };
        let arg: Table = lua.load("return { item = 'y', turn_id = 't-3' }").eval().unwrap();
        let ev = crate::dispatch::tap::EventEnvelope {
            event: "org/gate".into(),
            session_id: Some("sess-pol".into()),
            session_epoch: epoch.clone(),
            seq: None,
            turn_id: Some("t-3".into()),
        };
        let mut fold = PolicyFoldState::new(lua.clone(), kernel.policy_metadata("org/gate")).unwrap();
        let vote = kernel.eval_voter("org/gate", &voter, &[Value::Table(arg)], &ev, &mut fold).await;
        assert_eq!(vote.decision, "allow");
        let v_ev: Table = captured.get::<Table>("voter").unwrap().get("ev").unwrap();
        assert_eq!(v_ev.get::<String>("type").unwrap(), "org/gate");
        assert_eq!(v_ev.get::<Option<String>>("session_id").unwrap().as_deref(), Some("sess-pol"));
        assert_eq!(v_ev.get::<String>("session_epoch").unwrap(), epoch);
        assert_eq!(v_ev.get::<Option<String>>("turn_id").unwrap().as_deref(), Some("t-3"));
        assert_eq!(v_ev.get::<Option<u64>>("seq").unwrap(), None);

        let rw_fn: Function = lua
            .load("return function(data, ev) CAP.rw = { data = data, ev = ev }; return { extra = true } end")
            .eval()
            .unwrap();
        let rewriter = Voter { f: rw_fn, owner: "plug".into(), timeout: None };
        let call: Table = lua.load("return { name = 't', arguments = {} }").eval().unwrap();
        let rw_ev = crate::dispatch::tap::EventEnvelope {
            event: "org/rw".into(),
            session_id: Some("sess-pol".into()),
            session_epoch: epoch.clone(),
            seq: None,
            turn_id: None,
        };
        let rw_res = kernel.rewrite_chain("org/rw", vec![rewriter], call, &rw_ev).await.unwrap().unwrap();
        assert!(rw_res.as_table().unwrap().get::<bool>("extra").unwrap());
        let r_ev: Table = captured.get::<Table>("rw").unwrap().get("ev").unwrap();
        assert_eq!(r_ev.get::<String>("type").unwrap(), "org/rw");
        assert_eq!(r_ev.get::<Option<String>>("session_id").unwrap().as_deref(), Some("sess-pol"));

        let bad_rw_fn: Function = lua.load("return function() return 123 end").eval().unwrap();
        let bad_rewriter = Voter { f: bad_rw_fn, owner: "plug".into(), timeout: None };
        let call2: Table = lua.load("return { name = 't', arguments = {} }").eval().unwrap();
        let err = kernel.rewrite_chain("org/rw", vec![bad_rewriter], call2, &rw_ev).await.unwrap().unwrap_err();
        assert!(err.contains("returned a integer"), "{err}");
    }
}
