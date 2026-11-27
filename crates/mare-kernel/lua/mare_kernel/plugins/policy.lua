-- Host plugin `policy`: security
-- decisions are not events. A decision point's voters and rewriters register with the service (an
-- effect of the registrant's fiber: they go with it), and the service that owns the point decides:
--
--   policy:voter(point, fn, opts?) -> dispose opts: { timeout_ms?, slot?, prepend? }
--   policy:rewriter(point, fn, opts?) -> dispose opts: { timeout_ms?, prepend? }
--   policy:decide(point, args, opts?) -> decision args: the decision's arguments (a list);
--                                                   opts: { require_explicit_approval? }
--   policy:rewrite(point, call) -> arguments (raises `policy.rewrite_refused` on failure)
--
-- The fold is Rust's (dispatch/policy.rs, `prims.policy`): first deny stops; an error, timeout,
-- cancellation or invalid vote is a deny; votes are parsed strictly; arguments are frozen before
-- voting. Order: the host's voters (Kernel::host_voter) first, then the plugins' in registration order,
-- `prepend` first among the plugins'; one voter per decision slot (the first
-- registered of a slot decides). A voter runs as its registrant (owner and execution context).
local cordis = require("cordis")

local function method(doc, properties, order, extra)
	local m = {
		doc = doc,
		expose = "lua",
		params = { type = "object", properties = properties or {}, additionalProperties = false },
		order = order or {},
	}
	for k, v in pairs(extra or {}) do
		m[k] = v
	end
	return m
end

local REGISTER_OPTS = {
	timeout_ms = { type = "number" },
	slot = { type = "string" },
	prepend = { type = "boolean" },
}

local function contract(prims)
	-- Validate point names dynamically inside methods against live prims.policy.{points,rewrite_points}
	-- so plugins loaded/reloaded via merge_defs after kernel startup work too.
	local POINT = { type = "string" }
	local REWRITE_POINT = { type = "string" }
	return {
		doc = "Security decision points: registered voters and rewriters, and the fail-closed decision.",
		errors = {
			rewrite_refused = { doc = "a rewriter refused, timed out, or returned an invalid arguments table" },
		},
		methods = {
			voter = method(
				"register a voter for a decision point; returns its dispose",
				(function()
					local p = { point = POINT }
					for k, v in pairs(REGISTER_OPTS) do
						p[k] = v
					end
					return p
				end)(),
				{ "point", "fn", "..." },
				{ lua = { fn = "fun(...): any" }, registration = true }
			),
			rewriter = method(
				"register a rewriter for a rewrite point; returns its dispose",
				{ point = REWRITE_POINT, timeout_ms = { type = "number" }, prepend = { type = "boolean" } },
				{ "point", "fn", "..." },
				{ lua = { fn = "fun(call: table): table?" }, registration = true }
			),
			-- decide / epoch / has_voters are the point owner's own check, made inside the operation it
			-- guards (Lua-only; no RPC caller reaches them). Registering a voter or rewriter stays
			-- closed to a peer's work unless allowed in-process.
			decide = method(
				"run the point's voters over the arguments and fold their votes",
				{ point = POINT },
				{ "point", "args", "options" },
				{
					lua = { args = "any[]", options = "{require_explicit_approval?: boolean}?" },
					lua_unchecked = true,
					result = { type = "object", additionalProperties = true },
				}
			),
			epoch = method(
				"the policy epoch: it moves when what a decision would answer may change",
				nil,
				nil,
				{ result = { type = "integer" } }
			),
			has_voters = method("whether any voter (or rewriter) takes part in a point", {
				point = { type = "string" },
			}, { "point" }, { result = { type = "boolean" } }),
			rewrite = method(
				"run the point's rewriters over the call and return the rewritten arguments",
				{ point = REWRITE_POINT },
				{ "point", "call" },
				{
					lua = { call = "table" },
					lua_unchecked = true,
					result = { type = "object", additionalProperties = true },
					errors = { "rewrite_refused" },
				}
			),
		},
	}
end

return function(prims, Service, bus)
	local Policy = cordis.class("PolicyService", Service)

	local attach

	function Policy:constructor(ctx)
		local owner = prims.owner.open("policy", "root")
		ctx:effect(function()
			return function()
				prims.owner.close(owner)
			end
		end, "owner")
		self.voters = {} -- [point] = { entry, ... }
		self.rewriters = {}
		self.seq = 0
		prims.owner.enter(owner, Policy.super.constructor, self, ctx, "policy", { contract = contract(prims) })
		attach(self, ctx)
	end

	-- Whether a registration is the host's own: its voters form the front segment, ahead of every
	-- plugin's. The one place this is decided (not an option a plugin can pass): the registrant's
	-- fiber is the host's (`<root>`, mare_kernel/host_fiber.lua: Kernel::host_voter).
	local function from_host(caller)
		return bus ~= nil and bus.is_host_fiber ~= nil and bus.is_host_fiber(caller.fiber)
	end

	-- an entry in a point's list, as an effect of the caller's fiber (`self.ctx` is the caller): it goes with the registrant
	local function register(self, lists, point, fn, opts)
		-- every voter / rewriter registration passes here (policy:voter / rewriter, the rpc
		-- service's remote voters and rewrite hooks): only unconfined work registers one
		-- (prims.policy.unconfined)
		prims.policy.unconfined(("a %s on `%s`"):format(lists == self.voters and "voter" or "rewriter", point))
		opts = opts or {}
		local caller = self.ctx
		local owner = prims.owner.current()
		-- a voter runs in the context it was registered in (registered under a
		-- restricted grant, it stays restricted)
		local scope = caller.host.capture and caller.host.capture() or nil
		self.seq = self.seq + 1
		local entry = {
			seq = self.seq,
			fn = scope and function(...)
				return scope.run(fn, ...)
			end or fn,
			host = from_host(caller) or nil,
			owner = caller.fiber and caller.fiber.name or "?",
			owner_id = owner,
			timeout_ms = opts.timeout_ms == false and 0 or opts.timeout_ms,
			slot = opts.slot,
		}
		local dispose = caller:effect(function()
			local list = lists[point]
			if not list then
				list = {}
				lists[point] = list
			end
			-- the host's voters first (in registration order; a host prepend at the very front),
			-- then the plugins': `prepend` puts a plugin's voter first among the plugins'
			local function after_host()
				for i, e in ipairs(list) do
					if not e.host then
						return i
					end
				end
				return #list + 1
			end
			if entry.host then
				table.insert(list, opts.prepend and 1 or after_host(), entry)
			elseif opts.prepend then
				table.insert(list, after_host(), entry)
			else
				list[#list + 1] = entry
			end
			-- a voter or rewriter changes what a decision answers: an approval taken before is stale
			prims.policy.changed()
			return function()
				for i, e in ipairs(list) do
					if e == entry then
						table.remove(list, i)
						prims.policy.changed()
						break
					end
				end
			end
		end, ("policy %s"):format(point))
		-- a registration's disposer is a function (the effect's disposer is a callable table)
		return function()
			return dispose()
		end
	end

	local function listed(items, point)
		for _, p in ipairs(items or {}) do
			if p == point then
				return true
			end
		end
		return false
	end

	local function require_point(items, kind, point)
		if not listed(items, point) then
			error(("`%s` is not a declared %s point"):format(tostring(point), kind), 0)
		end
	end

	function Policy:voter(point, fn, opts)
		require_point(prims.policy.points, "policy", point)
		return register(self, self.voters, point, fn, opts)
	end

	function Policy:rewriter(point, fn, opts)
		require_point(prims.policy.rewrite_points, "rewrite", point)
		return register(self, self.rewriters, point, fn, opts)
	end

	-- the live list in order; one voter per decision slot: the earliest registered of a slot
	-- (registration order, whatever `prepend` did to the running order)
	local function snapshot(list)
		local first = {}
		for _, e in ipairs(list or {}) do
			if e.slot and (not first[e.slot] or e.seq < first[e.slot]) then
				first[e.slot] = e.seq
			end
		end
		local out = {}
		for _, e in ipairs(list or {}) do
			if not e.slot or first[e.slot] == e.seq then
				out[#out + 1] = e
			end
		end
		return out
	end

	-- Rust's view of the service: the live lists for Kernel::policy_with / rewrite
	function attach(self, ctx)
		local this = self
		prims.policy.attach(function(kind, point)
			return snapshot(this[kind] and this[kind][point])
		end)
		ctx:effect(function()
			return function()
				prims.policy.attach(nil)
			end
		end, "policy lists")
	end

	-- what an approval is fingerprinted with
	function Policy:epoch()
		return prims.events.policy_epoch()
	end

	-- a read: whether a point has voters (rewriters) now (was ctx:has_listeners on a decision
	-- point)
	function Policy:has_voters(point)
		if not listed(prims.policy.points, point) and not listed(prims.policy.rewrite_points, point) then
			error(("`%s` is not a declared policy or rewrite point"):format(tostring(point)), 0)
		end
		local list = self.voters[point] or self.rewriters[point]
		return #snapshot(list) > 0
	end

	function Policy:decide(point, args, options)
		require_point(prims.policy.points, "policy", point)
		if args == nil then
			return prims.policy.decide(point, options, nil)
		end
		if type(args) == "table" and (args.n ~= nil or args[1] ~= nil) then
			return prims.policy.decide(point, options, table.unpack(args, 1, args.n or #args))
		end
		return prims.policy.decide(point, options, args)
	end

	-- Returns rewritten arguments directly; a failed rewrite raises a structured
	-- `policy.rewrite_refused` error so the point owner refuses what it was about to run.
	function Policy:rewrite(point, call)
		require_point(prims.policy.rewrite_points, "rewrite", point)
		local ok, value = prims.policy.rewrite(point, call)
		if ok then
			return value
		end
		error(mare.error("policy.rewrite_refused", tostring(value)), 0)
	end

	return Policy
end
