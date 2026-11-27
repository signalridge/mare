-- Execution backends are capabilities, independent of package/plugin names.
return function(D)
	local tools, ctx = D.registry.tools, D.ctx
	local entries, sequence = {}, 0
	local function lookup(exposure)
		local found
		for _, entry in pairs(entries) do
			if entry.active and entry.exposures[exposure] and (not found or found.sequence < entry.sequence) then
				found = entry
			end
		end
		return found
	end
	function tools.register_executor(_, caller, id, definition)
		assert(type(id) == "string" and id ~= "", "executor id is required")
		assert(
			type(definition) == "table" and definition.api_version == 1 and type(definition.run) == "function",
			"executor requires api_version=1 and run"
		)
		local exposures = {}
		for _, name in ipairs(definition.exposures or {}) do
			assert(type(name) == "string" and name ~= "", "invalid executor exposure")
			exposures[name] = true
		end
		assert(next(exposures), "executor requires an exposure")
		sequence = sequence + 1
		local entry = { id = id, sequence = sequence, active = true, exposures = exposures, run = definition.run }
		entries[sequence] = entry
		local function remove()
			if not entry.active then
				return
			end
			entry.active = false
			entries[entry.sequence] = nil
			tools:invalidate(ctx)
			ctx.bus:emit("tools/executors-changed", { executor_id = id, kind = "removed", api_version = 1 })
		end
		remove = require("mare_lib").owned(caller, remove, "executor " .. id)
		tools:invalidate(ctx)
		ctx.bus:emit("tools/executors-changed", { executor_id = id, kind = "registered", api_version = 1 })
		return remove
	end
	function tools.executor(_, caller, exposure)
		local entry = lookup(exposure)
		if not entry then
			return nil
		end
		return {
			id = entry.id,
			api_version = 1,
			run = function(spec)
				if not entry.active or lookup(exposure) ~= entry then
					error("execution capability changed", 0)
				end
				return entry.run(spec, caller)
			end,
		}
	end
end
