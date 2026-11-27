-- tools.approval: policy version, snapshot hashing and the one-at-a-time approval prompt;
-- part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local registry = D.registry.registry

	-- bumped whenever the effective permission policy may have changed (preset switch, rule
	-- edits): approvals given under an older policy are void
	local policy_gen = 0
	ctx:on("approval/changed", function()
		policy_gen = policy_gen + 1
	end)
	ctx:on("permission/changed", function()
		policy_gen = policy_gen + 1
	end)

	--- the policy version: policy, sandbox, and host config/plugin reload state
	local function policy_version()
		local reload = mare.reload_version and mare.reload_version() or ""
		local permissions = D.svc.permissions
		local revision = permissions and permissions.policy_revision and permissions:policy_revision() or 0
		local approvals = D.svc.approvals
		local approval_version = approvals and approvals:version() or "disabled"
		return tostring(policy_gen)
			.. ":"
			.. tostring(revision)
			.. ":"
			.. approval_version
			.. ":"
			.. tostring(mare.proc.sandbox.generation())
			.. ":"
			.. reload
			.. ":"
			.. tostring(mare.projection.revision())
			.. ":"
			.. tostring(D.policy:epoch())
	end

	--- canonical serialization for hashing: sorted keys, exact numbers. Not mare.json.encode
	--- (which sorts keys too): encode turns NaN/±inf into `null` (so they'd hash like nil) and
	--- rewrites `properties` next to an `x-mare-order` key, dropping the hint — a change made
	--- after approval in either place would go unnoticed. This keeps every value distinct.
	local function canon(v, out)
		local t = type(v)
		if t == "table" then
			local n = #v
			local keys = {}
			for k in pairs(v) do
				keys[#keys + 1] = k
			end
			if n > 0 and #keys == n then
				out[#out + 1] = "["
				for i = 1, n do
					canon(v[i], out)
					out[#out + 1] = ","
				end
				out[#out + 1] = "]"
			else
				table.sort(keys, function(a, b)
					return tostring(a) < tostring(b)
				end)
				out[#out + 1] = "{"
				for _, k in ipairs(keys) do
					out[#out + 1] = ("%q:"):format(tostring(k))
					canon(v[k], out)
					out[#out + 1] = ","
				end
				out[#out + 1] = "}"
			end
		elseif t == "string" then
			out[#out + 1] = ("%q"):format(v)
		elseif t == "number" then
			out[#out + 1] = math.type(v) == "integer" and tostring(v) or ("%.17g"):format(v)
		else
			out[#out + 1] = tostring(v)
		end
		return out
	end
	local function snapshot_hash(snap)
		return mare.crypto.sha256(table.concat(canon({
			name = snap.name,
			arguments = mare.json.encode_strict(snap.arguments),
			cwd = snap.cwd,
			sandbox = snap.sandbox or false,
			policy = snap.policy_version,
			tool = snap.tool_version,
			schema = snap.schema,
			projection = snap.projection_version,
			inputs = snap.input_policy,
			access = snap.access,
			command = snap.command,
		}, {})))
	end

	--- approval for an `ask` verdict. Decisions:
	---   allow (once) · allow_session · allow_amend (+ `amendment`, a rule the permissions
	---   plugin persists) · deny · abort (deny and end the turn).
	--- tool/approval listeners vote (policy; an RPC host answers via `approval.request`);
	--- without any, an interactive UI asks the user; otherwise (headless) it's `unavailable`.
	local function approve_now(call, info, verdict, cancel)
		if D.policy:has_voters("tool/approval-host") then
			local v = D.policy:decide(
				"tool/approval-host",
				table.pack(call, info, verdict, cancel),
				{ require_explicit_approval = true }
			)
			if v.unavailable then
				return { decision = "unavailable", reason = v.reason, source = v.source or "user" }
			end
			if v.decision == "allow" then
				return {
					decision = v.grant or "allow",
					amendment = v.amendment,
					sandbox_constraints = v.sandboxes,
					permission_constraints = v.permission_constraints,
					scope = v.scope,
					source = v.source or "policy",
					human_approval = v.human_approval,
				}
			end
			return {
				decision = v.abort and "abort" or "deny",
				reason = v.reason or (v.decision == "ask" and "nobody approved the request" or nil),
				source = v.source or "user",
			}
		end
		if mare.ui.connected() then
			-- the answer keys are on the items (the hint bar keeps Ctrl+C / Esc in view)
			local scoped = info.permission_request or verdict.permission_scope ~= nil
			local session_allowed = not call.protected_arguments
				and (not scoped or verdict.permission_scope == "session")
			local choices = { "allow" }
			if session_allowed then
				choices[#choices + 1] = "allow_session"
			end
			if verdict.amendment and not call.protected_arguments then
				choices[#choices + 1] = "allow_amend"
			end
			choices[#choices + 1] = "deny"
			choices[#choices + 1] = "abort"
			-- what the call would change (diff shown in the dialog): a tool's
			-- `preview(arguments) -> node` is shown next to (or under) the choices
			local def = registry[call.name]
			local pok, pv = false, nil
			if def and def.preview then
				pok, pv = pcall(def.preview, call.arguments or {})
			end
			local d, human_approval = mare.ui.approve({
				approval_binding = call.approval_binding,
				approval_arguments = call.arguments,
				tool = tostring(call.name),
				choices = choices,
				amendment_description = verdict.amendment and tostring(verdict.amendment.description or "this rule")
					or nil,
				reason = verdict.reason,
				unconfined = info.unconfined,
				protected_arguments = call.protected_arguments,
				scoped = scoped,
				preview = (pok and pv) or nil,
				cancel = cancel,
				wait_kind = "approval", -- ui/wait-start {kind = "approval"} (status, notify, status hooks)
				wait_title = ("Allow %s?"):format(tostring(call.name)),
			})
			if cancel and cancel:is_cancelled() then
				return {
					decision = "deny",
					reason = "approval cancelled",
					source = "cancelled",
				}
			end
			local answered = d ~= nil
			d = d or "deny"
			local reason = (d == "deny" or d == "abort") and "declined by the user" or nil
			-- an explicit Deny (not Esc, not Ctrl+C) asks why: the model reads the answer in the
			-- tool result and can change course ("No, and tell the model what to do
			-- differently"); Enter on an empty line or Esc skips it
			if d == "deny" and answered then
				local why = mare.ui.input({
					title = "Tell the model why (optional — Enter to skip)",
					placeholder = "e.g. use the staging database instead",
					cancel = cancel,
				})
				if cancel and cancel:is_cancelled() then
					return {
						decision = "deny",
						reason = "approval cancelled",
						source = "cancelled",
					}
				end
				why = type(why) == "string" and mare.text.sanitize(why):match("^%s*(.-)%s*$") or ""
				if why ~= "" then
					reason = "declined by the user: " .. why
				end
			end
			return {
				decision = d,
				amendment = d == "allow_amend" and verdict.amendment or nil,
				reason = reason,
				source = "user",
				human_approval = human_approval,
			}
		end
		-- nobody can answer → `unavailable`, treated as deny and recorded
		return {
			decision = "unavailable",
			reason = "approval required but nobody can answer (headless)",
			source = "none",
		}
	end
	local function granted(d)
		return d == "allow" or d == "allow_session" or d == "allow_amend"
	end
	local function normalize_answer(ans)
		if type(ans) ~= "table" then
			return { decision = "unavailable", reason = "invalid approval answer", source = "error" }
		end
		if ans.abort ~= nil and type(ans.abort) ~= "boolean" then
			return { decision = "deny", reason = "invalid approval abort flag", source = "approval_policy" }
		end
		if ans.unavailable ~= nil and type(ans.unavailable) ~= "boolean" then
			return { decision = "deny", reason = "invalid approval unavailable flag", source = "approval_policy" }
		end
		if ans.abort and ans.unavailable then
			return { decision = "deny", reason = "contradictory approval flags", source = "approval_policy" }
		end
		local d = ans.decision
		if ans.grant ~= nil then
			if d ~= "allow" or not granted(ans.grant) then
				return { decision = "deny", reason = "invalid approval grant", source = "approval_policy" }
			end
			d = ans.grant
		end
		if granted(d) and (ans.abort or ans.unavailable) then
			return { decision = "deny", reason = "contradictory approval answer", source = "approval_policy" }
		end
		if ans.unavailable then
			d = "unavailable"
		elseif ans.abort then
			d = "abort"
		end
		ans.decision = d
		ans.grant = nil
		return ans
	end
	-- one approval at a time: concurrent calls never stack prompts
	local approval_tx, approval_rx = mare.task.channel()
	approval_tx:send(true)
	local function approve(call, info, verdict, custom, cancel, version, declaration, provided_opts)
		if cancel then
			while not approval_rx:try_recv() do
				if not mare.task.sleep(25, cancel) then
					return { decision = "deny", reason = "approval cancelled", source = "cancelled" }
				end
			end
		else
			approval_rx:recv()
		end
		local released, owned = false, true
		local function release()
			if not released then
				released = true
				approval_tx:send(true)
			end
		end
		-- A cancelled Lua task closes this variable, even if the approval callback was yielding.
		local lease <close> = setmetatable({}, {
			__close = function()
				if owned then
					release()
				end
			end,
		})
		if cancel and cancel:is_cancelled() then
			return { decision = "deny", reason = "approval cancelled", source = "cancelled" }
		end
		if version and policy_version() ~= version then
			return { decision = "retry" }
		end
		local req_opts = provided_opts or {}
		req_opts.emit_resolved = false
		local ok, a, human_receipt = pcall(function()
			local copy = require("mare_lib").copy_deep
			-- The compiled declaration is private to the execution pipeline. Policy observers
			-- receive their own call.schema copy and cannot rewrite what the reviewer sees.
			local review_call = copy(call)
			review_call.schema = mare.projection.project(copy(declaration))
			local service = D.svc.approvals
			local policy
			local function check_policy()
				policy =
					D.policy:decide("tool/approval", table.pack(copy(review_call), copy(info), copy(verdict), cancel))
				return policy
			end
			local answer = function(binding)
				call.approval_binding = copy(binding)
				local has_host = D.policy:has_voters("tool/approval-host")
				local review_required = service and service.review_required and service:review_required() == true
				if
					not has_host
					and not review_required
					and policy.decision == "allow"
					and not call.protected_arguments
					and not custom
				then
					return {
						decision = policy.grant or "allow",
						amendment = policy.amendment,
						sandbox_constraints = policy.sandboxes,
						permission_constraints = policy.permission_constraints,
						scope = policy.scope,
						source = policy.source or "policy",
					}
				end
				-- Forward the authoritative schema through ordinary user/RPC/child answerers too.
				call.schema = mare.projection.project(copy(declaration))
				if has_host or not custom then
					return approve_now(copy(call), copy(info), copy(verdict), cancel)
				end
				local ans = normalize_answer(custom(copy(call), copy(info), copy(verdict), cancel))
				return ans
			end
			if not service then
				return { decision = "unavailable", reason = "approval service unavailable", source = "none" }
			end
			return service:request("tool", {
				call = review_call,
				info = info,
				verdict = verdict,
				schema = mare.projection.project(copy(declaration)),
				history = D.svc.agent and D.svc.agent:list_messages().data or {},
			}, check_policy, answer, cancel, req_opts)
		end)
		if cancel and cancel:is_cancelled() then
			return { decision = "deny", reason = "approval cancelled", source = "cancelled" }, nil, req_opts.approval_id
		end
		if not ok then
			return { decision = "deny", reason = tostring(a), source = "error" }, nil, req_opts.approval_id
		end
		return a, human_receipt, req_opts.approval_id
	end

	--- the tool result the model reads for a denied approval: the user's decline and the headless one say not to retry or work
	--- around it; the recorded `reason` stays as it is (events, session, sub-agent hosts)
	local DECLINED, DECLINED_WHY = "declined by the user", "^declined by the user: (.*)$"
	local HEADLESS = "approval required but nobody can answer (headless)"
	local NO_RETRY = "Do not retry it unchanged or work around it; adjust your approach or ask the user."
	local function denied_text(reason)
		reason = tostring(reason or "not approved")
		if reason == HEADLESS then
			return "Tool call denied: approval was required but nobody can answer in this mode;"
				.. " do not retry — report what you could not do."
		elseif reason == DECLINED then
			return "Tool call denied by the user. " .. NO_RETRY
		end
		local why = reason:match(DECLINED_WHY)
		if why then
			why = why:gsub("[%.%s]+$", "")
			return ("Tool call denied by the user: %s. %s"):format(why, NO_RETRY)
		end
		return "Tool call denied: " .. reason
	end

	return {
		policy_version = policy_version,
		snapshot_hash = snapshot_hash,
		approve = approve,
		granted = granted,
		denied_text = denied_text,
	}
end
