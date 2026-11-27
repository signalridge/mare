-- The Rust host registers listeners on the host's fiber or an RPC connection's fiber.
-- They run in the context they registered in and as their
-- owner, whoever emits, and they go with their fiber (the host's closes with the tree).
--
--   host.owner()                             the host's owner record (`<root>`)
--   host.ctx()                               the host fiber's cordis ctx
--   host.on(ctx, owner, event, fn, opts) -> release (a plain function): `ctx:on`, opts =
--       { prepend? } (ahead of the listeners registered before; no numeric order)
--   host.voter(ctx, owner, point, fn, opts) -> release: `policy:voter` read through `ctx`, opts =
--       { prepend?, timeout_ms? } (`timeout_ms`: the answer time, 0 = none, nil = the default)
--   Explicit, as the native API is: an event listener is never turned into a voter by its name.
local cordis = require("cordis")
return function(root, prims, bus, Service)
	local host = {}
	local owner, ctx, fiber

	function host.owner()
		if not owner then
			owner = prims.owner.open("<root>", "root")
		end
		return owner
	end

	function host.ctx()
		if not ctx then
			-- the fiber's ctx exists from its construction (its apply runs a job later)
			local o = host.owner()
			fiber = prims.owner.enter(o, function()
				return root:plugin({ name = "<root>", apply = function() end })
			end)
			ctx = fiber.ctx
			-- the owner record goes with the fiber (disposed last: its first effect), as the policy
			-- fiber's does
			ctx:effect(function()
				return function()
					prims.owner.close(o)
				end
			end, "owner")
		end
		return ctx
	end

	-- the effect's disposer is a callable table; Rust keeps a function
	local function plain(release)
		return function()
			release()
		end
	end

	function host.on(on, as, event, fn, opts)
		opts = opts or {}
		return plain(prims.owner.enter(as, function()
			return on:on(event, fn, { prepend = opts.prepend == true })
		end))
	end

	-- policy:voter's release is already a plain function
	function host.voter(on, as, point, fn, opts)
		opts = opts or {}
		return prims.owner.enter(as, function()
			local policy = on:get("policy")
			if not policy then
				error(("the policy service is not running: no voter for `%s`"):format(point), 0)
			end
			-- the registrant's scope, captured before the service's own owner switch: the voter
			-- runs as its registrant (what it emits is attributed to `as`, not to `policy`)
			local scope = root.host.capture and root.host.capture() or nil
			local run = scope and function(...)
				return scope.run(fn, ...)
			end or fn
			return policy:voter(point, run, { prepend = opts.prepend == true, timeout_ms = opts.timeout_ms })
		end)
	end

	-- Register a contracted host-provided service on the host fiber through `mare.Service`.
	function host.provide(name, methods)
		local c = host.ctx()
		local o = host.owner()
		local cls = cordis.class("HostService_" .. tostring(name), Service)
		function cls:constructor(service_ctx)
			prims.owner.enter(o, cls.super.constructor, self, service_ctx, name)
		end
		for k, v in pairs(methods or {}) do
			cls[k] = v
		end
		return prims.owner.enter(o, function()
			return cls.new(c)
		end)
	end

	-- whether `f` is the host's fiber (the policy service's host segment: bus.is_host_fiber)
	function host.is(f)
		return f ~= nil and f == fiber
	end
	if bus then
		bus.is_host_fiber = host.is
	end

	return host
end
