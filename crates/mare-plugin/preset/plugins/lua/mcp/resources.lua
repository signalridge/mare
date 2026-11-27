-- mcp.resources: the list/read MCP resource tools;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local cfg = D.cfg
	local servers = D.servers
	local order, blocked = D.config.order, D.config.blocked
	-- result shaping loads on the first resource read
	local function limit_text(...)
		return D.results.limit_text(...)
	end
	local function images_supported(...)
		return D.results.images_supported(...)
	end
	local client_of = D.connection.client_of
	local ctx, has_tier = D.ctx, D.discovery.has_tier
	local resource_defs = {} -- name → tool definition

	-- resources: the widest tier among the
	-- servers decides whether they're declared or found with tool_search
	local function resource_servers(only)
		local out = {}
		for _, n in ipairs(order) do
			local e = servers[n]
			if (not only or only == n) and not blocked(e) and e.tier ~= "hidden" then
				out[#out + 1] = e
			end
		end
		return out
	end
	local function each_resource_server(server, fn)
		local list = resource_servers(server)
		if server and #list == 0 then
			error("no usable MCP server named " .. tostring(server))
		end
		local errors = {}
		local succeeded = 0
		for _, e in ipairs(list) do
			local ok, err = pcall(client_of, e.name)
			if ok then
				succeeded = succeeded + 1
				fn(e)
			elseif server then
				error(err, 0)
			else
				errors[#errors + 1] = { server = e.name, error = tostring(err) }
			end
		end
		if #errors > 0 and succeeded == 0 then
			local parts = {}
			for _, er in ipairs(errors) do
				parts[#parts + 1] = ("%s: %s"):format(er.server, er.error)
			end
			error(table.concat(parts, "; "), 0)
		end
		return errors
	end
	resource_defs.list_mcp_resources = {
		description = "List resources offered by MCP servers (JSON: server, uri, name, mimeType …). "
			.. "Without `server`, lists every server's resources.",
		parameters = {
			type = "object",
			properties = { server = { type = "string", description = "Only this MCP server" } },
		},
		side_effect = "read",
		unconfined = true,
		execute = function(a)
			local out = {}
			local ok, err_or_errors = pcall(each_resource_server, a.server, function(e)
				for _, r in ipairs(e.resources or {}) do
					local x = table.copy(r)
					x.server = e.name
					x.icons = nil
					if not tostring(x.uri or ""):match("^ui://") then
						out[#out + 1] = x
					end
				end
			end)
			if not ok then
				return { content = tostring(err_or_errors), is_error = true }
			end
			return {
				content = mare.json.encode({
					server = a.server,
					resources = out,
					errors = #err_or_errors > 0 and err_or_errors or nil,
				}),
			}
		end,
	}
	resource_defs.list_mcp_resource_templates = {
		description = "List resource URI templates offered by MCP servers (JSON). Without `server`, lists "
			.. "every server's templates.",
		parameters = {
			type = "object",
			properties = { server = { type = "string", description = "Only this MCP server" } },
		},
		side_effect = "read",
		unconfined = true,
		execute = function(a)
			local out = {}
			local ok, err_or_errors = pcall(each_resource_server, a.server, function(e)
				for _, r in ipairs(e.templates or {}) do
					local x = table.copy(r)
					x.server = e.name
					out[#out + 1] = x
				end
			end)
			if not ok then
				return { content = tostring(err_or_errors), is_error = true }
			end
			return {
				content = mare.json.encode({
					server = a.server,
					resourceTemplates = out,
					errors = #err_or_errors > 0 and err_or_errors or nil,
				}),
			}
		end,
	}
	resource_defs.read_mcp_resource = {
		description = "Read a resource from an MCP server by its URI.",
		parameters = {
			type = "object",
			properties = {
				server = { type = "string", description = "The MCP server" },
				uri = { type = "string", description = "The resource URI" },
			},
			required = { "server", "uri" },
		},
		side_effect = "read",
		unconfined = true,
		execute = function(a)
			local ok, r = pcall(function()
				if not servers[a.server] then
					error("no MCP server named " .. tostring(a.server))
				end
				local client = client_of(a.server)
				local e = servers[a.server]
				return client:read_resource(
					tostring(a.uri),
					{ timeout_ms = e.srv.call_timeout_ms or cfg.call_timeout_ms }
				)
			end)
			if not ok then
				return { content = ("MCP %s: %s"):format(tostring(a.server), tostring(r)), is_error = true }
			end
			local parts = mare.json.array()
			for _, c in ipairs(type(r.contents) == "table" and r.contents or {}) do
				if c.text then
					parts[#parts + 1] = { type = "text", text = limit_text(c.text) }
				elseif c.blob and tostring(c.mimeType or ""):match("^image/") and images_supported() then
					parts[#parts + 1] = { type = "image", data = c.blob, mime = c.mimeType }
				elseif c.blob then
					local bok, bytes = pcall(mare.crypto.base64_decode, c.blob)
					if not bok then
						return {
							content = ("MCP %s: failed to decode binary resource %s: %s"):format(
								tostring(a.server),
								tostring(c.uri),
								tostring(bytes)
							),
							is_error = true,
						}
					end
					local sok, path = pcall(mare.mcp.spill, bytes, "bin")
					if not sok or type(path) ~= "string" or path == "" then
						return {
							content = ("MCP %s: failed to save binary resource %s: %s"):format(
								tostring(a.server),
								tostring(c.uri),
								tostring(path)
							),
							is_error = true,
						}
					end
					parts[#parts + 1] = {
						type = "text",
						text = ("[binary resource %s (%s) saved to %s]"):format(
							tostring(c.uri),
							tostring(c.mimeType or "?"),
							path
						),
					}
				end
			end
			if #parts == 0 then
				parts[1] = { type = "text", text = "(empty resource)" }
			end
			return { content = parts }
		end,
	}
	if next(servers) then
		local exposure = has_tier("direct") and "direct" or has_tier("deferred") and "deferred" or nil
		if exposure then
			for rname, def in pairs(resource_defs) do
				def = table.copy(def)
				def.exposure = exposure == "deferred" and "deferred" or nil
				ctx.tools:register(rname, def)
			end
		end
	end
end
