-- permissions.approvals: the tool/before listener, approval results (caches, increments, rules)
-- and hints after a denial; part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	-- mare.Ctx still types `policy` as the transition layer's method
	local policy = ctx.policy --[[@as mare.PolicyService]]
	local P, child, copy = D.P, D.child, D.copy
	local shared, record, persist_rule = D.shared, D.record, D.persist_rule
	local LIKE, LEVEL = D.presets.LIKE, D.presets.LEVEL
	local inc_narrow, inc_union = D.escalation.inc_narrow, D.escalation.inc_union
	-- the vote module loads on the first vote, not while this module registers
	local function vote(...)
		return D.vote.vote(...)
	end

	-- the agent stores the last vote per call so the result handler knows the cache key
	local pending = setmetatable({}, { __mode = "k" })
	local function pending_key(call)
		if type(call) == "table" and type(call.execution_id) == "string" and call.execution_id ~= "" then
			return call.execution_id .. "\0" .. tostring(call.attempt or 0) .. "\0" .. tostring(call.snapshot or "")
		end
		return (type(call) == "table" and call.id) or call
	end
	policy:voter("tool/before", function(call, info)
		if not P.sandbox_ready then
			return {
				decision = "deny",
				reason = P.sandbox_error or "permission sandbox could not be restored",
			}
		end
		shared.refresh() -- rules another session or this session's parent saved since
		local v = vote(call, info or {})
		v.tool_version = info and info.tool_version
		v.info = info
		-- other plugins (org policy…) vote on the verdict: a policy event, so they can only
		-- tighten it (allow → ask → deny); an error, timeout or invalid vote counts as deny
		--
		local extra = policy:decide(
			"permission/verdict",
			table.pack({ call = call, info = info, preset = P.preset, verdict = copy(v) })
		)
		if extra.decision == "deny" then
			v = { decision = "deny", reason = extra.reason, abort = extra.abort }
			record("result", {
				tool = call.name,
				command = D.rules.command_text(call, info),
				decision = "deny",
				source = "permission/verdict",
				reason = v.reason,
			})
		elseif extra.decision == "ask" and v.decision ~= "deny" then
			v.decision = "ask"
			local reason = extra.reason or "a policy plugin asks before this call"
			v.reason = v.reason and (v.reason .. " · " .. reason) or reason
			v.key = v.key
				or (
					call.name
					.. "@"
					.. tostring(info and info.tool_version or "")
					.. "|="
					.. tostring(D.rules.command_text(call, info) or "")
				)
		end
		if v.key or v.request or v.permissions or v.amendment then
			if type(call.execution_id) == "string" and (call.attempt or 0) > 1 then
				-- Retrying a vote must not revive an approval from an ended turn.
				for key, earlier in pairs(pending) do
					if earlier.call and earlier.call.execution_id == call.execution_id then
						v.turn_ended = v.turn_ended or earlier.turn_ended
						pending[key] = nil
					end
				end
			end
			v.revision = D.policy_revision()
			v.call = copy(call)
			pending[pending_key(call)] = v
		end
		local categories = copy(v.categories or {})
		for _, category in ipairs(extra.categories or {}) do
			categories[#categories + 1] = category
		end
		-- the kernel validates votes strictly: only the vote fields go out
		return {
			decision = v.decision,
			category = v.decision == "ask" and v.category or nil,
			categories = v.decision == "ask" and categories or nil,
			reason = v.reason,
			abort = v.decision == "deny" and v.abort or nil,
			sandbox_grant = v.decision ~= "deny" and v.sandbox or nil,
			sandbox_constraints = v.decision ~= "deny" and extra.sandboxes or nil,
			amendment = v.decision ~= "deny" and v.amendment or nil,
		}
	end)

	-- Switching independent policy/reviewer revokes remembered transient approvals.
	-- Explicit persisted rules remain user configuration, rather than cached decisions.
	ctx:on("approval/changed", function()
		D.reset_session_grants(true)
	end)
	-- Only the pipeline can commit a structured grant, after snapshot/cancellation checks.
	D.describe_permissions = function(call, expected)
		local v = pending[pending_key(call)]
		if not v or not v.permissions then
			return nil
		end
		assert(expected and v.binding == expected, "frozen permission request changed during policy evaluation")
		return D.request.clean(copy(v.request or v.increment or v.permissions)), v.scope
	end
	D.resolve_permissions = function(call, constraints, scope, expected)
		local v = pending[pending_key(call)]
		if not v or not (v.request or v.permissions) then
			return nil
		end
		if v.turn_ended and (v.permission_request or v.scope == "turn" or scope == "turn") then
			pending[pending_key(call)] = nil
			error("permission request outlived its turn")
		end
		if v.permissions then
			assert(expected and v.binding == expected, "frozen permission request changed during policy evaluation")
		end
		local granted, sandbox, lifetime = D.request.resolve(v, constraints, scope)
		v.resolved_permissions, v.resolved_scope = copy(granted), lifetime
		v.resolved_revision, v.resolved_generation = D.policy_revision(), mare.proc.sandbox.generation()
		return granted, sandbox, lifetime
	end
	D.commit_permissions = function(call)
		local v = pending[pending_key(call)]
		if not v or not v.permissions or not v.resolved_permissions then
			return
		end
		if
			(v.turn_ended and (v.permission_request or v.scope == "turn" or v.resolved_scope == "turn"))
			or v.resolved_revision ~= D.policy_revision()
			or v.resolved_generation ~= mare.proc.sandbox.generation()
		then
			v.resolved_permissions = nil
			pending[pending_key(call)] = nil
			return false
		end
		local granted = D.request.clean(inc_narrow(v.increment or v.permissions, v.resolved_permissions))
		local scope = v.resolved_scope
		if scope then
			local fresh = D.escalation.inc_remainder(granted, P.approved[scope])
			if fresh then
				P.approved[scope] = inc_union(P.approved[scope], fresh)
			end
		end
		v.committed_permissions, v.committed_scope = granted, scope
		v.resolved_permissions = nil -- a receipt commits only once
		return true
	end

	local function invalidate_context()
		local context = D.svc.context
		if context then
			context:invalidate()
		end
	end
	-- Audit after both Lua and host scope stores committed without yielding.
	ctx:on("tool/authorized", function(ev)
		local key = pending_key(ev.call)
		local v = pending[key]
		if type(key) == "string" then
			pending[key] = nil
		end
		if not v or not v.committed_permissions then
			return
		end
		local granted, scope = v.committed_permissions, v.committed_scope
		v.committed_permissions = nil
		record("grant", { tool = ev.call.name, permissions = granted, scope = scope })
		invalidate_context()
	end)

	-- approvals for "this turn" end with the turn
	ctx:on("turn/completed", function()
		local had_turn_grants = next(P.approved.turn) ~= nil
		for k, v in pairs(pending) do
			-- Keep permission receipts long enough to reject answers arriving after this turn.
			if type(k) == "string" and not (v.request or v.permissions) then
				pending[k] = nil
			else
				v.turn_ended = true
			end
		end
		if not had_turn_grants then
			return
		end
		D.bump_policy_revision()
		P.approved.turn = {}
		invalidate_context()
	end)
	local function reset_pending()
		for k in pairs(pending) do
			pending[k] = nil
		end
	end
	ctx:on("session/new", reset_pending)
	ctx:on("session/loaded", reset_pending)

	local function current_pending(v, call)
		shared.refresh()
		if v.revision ~= D.policy_revision() or not v.call or call.snapshot ~= v.call.snapshot then
			return false
		end
		local ok, same = pcall(function()
			if call.access == nil or v.call.access == nil then
				return call.access == nil and v.call.access == nil
			end
			return mare.json.encode_strict(call.access) == mare.json.encode_strict(v.call.access)
		end)
		if not ok or not same then
			return false
		end
		if v.info and v.info.access_state == "declared" then
			for _, item in ipairs(v.info.access or {}) do
				local resolved_ok, path = pcall(mare.fs.resolve_path, item.logical)
				if
					not resolved_ok
					or path.logical ~= item.logical
					or path.physical ~= item.physical
					or path.identity ~= item.identity
				then
					return false
				end
			end
		end
		return true
	end

	ctx:on("pipeline/approval-decided", function(ev)
		local call, d = ev.call, ev.decision
		local key = call and pending_key(call)
		local v = key and pending[key]
		if
			type(key) == "string"
			and not (
				v
				and (v.permissions or v.request)
				and (d == "allow" or d == "allow_session" or d == "allow_amend")
			)
		then
			pending[key] = nil
		end
		local approved_permissions = v and v.resolved_permissions or ev.permissions
		record("result", {
			tool = call and call.name,
			command = call and D.rules.command_text(call, v and v.info),
			decision = d,
			source = ev.source,
			permissions = ev.permissions,
			scope = ev.scope,
		})
		if not (d == "allow" or d == "allow_session" or d == "allow_amend") then
			P.streak = { key = nil, n = 0 }
			return
		end
		local own = not (call and call.origin)
		if own and v and not current_pending(v, call) then
			pending[key] = nil
			return
		end
		if
			v
			and v.key
			and not v.permissions
			and (d == "allow_session" or d == "allow_amend")
			and ev.scope ~= "turn"
			and (
				not v.request
				or not approved_permissions
				or not D.escalation.inc_remainder(v.request, approved_permissions)
			)
		then
			P.session_cache[v.key] = true
			if v.original_key then
				P.session_cache[v.original_key] = true
			end
		end
		-- approved escalation scopes, per kind: the approval is
		-- intersected with the request first; "allow" alone covers this call only
		-- `all` (unsandboxed) is never stored: it would turn one approved command into a
		-- full-access session; repeating that exact command is covered by the session key
		if v and not v.permissions and v.request and not v.request.all then
			local granted = inc_narrow(v.request, approved_permissions)
			local scope = ev.scope or ((d == "allow_session" or d == "allow_amend") and "session" or nil)
			if scope == "session" then
				P.approved.session = inc_union(P.approved.session, granted)
			elseif scope == "turn" then
				P.approved.turn = inc_union(P.approved.turn, granted)
			end
			if scope then
				invalidate_context()
			end
		end
		-- "always allow": a remembered rule goes into
		-- the shared table, written by the top-level session — for a sub-agent's call too (its
		-- request reaches us with `call.origin`), never from a child VM — so the parent, the
		-- other sub-agents and later sessions all see it. A sub-agent's rule is the one it
		-- proposed, exactly as the parent's dialog showed it: the host hands that object over
		-- once, by the token of the answer (persisting the approved amendment as proposed); a rule
		-- arriving any other way is not saved. A session-scoped rule stays with the agent whose
		-- call it was, like "allow for this session".
		if not (v and v.permissions) and d == "allow_amend" and type(ev.amendment) == "table" then
			local rule = copy(ev.amendment)
			rule.description = nil
			if own then
				local proposed = v and v.amendment and copy(v.amendment)
				if proposed then
					proposed.description = nil
				end
				local ok, same = pcall(function()
					return proposed and mare.json.encode_strict(proposed) == mare.json.encode_strict(rule)
				end)
				if not ok or not same then
					return
				end
			end
			if rule.expires == "session" then
				if own then
					rule.tool_version = tostring(v and v.tool_version or "")
					D.bump_policy_revision()
					P.session_rules[#P.session_rules + 1] = rule
				end
			elseif not child then
				if not own then
					shared.refresh()
					rule = mare.subagent and mare.subagent.take_rule and mare.subagent.take_rule(ev.rule_token) or nil
				end
				if rule then
					persist_rule(rule)
				end
			end
		end
		-- consecutive approvals of the same kind: offer a wider preset (with 7-day snooze)
		local kind = (call and call.arguments and call.arguments.sandbox_permissions) or (call and call.name)
		if P.streak.key == kind then
			P.streak.n = P.streak.n + 1
		else
			P.streak = { key = kind, n = 1 }
		end
		if P.streak.n >= cfg.suggest_after and LIKE[P.preset] ~= "full-access" and mare.ui.connected() then
			P.streak.n = 0
			local SNOOZE_KEY = "permission_suggest_snooze_until"
			local SNOOZE_SECONDS = 7 * 86400
			local ok_st, st = pcall(mare.state.read)
			local snooze_until = ok_st and type(st) == "table" and tonumber(st[SNOOZE_KEY]) or 0
			if snooze_until > os.time() or P.suggesting_preset then
				return
			end
			local target = LIKE[P.preset] == "read-only" and "auto" or "full-access"
			local ceiling = child and mare.args and mare.args.permission_ceiling
			if not mare.trusted or (ceiling and D.presets.exceeds_ceiling(target, ceiling)) then
				return
			end
			P.suggesting_preset = true
			ctx:spawn(function()
				local ok, choice = pcall(mare.ui.select, {
					title = ("You approved %d similar requests — switch to a wider preset?"):format(
						cfg.suggest_after
					),
					items = {
						{
							label = "Switch to " .. target,
							detail = D.presets.DESC[target] or "Use a wider permission preset",
						},
						{
							label = "Keep " .. P.preset,
							detail = "Ask again next time",
						},
						{
							label = "Not now — don't ask for 7 days",
							detail = "Snooze this suggestion for 7 days",
						},
					},
				})
				P.suggesting_preset = false
				if not ok then
					return
				end
				if choice == 1 then
					local switched, err = D.presets.set_preset(target, { source = "user", confirm = true })
					if not switched and err ~= "cancelled" then
						mare.ui.notify("permission: " .. tostring(err), "warning")
					end
				elseif choice == 3 then
					D.lib.save_state(ctx, SNOOZE_KEY, os.time() + SNOOZE_SECONDS, true)
				end
			end)
		end
	end)

	---------------------------------------------------------------------------
	-- hints after a denial: targeted only with reliable evidence
	---------------------------------------------------------------------------
	local function after_tool(res, call)
		local key = call and pending_key(call)
		if type(key) == "string" then
			pending[key] = nil
		end
		if type(res) ~= "table" or LIKE[P.preset] == "full-access" then
			return res
		end
		local evidence = res.sandbox_denials or (res.details and res.details.sandbox_denials)
		local text = type(res.content) == "string" and res.content or nil
		if not text then
			return res
		end
		local hint
		if type(evidence) == "table" and #evidence > 0 then
			local d = evidence[1]
			local scope
			if d.path and d.sandbox_denied == "read" then
				hint = ("[sandbox: read of %s denied under %s mode. Request extra read access with request_permissions, additional_permissions = %s, and a justification. This does not override deny_read. The call may have partly run — check the actual state first.]"):format(
					d.path,
					LEVEL[P.preset] or "custom",
					mare.json.encode({ read = { d.path } })
				)
			elseif d.host or d.sandbox_denied == "network" then
				scope = d.host and ("network:" .. d.host) or "network"
			elseif d.path then
				-- the parent directory, `/` or `\` separated (Windows paths)
				local dir = tostring(d.path):match("^(.*)[/\\][^/\\]*$") or d.path
				scope = "write:" .. dir
			end
			-- what was refused, by name: "write to /etc/x", "network access to example.com"
			local what = d.host and ("network access to " .. d.host)
				or d.path and (d.sandbox_denied == "read" and "read of " or "write to ") .. d.path
				or "access"
			if scope then
				hint = ("[sandbox: %s denied under %s mode. To run it with more access, call the tool again with sandbox_permissions = %q and a justification. It may have partly run — check the actual state first.]"):format(
					what,
					LEVEL[P.preset] or "custom",
					scope
				)
			end
		elseif
			res.is_error
			and (
				text:find("Permission denied", 1, true)
				or text:find("Operation not permitted", 1, true)
				-- Windows (cmd, native tools) when the output names no path
				or text:find("Access is denied", 1, true)
			)
		then
			hint = "[this may be related to the sandbox ("
				.. (LEVEL[P.preset] or "custom")
				.. " mode); the cause is uncertain]"
		end
		if hint then
			res.content = text .. "\n" .. hint
		end
		return res
	end
	-- `tool/after` is an around waterfall (`{ result, call, operation }`, `ev.next(data)`):
	-- the hinted result goes on to the later listeners.
	ctx:on("tool/after", function(data, ev)
		data.result = after_tool(data.result, data.call)
		return ev.next(data)
	end)
end
