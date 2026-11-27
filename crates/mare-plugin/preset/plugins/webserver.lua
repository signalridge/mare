-- webserver: host-level HTTP/WebSocket registry service.

return {
	name = "webserver",
	tags = { "minimal" },
	provide = { "webserver" },
	apply = function(ctx)
		local lib = require("mare_lib")
		local service = {}

		function service.get(_, _)
			return mare.webserver.get()
		end

		function service.list_routes(_, _)
			return { data = mare.webserver.list_routes() }
		end

		function service.list_assets(_, _)
			return { data = mare.webserver.list_assets() }
		end

		function service.register_route(_, caller, path, opts)
			local owner_name = lib.caller_name(caller)
			return lib.owned(
				caller,
				mare.webserver.register_route(owner_name, path, opts),
				"webserver route " .. tostring(path)
			)
		end

		function service.register_assets(_, caller, dir, opts)
			local owner_name = lib.caller_name(caller)
			return lib.owned(
				caller,
				mare.webserver.register_assets(owner_name, dir, opts),
				"webserver assets " .. tostring(dir)
			)
		end

		lib.service(ctx, "webserver", service)
	end,
}
