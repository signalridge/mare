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

function M.create(ctx, cfg, S)
	local running = false
	local rerun = false
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
				local cmd, args
				if type(cfg.command) == "table" then
					cmd, args = cfg.command[1], { table.unpack(cfg.command, 2) }
				else
					cmd, args = "sh", { "-c", cfg.command }
				end
				local ok, r = pcall(mare.proc.run, {
					cmd = cmd,
					args = args,
					input = payload,
					timeout_ms = cfg.command_timeout_ms,
					max_output = 4096,
				})
				if ok and r and not r.timed_out and r.code == 0 then
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
