-- proc: caller-owned sandboxed piped and PTY child processes (`proc.{spawn,write,resize,kill}`).
local function raise_if_err(ok, code, msg)
	if ok then
		return
	end
	if type(code) == "string" and code ~= "" then
		local full = (code == "invalid_arguments" or code == "denied") and code or ("proc." .. code)
		require("mare_lib").fail(full, tostring(msg or code))
	end
	error(tostring(msg or "proc error"), 0)
end

return {
	name = "proc",
	provide = { "proc" },
	apply = function(ctx)
		local lib = require("mare_lib")
		local registry = lib.owner_registry(ctx, function(mgr)
			mgr:close_all()
		end, "proc")
		ctx:inject({ "session" }, function(sub)
			if sub.session.on_drain then
				sub.session:on_drain(registry.close_all)
			end
		end)

		local function call_mgr(caller, method, opts)
			assert(type(opts) == "table", "proc." .. method .. " expects an options table")
			local mgr = registry.get(caller)
			if not mgr then
				lib.fail("proc.unknown_process", "unknown process_id `" .. tostring(opts.process_id) .. "`")
			end
			local ok, code, err = mgr[method](mgr, opts)
			raise_if_err(ok, code, err)
			return {}
		end

		local service = {}

		function service.spawn(_, caller, opts)
			assert(type(opts) == "table", "proc.spawn expects an options table")
			local mgr = registry.get(caller, mare.proc.manager)
			local ok, res, err = mgr:spawn(opts)
			raise_if_err(ok ~= nil, res, err)
			return ok
		end

		function service.write(_, caller, opts)
			return call_mgr(caller, "write", opts)
		end

		function service.resize(_, caller, opts)
			return call_mgr(caller, "resize", opts)
		end

		function service.kill(_, caller, opts)
			return call_mgr(caller, "kill", opts)
		end

		lib.service(ctx, "proc", service)
	end,
}
