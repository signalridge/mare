-- A plugin definition as a native cordis component. `component(def, origin)` gives the
-- component the loader applies: the
-- plugin's own owner record (what it provides, registers and emits is attributed to it, closed with
-- its fiber), its lua/ directory, the kernel's active set, and the apply's outcome for the failure
-- and reload reports:
--   errors[name] the last apply error (cleared by a successful apply)
--   applies[name] successful applies (a fiber keeps its uid across reloads: this is how a restarted
--                  dependent shows up in the reload report's `applied`)
-- `inject` is the definition's: every name is required (an optional service is ctx:inject({ name }, fn)).
local cordis = require("cordis")
local calls = require("mare_kernel.calls")

local function isIter(v)
	local mt = type(v) == "table" and getmetatable(v)
	local name = type(mt) == "table" and rawget(mt, "__name")
	return name == "cordis.Iter" or name == "cordis.AsyncIter"
end

local function loadingChildren(parentFiber, rootCtx, childIdx)
	local idx = childIdx or calls.child_index(rootCtx)
	if not idx[parentFiber] then
		return nil
	end
	local out
	calls.each_descendant(rootCtx, parentFiber, function(fiber)
		if fiber.inertia and calls.is_child_component(fiber) then
			if not out then
				out = { fiber }
			else
				out[#out + 1] = fiber
			end
		end
	end)
	return out
end

return function(prims, rootCtx, rowFibers)
	local errors, applies = {}, {}
	local childIdx = rootCtx and calls.child_index(rootCtx) or nil

	local function component(def, origin)
		local apply = def.apply
		return {
			name = def.name,
			inject = def.inject,
			provide = def.provide,
			apply = function(ctx, config)
				-- the plugin's lua/ dir, again on every apply: the previous fiber's unload dropped it
				-- together with its modules, possibly after this component was built
				if prims.module_dir then
					prims.module_dir(def.name)
				end
				local fiber = ctx.fiber
				if rowFibers then
					rowFibers[fiber] = def.name
				end
				local owner = prims.owner.open(def.name, origin or "builtin")
				-- the row's fiber acts for this owner from now on (calls.owners: a remote service
				-- called during the apply already sees its caller)
				calls.owners[fiber] = owner
				local active = false
				local ownerEffect = ctx:effect(function()
					return function()
						-- Mark inactive before closing owner and unloading modules.
						if active then
							active = false
							prims.mark_active(def.name, nil)
						end
						calls.owners[fiber] = nil
						prims.owner.close(owner)
						-- its modules go with it (the next apply requires their current files)
						if prims.unload_modules then
							prims.unload_modules(def.name)
						end
					end
				end, "owner")
				-- the kernel's bookkeeping, not the plugin's: left out of the stall report
				calls.kernel_effects[rawget(ownerEffect, cordis.Context.effect)] = true
				local function activate()
					if calls.inactive(fiber) then
						return
					end
					applies[def.name] = (applies[def.name] or 0) + 1
					-- the kernel's active set follows the fiber
					if prims.mark_active then
						active = true
						prims.mark_active(def.name, owner, config)
					end
				end
				local resolved_origin = origin or "builtin"
				local result
				if apply then
					errors[def.name] = nil
					local ok, value
					if prims.measure_apply then
						local busy_ms, yielded, reqs
						busy_ms, yielded, reqs, ok, value = prims.measure_apply(function(...)
							return pcall(prims.owner.enter, owner, apply, ...)
						end, ctx, config)
						local did_yield = yielded or (ok and cordis.future.isThenable(value))
						if prims.profile_record then
							prims.profile_record(
								string.format(
									"plugin %s (%s) [%s, req=%d]",
									def.name,
									resolved_origin,
									did_yield and "yielded" or "sync",
									reqs or 0
								),
								busy_ms
							)
						end
					elseif prims.profile_record then
						local t0 = os.clock()
						local p0 = prims.poll_count and prims.poll_count() or 0
						ok, value = pcall(prims.owner.enter, owner, apply, ctx, config)
						local ms = (os.clock() - t0) * 1000
						local p1 = prims.poll_count and prims.poll_count() or 0
						local did_yield = (p1 > p0) or (ok and cordis.future.isThenable(value))
						prims.profile_record(
							did_yield and string.format("plugin %s (%s) [yielded]", def.name, resolved_origin)
								or string.format("plugin %s (%s)", def.name, resolved_origin),
							ms
						)
					else
						ok, value = pcall(prims.owner.enter, owner, apply, ctx, config)
					end
					if not ok then
						errors[def.name] = calls.error_message(value)
						error(value, 0)
					end
					result = value
				elseif prims.profile_record then
					if prims.measure_apply then
						prims.profile_record(
							string.format("plugin %s (%s) [sync, req=0]", def.name, resolved_origin),
							0
						)
					else
						prims.profile_record(string.format("plugin %s (%s)", def.name, resolved_origin), 0)
					end
				end
				local root = rootCtx or ctx[cordis.Context.root]
				local children = loadingChildren(fiber, root, childIdx)
				if (not children and not cordis.future.isThenable(result)) or isIter(result) then
					activate()
					return result
				end
				-- Await an async apply or in-flight child fibers (`ctx:plugin(S)` inside `apply`)
				-- before marking this plugin active: a failing child rolls back and fails this row.
				return cordis.future.AsyncFunction
					.spawn(
						cordis.async(function()
							local value = result
							if cordis.future.isThenable(value) then
								value = cordis.await(value)
							end
							while true do
								local pending = loadingChildren(fiber, root, childIdx)
								if not pending then
									break
								end
								for _, child in ipairs(pending) do
									child:await():await()
								end
							end
							activate()
							return value
						end),
						ctx.host
					)
					:catch(function(err)
						errors[def.name] = calls.error_message(err)
						error(err, 0)
					end)
			end,
		}
	end

	return { component = component, errors = errors, applies = applies }
end
