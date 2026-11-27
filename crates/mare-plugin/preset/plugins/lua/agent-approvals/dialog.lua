-- agent-approvals.dialog: the parent's dialog for a sub-agent's approval —
-- the same choices as the tools service's own dialog, titled with the sub-agent and its task —
-- and the text of the rule it offers to remember. Presentation only: the host decides when it
-- is asked (one at a time, only with a UI and no `tool/approval` listener), keeps the rule it
-- stores (this gets a copy) and builds the vote from the `choice` returned here.
local M = {}

--- a rule field as it is shown: a string as written, anything else as JSON
local function show(v)
	return type(v) == "string" and v or mare.json.encode(v)
end

--- The rule's text, from its own fields (never the child's description, which the host strips):
--- what it matches (a command prefix or a tool), the decision, the scope it widens to, where it
--- applies and the arguments it excludes.
function M.describe(rule)
	local function s(k)
		local v = rule[k]
		if v ~= nil then
			return show(v)
		end
	end
	local function words(k)
		local out = {}
		if type(rule[k]) == "table" then
			for i, v in ipairs(rule[k]) do
				out[i] = show(v)
			end
		end
		return out
	end
	local argv = words("argv")
	local what
	if #argv > 0 then
		what = "commands that start with `" .. table.concat(argv, " ") .. "`"
	elseif s("tool") then
		what = "`" .. s("tool") .. "` calls"
	else
		return "this rule: " .. mare.json.encode(rule)
	end
	local d = s("decision")
	local text
	if d == "allow" then
		text = "don't ask again for " .. what
	elseif d == "prompt" then
		text = "always ask before " .. what
	elseif d == "forbidden" then
		text = "always refuse " .. what
	else
		text = (d or "(no decision)") .. " " .. what
	end
	if s("scope") then
		text = text .. ", with `" .. s("scope") .. "` access"
	end
	if #argv > 0 then
		text = text .. (s("cwd") and (", in " .. s("cwd")) or ", in any directory")
		local deny = words("deny_args")
		if #deny > 0 then
			text = text .. " (not with " .. table.concat(deny, " ") .. ")"
		end
	end
	local expires = s("expires")
	if expires and expires ~= "never" then
		text = text .. (expires == "session" and ", for this sub-agent's session" or (", expires " .. expires))
	end
	return text
end

--- `agent/approval-dialog {call, info, verdict, rule?}` → `{choice?, reason?}`: an explicit Deny
--- asks why, and the reason reaches the child's tool result; no choice = dismissed.
function M.ask(ev)
	local call, info, verdict = ev.call or {}, ev.info, ev.verdict
	local rule = ev.rule and M.describe(ev.rule)
	local o = call.origin or {}
	-- the source agent: label, the run it
	-- belongs to (a workflow's run / phase) and its task
	local who = tostring(o.label or o.agent_id or "sub-agent")
	if o.run_id then
		who = who .. " · run " .. tostring(o.run_id)
	end
	if o.phase then
		who = who .. " · " .. tostring(o.phase)
	end
	if o.task then
		who = who .. " · " .. tostring(o.task)
	end
	local scoped = (info and info.permission_request) or (verdict and verdict.permission_scope ~= nil)
	local session_allowed = not call.protected_arguments
		and (not scoped or (verdict and verdict.permission_scope == "session"))
	local choices = { "allow" }
	if session_allowed then
		choices[#choices + 1] = "allow_session"
	end
	-- the proposed rule as its own fields say (never the child's description)
	if rule and not call.protected_arguments then
		choices[#choices + 1] = "allow_amend"
	end
	choices[#choices + 1] = "deny"
	choices[#choices + 1] = "abort"
	-- an explicit Deny (not Esc, not Ctrl+C) asks why, as the main approval box does: the child's
	-- model reads it in the tool result; Enter on an empty line or Esc skips it
	local function why()
		local w = mare.ui.input({
			title = "Tell the sub-agent why (optional — Enter to skip)",
			placeholder = "e.g. use the staging database instead",
			cancel = ev.cancel,
		})
		w = type(w) == "string" and mare.text.sanitize(w):match("^%s*(.-)%s*$") or ""
		return w ~= "" and w or nil
	end
	local choice, human_approval = mare.ui.approve({
		approval_binding = call.approval_binding,
		approval_arguments = call.arguments,
		tool = tostring(call.name),
		choices = choices,
		amendment_description = rule,
		origin = o,
		reason = verdict and verdict.reason,
		unconfined = info and info.unconfined,
		protected_arguments = call.protected_arguments,
		scoped = scoped,
		cancel = ev.cancel,
		wait_kind = "approval",
		wait_title = ("[%s] Allow %s?"):format(who, tostring(call.name)),
	})
	if ev.cancel and ev.cancel:is_cancelled() then
		return { choice = "deny", reason = "approval cancelled" }
	end
	if choice == "deny" then
		return { choice = "deny", reason = why() }
	end
	return { choice = choice, human_approval = human_approval }
end

return M
