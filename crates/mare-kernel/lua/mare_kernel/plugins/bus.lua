-- Host plugin `bus`: the one entry for a native plugin's mare events.
--
--   bus:emit(event,...) every listener in order, a failure reported
--   bus:emit_checked(event,...) the first failure raises
--   bus:bail(event,...) -> answer the first non-nil answer
--   bus:waterfall(event, value,...) -> value (a failure keeps `value`)
--   bus:waterfall_checked(event, value,...) -> value (a failure raises)
--
-- Each goes through the Rust entry (prims.events.dispatch -> Kernel::emit* / bail* / waterfall*):
-- the blocked-turn check, the event record and the taps (the RPC / headless event streams, with
-- must_deliver backpressure, which a synchronous `internal/dispatch` hook could not wait for), then
-- the bus (mare_kernel/bus.lua) with mare's semantics. Rule: a plugin emits a mare event (on the
-- public event list) through `ctx.bus`; bare `ctx:emit` / `serial` / `waterfall` only for its own
-- in-process events.
--
-- A plain cordis service, not a mare.Service: its methods run in the caller's scope, so the emitter
-- is the calling plugin (prims.events.emitter), and the event's arguments pass through as they are
-- (a contract has no variadic parameters).
local cordis = require("cordis")
local calls = require("mare_kernel.calls")

local BusService = cordis.class("BusService", cordis.Service)

local bus_owners = setmetatable({}, { __mode = "k" })

local function resolve_bus_owner(bus_self)
	if type(bus_self) ~= "table" then
		return nil, false
	end
	local cached = bus_owners[bus_self]
	if cached ~= nil then
		if cached == false then
			return nil, true
		end
		return cached, true
	end
	local ctx = bus_self.ctx
	if type(ctx) ~= "table" or not cordis.Context.is(ctx) then
		return nil, false
	end
	local fiber = calls.caller_fiber(ctx)
	local resolved = false
	for f in calls.fibers_up(fiber) do
		local owner = calls.owners[f]
		if owner ~= nil then
			resolved = owner
			break
		end
	end
	bus_owners[bus_self] = resolved
	if resolved == false then
		return nil, true
	end
	return resolved, true
end

-- The modes whose dispatch to nobody is a plain no-op: nothing listens on the tree and no tap
-- takes the event (bus.lua `bus.quiet`), so the call returns at once — an emit nothing, a
-- waterfall its value unchanged — without dispatching through Rust; only the Rust entry's emitter
-- check is kept (a closed owner cannot emit). A plugin emits unconditionally; the bus pays for it
-- only when someone is there (a stream chunk's provider/event costs nothing unobserved). `bail`
-- `bail` and `emit_checked` always go through the Rust entry.
local FAST = { emit = true, waterfall = true, waterfall_checked = true }

function BusService:constructor(ctx, config)
	cordis.Service.constructor(self, ctx, "bus")
	-- The five methods close over the bus's internals: the service shows plugins nothing else (no
	-- listener query), and each consumer receives its own shadow so the dispatch check verifies the
	-- injecting plugin's owner identity cleanly via cached `bus_owners[proxy]`.
	local dispatch, check_dispatch, quiet, emitter_live =
		config.prims.events.dispatch,
		config.prims.events.check_dispatch,
		config.bus.quiet,
		config.prims.events.emitter_live
	for _, mode in ipairs({ "emit", "emit_checked", "bail", "waterfall", "waterfall_checked" }) do
		local fast = FAST[mode]
		rawset(self, mode, function(proxy, event, ...)
			if type(event) ~= "string" then
				error(("bus:%s: the event name must be a string"):format(mode), 2)
			end
			if check_dispatch then
				local owner, known = resolve_bus_owner(proxy)
				check_dispatch(mode, event, owner, known)
			end
			if fast and quiet(event) then
				emitter_live()
				if mode == "emit" then
					return
				end
				return (...)
			end
			return dispatch(mode, event, ...)
		end)
	end
end

return BusService
