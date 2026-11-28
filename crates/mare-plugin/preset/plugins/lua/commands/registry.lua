-- commands.registry: slash-command stack registry and `commands` service;
-- part of the `commands` plugin (plugins/commands.lua).
-- the availability checks run when a command runs, so they load on the first run; registering
-- only needs the condition names
local avail_module = require("mare_lib").lazy(function()
	return require("commands.availability")
end)
local avail = setmetatable({}, {
	__index = function(_, k)
		return avail_module()[k]
	end,
})
local AVAILABILITY = { always = true, idle = true, session = true, main = true, running = true, tui = true }

local M = {}

local function validate_availability(name, def)
	local list = def.availability
	if list ~= nil and type(list) ~= "table" then
		list = { list }
	end
	for _, c in ipairs(list or {}) do
		if not AVAILABILITY[c] then
			error({
				code = "invalid_arguments",
				message = ('command /%s: availability must be "always", "idle", "session", "main", "running" or "tui" (or a list), not %s'):format(
					name,
					tostring(c)
				),
			}, 0)
		end
	end
	if type(list) == "table" and #list == 0 and next(list) ~= nil then
		error({
			code = "invalid_arguments",
			message = ("command /%s: availability must be a list of conditions"):format(name),
		}, 0)
	end
end

local function copy_availability(a)
	if type(a) ~= "table" then
		return a
	end
	local copy = {}
	for i, v in ipairs(a) do
		copy[i] = v
	end
	return copy
end

local function to_info(d)
	local info = { name = d.name }
	if d.description ~= nil then
		info.description = d.description
	end
	if d.args ~= nil then
		info.args = d.args
	end
	if d.auto_execute ~= nil then
		info.auto_execute = d.auto_execute
	end
	if d.local_only ~= nil then
		info.local_only = d.local_only
	end
	if d.availability ~= nil then
		info.availability = copy_availability(d.availability)
	end
	if d.alias_of ~= nil then
		info.alias_of = d.alias_of
	end
	return info
end

local function parse_command_line(line)
	local name, args = string.match(line or "", "^/([^/%s]+)(.*)$")
	if not name or not (args == "" or string.find(args, "^%s")) then
		return nil
	end
	return name, string.match(args, "^%s*(.-)%s*$")
end

function M.create(ctx)
	local cordis = require("cordis")
	local mare_lib = require("mare_lib")
	-- name -> stack of definitions; disposing an override restores the previous command
	local stacks = {}
	local registry = setmetatable({}, {
		__index = function(_, name)
			local st = stacks[name]
			return st and st[#st] or nil
		end,
	})

	local function resolve(name)
		local seen = {}
		local cur = name
		local local_only = false
		while cur do
			if seen[cur] then
				return nil, false
			end
			seen[cur] = true
			local d = registry[cur]
			if not d then
				return nil, false
			end
			local_only = local_only or (d.local_only == true)
			if not d.alias_of then
				return d, local_only
			end
			cur = d.alias_of
		end
		return nil, false
	end

	local function is_remote_caller(caller)
		local cname = mare_lib.caller_name(caller)
		return cname ~= nil and string.sub(cname, 1, 4) == "rpc:"
	end

	-- a mare.Service: inside a method `self.ctx` is the caller, so a command
	-- registration is an effect of the registering plugin's fiber
	local Commands = cordis.class("Commands", mare.Service)
	function Commands:constructor(owner)
		Commands.super.constructor(self, owner, "commands")
	end

	function Commands:register(name, opts)
		if type(name) ~= "string" or name == "" or string.find(name, "[/%s]") then
			error({ code = "invalid_arguments", message = ("command name %q is invalid"):format(tostring(name)) }, 0)
		end
		if type(opts) ~= "table" then
			error({ code = "invalid_arguments", message = ("command /%s: options must be a table"):format(name) }, 0)
		end
		if is_remote_caller(self.ctx) and stacks[name] and #stacks[name] > 0 and opts.replace ~= true then
			error({
				code = "invalid_arguments",
				message = ("command /%s already exists (pass replace = true to replace it)"):format(name),
			}, 0)
		end
		validate_availability(name, opts)
		if opts.alias_of ~= nil then
			if
				type(opts.alias_of) ~= "string"
				or opts.alias_of == ""
				or string.find(opts.alias_of, "[/%s]")
				or opts.alias_of == name
			then
				error({
					code = "invalid_arguments",
					message = ("command /%s: invalid alias_of %q"):format(name, tostring(opts.alias_of)),
				}, 0)
			end
			if opts.run ~= nil then
				error({
					code = "invalid_arguments",
					message = ("command /%s: alias_of cannot be combined with run"):format(name),
				}, 0)
			end
		elseif not mare_lib.callable(opts.run) then
			error({
				code = "invalid_arguments",
				message = ("command /%s: requires `run` or `alias_of`"):format(name),
			}, 0)
		end
		if opts.complete ~= nil and not mare_lib.callable(opts.complete) then
			error({
				code = "invalid_arguments",
				message = ("command /%s: complete must be callable"):format(name),
			}, 0)
		end
		local def = {
			name = name,
			description = opts.description or (opts.alias_of and ("alias of /" .. opts.alias_of) or nil),
			args = opts.args,
			auto_execute = opts.auto_execute,
			local_only = opts.local_only,
			availability = copy_availability(opts.availability),
			alias_of = opts.alias_of,
			run = opts.run,
			complete = opts.complete,
		}
		stacks[name] = stacks[name] or {}
		local st = stacks[name]
		table.insert(st, def)
		-- the release is the effect's disposer: idempotent, the removal runs once
		return self.ctx:effect(function()
			return function()
				local i = table.find(st, def)
				if i then
					table.remove(st, i)
				end
				if #st == 0 and stacks[name] == st then
					stacks[name] = nil
				end
			end
		end, "command /" .. name)
	end

	function Commands:get(name)
		local def = registry[name]
		if not def then
			return nil
		end
		return to_info(def)
	end

	function Commands:list(opts)
		local only_available = type(opts) == "table" and opts.available == true
		local allow_local = type(opts) == "table" and opts.allow_local == true
		local running = only_available and avail.busy(ctx) or nil
		local out = {}
		for _, st in pairs(stacks) do
			local d = st[#st]
			if not d.alias_of then
				local include = true
				if only_available then
					if not allow_local and d.local_only == true then
						include = false
					elseif avail.refusal(ctx, d, running) ~= nil then
						include = false
					end
				end
				if include then
					out[#out + 1] = to_info(d)
				end
			end
		end
		table.sort(out, function(a, b)
			return a.name < b.name
		end)
		return { data = out }
	end

	local function dispatch_line(caller_ctx, line, input, opts)
		local ignore_non_cmd = opts and opts.ignore_non_command
		local allow_local = opts and opts.allow_local
		local return_outcome = opts and opts.return_outcome
		local notify = not is_remote_caller(caller_ctx)
		local name, args = parse_command_line(line)
		if not name or not args then
			if ignore_non_cmd then
				return false
			end
			local msg = string.sub(line or "", 1, 1) == "/" and "invalid command syntax" or "commands start with `/`"
			error({ code = "invalid_arguments", message = msg }, 0)
		end
		-- In-process Lua callers (`Commands:run`) can still run `/quit`, `/exit`, `/q`, and
		-- `/status` without waiting on `exec_gate` / `reload_barrier` (RPC requests wait at
		-- `serve.rs` `reload_settled` before entering Lua; remote diagnostics use `status.get`).
		local reload_exempt = name == "quit" or name == "exit" or name == "q" or name == "status"
		if not allow_local and not reload_exempt then
			local failure = mare.task.session_error()
			if failure then
				error({ code = "unavailable", message = failure }, 0)
			end
			local kernel = ctx:get("kernel")
			if kernel then
				kernel:reload_barrier()
			end
		end
		local entry = registry[name]
		local target, chain_local_only = resolve(name)
		if not entry or not target then
			local msg = "Unknown command: /" .. name
			if notify then
				ui.notify(msg, "warning")
			end
			if return_outcome then
				return true, { ok = false, message = msg }
			end
			error({ code = "commands.unknown_command", message = msg }, 0)
		end
		if not allow_local and chain_local_only then
			error({
				code = "commands.local_only",
				message = "this command is only available in the terminal UI",
			}, 0)
		end
		local why = avail.refuse(ctx, name, target, notify)
		if why then
			if return_outcome then
				return true, { ok = false, message = why }
			end
			error({ code = "commands.command_unavailable", message = why }, 0)
		end
		local ok, res, extra = pcall(target.run, args, input)
		if return_outcome then
			if not ok then
				local msg = "/" .. name .. ": " .. avail.format_error(res)
				ui.notify(msg, "error")
				return true, { ok = false, message = msg }
			end
			return true, avail.normalize_result(res, extra)
		end
		return avail.settle_contract(name, ok, res, extra, notify)
	end

	function Commands:run(line, input, opts)
		local target_sid = opts and opts.session_id
		if target_sid ~= nil then
			if type(target_sid) ~= "string" or not target_sid:match("%S") then
				error({ code = "invalid_arguments", message = "session_id must not be empty" }, 0)
			end
			local session = ctx:get("session")
			local info = session and session.info and session:info() or nil
			local cur_sid = info and info.session_id or nil
			if cur_sid == nil or target_sid ~= cur_sid then
				error({ code = "unknown_session", message = "unknown session: " .. target_sid }, 0)
			end
		end
		return dispatch_line(self.ctx, line, input, nil)
	end

	--- TUI runner: allows `local_only` (e.g. /lua), ignores non-commands (`false`), and
	--- returns `{ ok, message?, result? }` for UI submit and `/commands` browser handling.
	local function run_local(line, input)
		return dispatch_line(nil, line, input, {
			ignore_non_command = true,
			allow_local = true,
			return_outcome = true,
		})
	end

	local commands = Commands.new(ctx)
	return {
		commands = commands,
		stacks = stacks,
		registry = registry,
		resolve = resolve,
		run_local = run_local,
	}
end

return M
