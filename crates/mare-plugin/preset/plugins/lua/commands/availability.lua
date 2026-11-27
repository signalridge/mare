-- commands.availability: command availability checks and result normalization;
-- part of the `commands` plugin (plugins/commands.lua).
local M = {}

--- true while the agent runs a turn (the agent service is looked up lazily: no load-order edge)
function M.busy(ctx)
	local ok, a = pcall(ctx.get, ctx, "agent")
	if not ok or not a then
		return false
	end
	local okb, st = pcall(a.activity, a)
	if okb and type(st) == "table" then
		return st.busy == true
	end
	local okb2, b = pcall(a.busy, a)
	return okb2 and b == true
end

--- nil when the session store can be used, else why not ("without a saved session …")
local function store_unavailable(ctx)
	local ok, s = pcall(ctx.get, ctx, "session")
	if not ok or not s or not s.available then
		return "without a session store"
	end
	local oka, avail, why = pcall(s.available, s)
	if not oka then
		return "without a session store"
	end
	return (not avail) and (why or "without a session store") or nil
end

local function conditions(def)
	local a = def.availability
	if a == nil then
		return {}
	end
	return type(a) == "table" and a or { a }
end

--- nil when `def` may run now, else the reason (the notice and the completion use it)
--- `running`: the busy state when the caller already has it
function M.refusal(ctx, def, running)
	for _, c in ipairs(conditions(def)) do
		if c == "idle" then
			if running == nil then
				running = M.busy(ctx)
			end
			if running then
				return "while a turn is running (Esc stops it)", "idle"
			end
		elseif c == "main" and mare.mode == "child" then
			return "in a child agent", "main"
		elseif c == "session" then
			local why = store_unavailable(ctx)
			if why then
				return why, "session"
			end
		elseif c == "running" then
			if running == nil then
				running = M.busy(ctx)
			end
			if not running then
				return "when no turn is running", "running"
			end
		elseif c == "tui" then
			if not (mare.mode == "tui" and mare.ui and mare.ui.connected and mare.ui.connected()) then
				return "outside the terminal UI", "tui"
			end
		end
	end
	return nil
end

function M.refuse(ctx, name, def, should_notify)
	local why = M.refusal(ctx, def)
	if why then
		local msg = ("/%s isn't available %s"):format(name, why)
		if should_notify ~= false then
			ui.notify(msg, "warning")
		end
		return msg
	end
	return nil
end

function M.format_error(v)
	if type(v) == "table" and v.message ~= nil then
		return tostring(v.message)
	end
	return tostring(v)
end

local function has_extra_keys(t, skip)
	for k in pairs(t) do
		if not skip[k] then
			return true
		end
	end
	return false
end

--- Normalize `(res, extra)` returned by a command handler into
--- `(ok, message?, result?, wrap_table_result)` so UI and contract modes share one parser.
local function parse_outcome(res, extra)
	if res == nil then
		if extra ~= nil then
			return false, tostring(extra), nil, false
		end
		return true, nil, nil, false
	end
	if res == false then
		return false, extra ~= nil and tostring(extra) or nil, nil, false
	end
	if res == true then
		if extra == nil then
			return true, nil, nil, false
		end
		if type(extra) == "string" then
			return true, extra, extra, false
		end
		return true, nil, extra, true
	end
	if type(res) == "table" then
		local ok_val
		if res.ok ~= nil then
			ok_val = res.ok == true
		elseif res.kind == "error" then
			ok_val = false
		else
			ok_val = true
		end
		local msg
		if res.message ~= nil then
			msg = tostring(res.message)
		elseif res.error ~= nil then
			msg = tostring(res.error)
		elseif res.text ~= nil then
			msg = tostring(res.text)
		end
		local has_payload = (res.ok == nil and res.kind == nil)
			or has_extra_keys(res, { ok = true, kind = true, message = true, error = true, text = true })
		return ok_val, msg, has_payload and res or nil, false
	end
	if type(res) == "string" then
		return true, res, res, false
	end
	return true, nil, res, false
end

function M.normalize_result(res, extra)
	if res == nil and extra == nil then
		return { ok = true }
	end
	if res == nil and extra ~= nil then
		return { ok = true }
	end
	local ok_val, msg, payload, from_true_extra = parse_outcome(res, extra)
	local out = { ok = ok_val }
	if type(res) == "boolean" then
		if from_true_extra then
			out.result = payload
		else
			out.message = msg
		end
	elseif type(res) == "table" then
		out.message = msg
		out.result = payload
	elseif type(res) == "string" then
		out.message = msg
	else
		out.result = payload
	end
	return out
end

--- Convert a settled `(ok, res, extra)` command outcome into `{ result? }` or raise a coded
--- `commands.command_failed` error (or re-throw an inner coded error).
function M.settle_contract(name, ok, res, extra, should_notify)
	if not ok then
		local msg = "/" .. name .. ": " .. M.format_error(res)
		if should_notify ~= false then
			ui.notify(msg, "error")
		end
		if type(res) == "table" and type(res.code) == "string" then
			error(res, 0)
		end
		error({ code = "commands.command_failed", message = msg }, 0)
	end
	local ok_val, msg, payload = parse_outcome(res, extra)
	if not ok_val then
		error({
			code = "commands.command_failed",
			message = msg or ("command /" .. name .. " failed"),
		}, 0)
	end
	if type(res) == "table" and res.ok == nil and res.kind == nil then
		return { result = res }
	end
	if res == true then
		return extra ~= nil and { result = extra } or {}
	end
	if type(res) == "table" then
		return { result = res }
	end
	return payload ~= nil and { result = payload } or {}
end

return M
