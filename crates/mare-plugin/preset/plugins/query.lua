-- Capability-based query providers. A callback always retains its calling execution context.
return {
	name = "query",
	provide = { "query" },
	-- `bus` for query/changed
	inject = { "bus" },
	apply = function(ctx)
		local cordis = require("cordis")
		local providers = {}
		-- a mare.Service: inside a method `self.ctx` is the caller, so a provider
		-- registration is an effect of the caller's fiber
		local Query = cordis.class("Query", mare.Service)
		function Query:constructor(owner)
			Query.super.constructor(self, owner, "query")
		end
		local function changed()
			ctx.bus:emit("query/changed", {})
		end
		local function select(operation, name)
			local best
			for id, p in pairs(providers) do
				if (not name or id == name) and p.operations[operation] then
					if not best or p.priority > best.priority or (p.priority == best.priority and id < best.name) then
						best = p
					end
				end
			end
			return best
		end
		function Query:register(name, def)
			-- a registration changes host state: refused in a read-only (ephemeral) execution, as the
			-- release is (agent activity checks both the same way)
			mare.exec.check_mutation("query provider registration")
			assert(type(name) == "string" and name:match("^[%w_.-]+$"), "invalid query provider name")
			assert(not providers[name], "query provider already registered: " .. name)
			assert(type(def) == "table" and type(def.operations) == "table", "query provider requires operations")
			local operations = {}
			for operation, execute in pairs(def.operations) do
				assert(type(operation) == "string" and type(execute) == "function", "invalid query operation")
				operations[operation] = execute
			end
			assert(next(operations), "query provider has no operations")
			local source = def.source or { capability = "opaque" }
			assert(
				type(source) == "table" and (source.capability == "in_process" or source.capability == "opaque"),
				"invalid query source capability"
			)
			local priority = def.priority or 0
			assert(type(priority) == "number" and priority % 1 == 0, "invalid query provider priority")
			local p = {
				name = name,
				generation = mare.crypto.uuid(),
				operations = operations,
				priority = priority,
				source = { capability = source.capability },
				active = {},
			}
			providers[name] = p
			local function remove()
				if providers[name] == p then
					providers[name] = nil
					for token in pairs(p.active) do
						token:cancel()
					end
					changed()
				end
			end
			local dispose = self.ctx:effect(function()
				return remove
			end, "query provider " .. name)
			changed()
			-- The release is the caller's action, checked in the caller's execution context: the
			-- disposer itself runs in the registrant's, so a check there would let a
			-- read-only (ephemeral) execution unregister. Unload disposes without a check.
			return function()
				mare.exec.check_mutation("query provider unregister")
				dispose()
			end
		end

		function Query:list_providers()
			local data = mare.json.array({})
			for id, p in pairs(providers) do
				local operations = mare.json.array({})
				for operation in pairs(p.operations) do
					operations[#operations + 1] = operation
				end
				table.sort(operations)
				data[#data + 1] = {
					name = id,
					revision = p.generation,
					priority = p.priority,
					operations = operations,
					source = table.copy(p.source),
				}
			end
			table.sort(data, function(a, b)
				return a.name < b.name
			end)
			return { data = data }
		end
		function Query:supports(operation)
			local provider = select(operation)
			return provider ~= nil and provider.source.capability == "in_process"
		end
		function Query:call(operation, args, opts)
			local lib = require("mare_lib")
			assert(type(operation) == "string" and type(args) == "table", "query requires operation and arguments")
			opts = opts or {}
			local caller_cancel = opts.cancel and opts.cancel()
			local p = select(operation, opts.provider)
			if not p then
				lib.fail("unavailable", "QUERY_UNAVAILABLE: no provider supports this operation")
			end
			if p.source.capability ~= "in_process" then
				lib.fail(
					"unavailable",
					"QUERY_CAPABILITY_UNAVAILABLE: opaque query callbacks cannot be constrained by this runtime"
				)
			end
			local request = table.copy(args)
			if request.cursor then
				local ok, cursor = pcall(mare.json.decode, request.cursor)
				if
					not ok
					or type(cursor) ~= "table"
					or cursor.operation ~= operation
					or cursor.provider ~= p.name
					or cursor.generation ~= p.generation
				then
					lib.fail("query.stale_cursor", "QUERY_STALE_CURSOR: provider changed; restart query")
				end
				request.cursor = cursor.cursor
			end
			if caller_cancel and caller_cancel:is_cancelled() then
				lib.fail("cancelled", "QUERY_CANCELLED: query cancelled")
			end
			local token = caller_cancel and caller_cancel:child() or mare.task.token()
			p.active[token] = true
			local lease <close> = setmetatable({}, {
				__close = function()
					p.active[token] = nil
					token:cancel()
				end,
			})
			local call_opts = table.copy(opts)
			call_opts.cancel = token
			local current = mare.exec.current()
			local ok, result = pcall(mare.exec.run, {
				cwd = (current and current.cwd) or mare.cwd,
				lifetime = "ephemeral-read-only",
				sandbox = { level = "read-only", network = false, accept_partial = mare.json.array({}) },
			}, function()
				return p.operations[operation](request, call_opts)
			end)
			if not ok then
				error(result, 0)
			end
			if caller_cancel and caller_cancel:is_cancelled() then
				lib.fail("cancelled", "QUERY_CANCELLED: query cancelled")
			end
			if providers[p.name] ~= p then
				lib.fail("query.provider_changed", "QUERY_PROVIDER_CHANGED: query provider was unloaded")
			end
			assert(type(result) == "table", "query provider returned a non-table result")
			result = table.copy(result)
			if result.cursor ~= nil then
				lib.fail("invalid_result", "query providers must return next_cursor, not cursor")
			end
			local next_cursor = result.next_cursor
			if next_cursor then
				result.next_cursor = mare.json.encode({
					operation = operation,
					provider = p.name,
					generation = p.generation,
					cursor = next_cursor,
				})
			end
			result.provider = p.name
			return result
		end
		Query.new(ctx)
	end,
}
