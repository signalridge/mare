-- The mare event bus on the cordis tree: the dispatch
-- semantics of mare's events over the root's one events service, the same bus native plugins
-- listen on. The Rust emitters (Kernel::emit*, bail*, waterfall*) check a blocked turn, record the event and feed the taps (headless/RPC event stream), then
-- dispatch here; nothing else dispatches mare events.
--
--   bus.emit(event,...) -> failures every listener in order, awaited; one failing
--                                                       does not stop the rest (returned, reported by Rust)
--   bus.emit_checked(event,...) -> nil | plugin, err the first failure stops the rest
--   bus.bail(event,...) -> value, failures the first non-nil result; a failing
--                                                       listener is reported and the next asked
--   bus.waterfall(event, value,...) -> value | nil, failures each listener is
--                                                       `function(value,..., next)` and passes on with
--                                                       `next(value,...)`; a failure keeps `value`
--   bus.waterfall_checked(event, value,...) -> value | nil, plugin, err
--
-- A listener's result is ignored by `emit` (a notification, never a bail). Policy points do not go
-- through the bus: they are the policy service's (mare_kernel/plugins/policy.lua).
local cordis = require("cordis")
local future = require("cordis.future")

-- the plugin a hook belongs to: its registrant's fiber
local function owner(hook)
	local ctx = hook and hook.ctx
	local fiber = ctx and ctx.fiber
	return fiber and fiber.name or "?"
end

local function message(err)
	return tostring(err)
end

-- one listener: it runs in the context it was registered in (a listener registered
-- under a restricted grant or a tool's sandbox stays restricted, whoever emits)
local function invoke(host, hook, args, ...)
	local all = table.pack(table.unpack(args, 1, args.n))
	for i = 1, select("#", ...) do
		all.n = all.n + 1
		all[all.n] = (select(i, ...))
	end
	return future.runSync(host, hook.scope, hook.callback, table.unpack(all, 1, all.n))
end

local EV_MT = { __metatable = "mare.EventEnvelope" }

local function copy_ev(ev, event, nextFn)
	local out = {
		type = (ev and ev.type) or event,
		session_id = ev and ev.session_id or nil,
		session_epoch = ev and ev.session_epoch or nil,
		seq = ev and ev.seq or nil,
		turn_id = ev and ev.turn_id or nil,
	}
	if nextFn ~= nil then
		out.next = nextFn
		return setmetatable(out, {
			__metatable = "mare.EventEnvelope",
			__call = function(_, ...)
				return nextFn(...)
			end,
		})
	end
	return setmetatable(out, EV_MT)
end

return function(root, prims)
	local bus = {}
	local events = root.events
	local host = root.host
	-- Cache traced method closures once: `events` is a traced service and lives for the tree's life.
	local resolve = events.resolve
	local listeners = events.listeners
	local listenerCount = events.listenerCount

	local timed = prims and prims.events and prims.events.timed
	local envelope_of = prims and prims.events and prims.events.envelope

	local function default_ev(event, arg)
		if envelope_of then
			return envelope_of(event, arg)
		end
		return { type = event }
	end

	-- one listener call on `event`, awaited and timed like the engine's
	-- (mare.profile callback timings under the event's own name)
	local function call(event, hook, args, ...)
		local extra = table.pack(...)
		local function run()
			return cordis.await(invoke(host, hook, args, table.unpack(extra, 1, extra.n)))
		end
		if not timed then
			return run()
		end
		return timed(event, owner(hook), run)
	end

	local function listener_args(args)
		if args.n <= 1 then
			return { n = 1, [1] = args[1] }
		end
		return args
	end

	-- every listener in order, each awaited, its result ignored (a notification, never a bail).
	-- A failure goes to `onError(err, hook)`: true goes on with the rest, false stops and raises.
	local function sequence(ev, onError, event, ...)
		local _, hooks, _, args = resolve(events, "serial", event, ...)
		local payload = listener_args(args)
		for _, hook in ipairs(hooks) do
			if not hook.removed then
				local ok, err = pcall(function()
					call(event, hook, payload, copy_ev(ev, event))
				end)
				if not ok and not onError(err, hook) then
					error(err, 0)
				end
			end
		end
	end

	-- the plugins listening to `event`, in dispatch order (diagnostics, order tests)
	function bus.owners(event)
		local out = {}
		for _, hook in ipairs(listeners(events, event)) do
			if not hook.removed then
				out[#out + 1] = owner(hook)
			end
		end
		return out
	end

	-- Whether dispatching `event` would reach nobody: no live listener on the tree (of the event,
	-- or of every mare event through `internal/dispatch`) and no tap
	-- (RPC / headless event stream) taking it. Then a native `ctx.bus:emit` / `waterfall` is a no-op
	-- (plugins/bus.lua) and never enters Rust: not recorded, not tapped, as when a plugin skipped
	-- the dispatch for want of listeners. The taps' answer is cached per event name while every tap
	-- filter is fixed (Rust clears it when a tap is added); with a filter that reads live state (an
	-- RPC connection's subscription), it is asked each time.
	local tapped, dynamic = {}, prims.events.taps_dynamic and prims.events.taps_dynamic() or false
	function bus.taps_changed(is_dynamic)
		tapped, dynamic = {}, is_dynamic == true
	end
	function bus.quiet(event)
		-- cordis drops an event's key with its last listener (events.lua `unregister`); a mare
		-- event also reaches every `internal/dispatch` listener (events.lua `resolve`)
		if
			listenerCount(events, event) > 0
			or (event:sub(1, 9) ~= "internal/" and listenerCount(events, "internal/dispatch") > 0)
		then
			return false
		end
		if dynamic then
			return not prims.events.tapped(event)
		end
		local t = tapped[event]
		if t == nil then
			t = prims.events.tapped(event)
			tapped[event] = t
		end
		return not t
	end

	-- how many live listeners an event has on the tree (the one count the Rust `listener_count`
	-- reads)
	function bus.listener_count(event)
		local n = 0
		for _, hook in ipairs(listeners(events, event)) do
			if not hook.removed then
				n = n + 1
			end
		end
		return n
	end

	function bus.emit_ev(ev, event, ...)
		local failures = {}
		sequence(ev, function(err, hook)
			failures[#failures + 1] = { owner(hook), message(err) }
			return true
		end, event, ...)
		return failures
	end

	function bus.emit(event, ...)
		return bus.emit_ev(default_ev(event, (...)), event, ...)
	end

	function bus.emit_checked_ev(ev, event, ...)
		local plugin = "?"
		local ok, err = pcall(sequence, ev, function(_, hook)
			plugin = owner(hook)
			return false
		end, event, ...)
		if ok then
			return nil
		end
		return plugin, message(err)
	end

	function bus.emit_checked(event, ...)
		return bus.emit_checked_ev(default_ev(event, (...)), event, ...)
	end

	-- the first answer that is not nil (or null) wins, `false` included (a refusal); a failing listener is reported (in the
	-- returned failures) and the next one asked, as on the old engine
	function bus.bail(event, ...)
		return bus.bail_each(nil, event, ...)
	end

	-- `before()` runs ahead of every listener (Rust: re-binding a persistence record to its
	-- session, so an earlier listener cannot strip it from a later one)
	function bus.bail_each_ev(ev, before, event, ...)
		local _, hooks, _, args = resolve(events, "bail", event, ...)
		local payload = listener_args(args)
		local failures = {}
		for _, hook in ipairs(hooks) do
			if not hook.removed then
				if before then
					before()
				end
				local ok, result = pcall(call, event, hook, payload, copy_ev(ev, event))
				if not ok then
					failures[#failures + 1] = { owner(hook), message(result) }
				elseif result ~= nil and result ~= cordis.null then
					return result, failures
				end
			end
		end
		return nil, failures
	end

	function bus.bail_each(before, event, ...)
		return bus.bail_each_ev(default_ev(event, (...)), before, event, ...)
	end

	-- around form: `function(data, ev)`; `ev.next(data?)` (or calling `ev(data?)`) passes on to
	-- the rest of the chain; the first registered is outermost; past the last listener the value is
	-- the result.
	local function waterfall(ev, failed, checked, event, ...)
		local _, hooks, _, args = resolve(events, "waterfall", event, ...)
		local scope = host.capture and host.capture() or nil
		local index = 0
		local dispatch
		dispatch = function(current)
			index = index + 1
			while hooks[index] and hooks[index].removed do
				index = index + 1
			end
			local hook = hooks[index]
			if not hook then
				return current[1]
			end
			local called, rest = false, nil
			local function nextFn(...)
				if called then
					error("next() called multiple times", 2)
				end
				called = true
				local passed = select("#", ...) > 0 and table.pack(...) or current
				if scope then
					rest = scope.run(dispatch, passed)
				else
					rest = dispatch(passed)
				end
				return rest
			end
			local ok, result = pcall(call, event, hook, listener_args(current), copy_ev(ev, event, nextFn))
			if not ok then
				if checked then
					failed.owner = failed.owner or owner(hook)
					error(result, 0)
				end
				failed.list[#failed.list + 1] = { owner(hook), message(result) }
				if called then
					return rest
				end
				return dispatch(current)
			end
			return result
		end
		return dispatch(args)
	end

	function bus.waterfall_ev(ev, event, value, ...)
		local failed = { list = {} }
		local ok, result = pcall(waterfall, ev, failed, false, event, value, ...)
		if not ok then
			failed.list[#failed.list + 1] = { "?", message(result) }
			result = nil
		end
		return result, #failed.list > 0 and failed.list or nil
	end

	function bus.waterfall(event, value, ...)
		return bus.waterfall_ev(default_ev(event, value), event, value, ...)
	end

	function bus.waterfall_checked_ev(ev, event, value, ...)
		local failed = {}
		local ok, result = pcall(waterfall, ev, failed, true, event, value, ...)
		if ok then
			return result
		end
		return nil, failed.owner or "?", message(result)
	end

	function bus.waterfall_checked(event, value, ...)
		return bus.waterfall_checked_ev(default_ev(event, value), event, value, ...)
	end

	return bus
end
