-- statusline.external: external statusLine command runner and status segment;
-- part of the `statusline` plugin (plugins/statusline.lua).
local M = {}

local function build_payload(ctx, S)
	local m = ctx.llm:current()
	local usage_service = ctx:get("usage")
	local used = usage_service and usage_service:context_tokens() or 0
	local window = m and m.context_window or nil
	local session_service = ctx:get("session")
	local info = session_service and session_service:info() or {}
	local totals = usage_service and usage_service:totals() or {}
	local status_service = ctx:get("status")
	return mare.json.encode({
		session_id = info.session_id,
		transcript_path = info.path,
		cwd = mare.cwd,
		width = mare.ui.width and mare.ui.width() or nil,
		model = m and { id = m.id, name = m.name, ref = m.ref } or nil,
		context = {
			used = used,
			window = window,
			percent = window and window > 0 and math.floor(used * 100 / window) or nil,
		},
		cost = totals.cost,
		git_branch = S.branch,
		-- the unified agent state: state, message, session, seq
		status = status_service and status_service:get() or nil,
	})
end

local function resolve_command(command)
	if type(command) == "table" then
		return command[1], { table.unpack(command, 2) }
	end
	if mare.os == "windows" then
		local comspec = mare.env("COMSPEC")
		if type(comspec) ~= "string" or comspec:match("^%s*$") then
			comspec = "cmd.exe"
		end
		return comspec, { "/C", command }
	end
	return "sh", { "-c", command }
end

local function warn_once(ctx, key, msg, state)
	if state.last_error == key then
		return
	end
	state.last_error = key
	if ctx.logger and ctx.logger.warn then
		ctx.logger:warn(msg)
	elseif mare.log then
		mare.log("warn", msg)
	end
end

function M.create(ctx, cfg, S)
	local running = false
	local rerun = false
	local diag = { last_error = nil }
	return function()
		if not cfg.command then
			return
		end
		if running then
			rerun = true
			return
		end
		running = true
		ctx:spawn(function()
			local _reset <close> = setmetatable({}, {
				__close = function()
					running = false
					rerun = false
				end,
			})
			repeat
				rerun = false
				local payload = build_payload(ctx, S)
				local cmd, args = resolve_command(cfg.command)
				local ok, r = pcall(mare.proc.run, {
					cmd = cmd,
					args = args,
					input = payload,
					timeout_ms = cfg.command_timeout_ms,
					max_output = 4096,
				})
				if not ok then
					local err = tostring(r)
					warn_once(
						ctx,
						"spawn:" .. tostring(cmd) .. ":" .. err,
						("statusline command failed to launch (%s): %s"):format(tostring(cmd), err),
						diag
					)
				elseif r and not r.timed_out and r.code == 0 then
					diag.last_error = nil
					local line = string.match(r.stdout or "", "^([^\n]*)") or ""
					S.external_text = mare.text.sanitize(line)
					ctx.ui:invalidate("external")
					ctx.ui:invalidate("left")
					ctx.ui:invalidate("right")
				end
			until not rerun
		end)
	end
end

return M
