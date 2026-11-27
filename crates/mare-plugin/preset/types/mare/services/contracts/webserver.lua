---@meta _
-- The `webserver` service contract: host-level HTTP/WebSocket
-- listener status, dynamic route registrations, and static plugin web asset mounts (`web = { dir }`).
local S = ...
local object, integer, string, boolean, list_of, map = S.object, S.integer, S.string, S.boolean, S.list_of, S.map

local route_info = object({
	route_id = string,
	method = string,
	path = string,
	owner_session_id = string,
	plugin = string,
}, { "route_id", "method", "path" })

local asset_info = object({
	asset_id = string,
	prefix = string,
	dir = string,
	plugin = string,
	index = string,
	fallback = string,
}, { "asset_id", "prefix", "dir" })

local http_method = {
	type = "string",
	enum = { "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "ANY" },
}

return {
	["webserver"] = {
		doc = "Host-level HTTP/WebSocket listener, named routes, and static plugin asset mounts.",
		errors = {
			unknown_route = { doc = "no route with that route_id" },
			route_conflict = { doc = "a route for that method and path is already registered" },
			invalid_route = { doc = "route path must start with `/` and must not collide with reserved endpoints" },
			route_limit_reached = { doc = "maximum number of registered HTTP routes reached" },
			asset_conflict = { doc = "an asset mount already uses that URL prefix" },
			invalid_asset_dir = { doc = "asset directory is missing, not a directory, or escapes its root" },
		},
		methods = {
			get = {
				doc = "Status and bound endpoints of the host webserver listener.",
				expose = "both",
				access = "peer",
				scope = "host",
				order = {},
				result = object({
					running = boolean,
					url = string,
					ws_url = string,
					host = string,
					port = integer,
					tls = boolean,
					trusted_hosts = { type = "array", items = string },
				}, { "running" }),
			},
			list_routes = {
				doc = "Registered HTTP routes on the host webserver.",
				expose = "both",
				access = "peer",
				scope = "host",
				order = {},
				result = list_of(route_info),
			},
			list_assets = {
				doc = "Registered static asset mounts on the host webserver.",
				expose = "both",
				access = "peer",
				scope = "host",
				order = {},
				result = list_of(asset_info),
			},
			register_route = {
				doc = "Register a named HTTP route on the host webserver. Requests are forwarded to the caller handler.",
				expose = "both",
				access = "primary",
				admin = true,
				scope = "host",
				registration = true,
				host_local = true,
				params = object({
					path = string,
					method = http_method,
				}, { "path" }),
				order = { "path", "..." },
				callbacks = {
					handle = {
						type = "fun(req: table): table",
						params = object({
							req = object({
								method = string,
								path = string,
								query = map(string),
								headers = map(string),
								body = string,
							}, { "method", "path" }, true),
						}, { "req" }),
						order = { "req" },
						result = object({
							status = integer,
							headers = map(string),
							body = string,
						}, { "status" }, true),
					},
				},
				errors = { "route_conflict", "invalid_route", "route_limit_reached" },
			},
			register_assets = {
				doc = "Register a static asset directory served under a URL prefix on the host webserver.",
				expose = "both",
				access = "primary",
				admin = true,
				scope = "host",
				registration = true,
				host_local = true,
				params = object({
					dir = string,
					prefix = string,
					index = string,
					fallback = string,
				}, { "dir" }),
				order = { "dir", "..." },
				errors = { "asset_conflict", "invalid_asset_dir" },
			},
		},
	},
}
