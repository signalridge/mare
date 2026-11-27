-- What the service guards share (service.lua for mare.Service, rpc.lua): coded error values and
-- the caller's fiber, to refuse a call made through a context that has gone away.
local cordis = require("cordis")

local M = {}

-- Service instances that have passed contract admission, with the name they were admitted under;
-- weak keys (do not extend their lifetime).
M.admitted = setmetatable({}, { __mode = "k" })

-- Per-coroutine active caller ctx while invoking a host-provided method (weak keys by thread).
local active_ctxs = setmetatable({}, { __mode = "k" })

function M.active_ctx()
	local th = coroutine.running()
	return th and active_ctxs[th] or nil
end

function M.set_active_ctx(ctx)
	local th = coroutine.running()
	if not th then
		return nil
	end
	local prev = active_ctxs[th]
	active_ctxs[th] = ctx
	return prev
end

local UNLOADING, DISPOSED = cordis.FiberState.UNLOADING, cordis.FiberState.DISPOSED

---The caller's fiber and cordis ctx, when the caller is a cordis ctx.
function M.caller_fiber(caller)
	if type(caller) ~= "table" or not cordis.Context.is(caller) then
		return nil
	end
	return caller.fiber, caller
end

---Resolve `(conn_id, owner_id)` walking `M.fibers_up(fiber)` nearest-first.
function M.resolve_caller(fiber, rpc_ids)
	local conn_id, owner_id
	for f in M.fibers_up(fiber) do
		if not conn_id and rpc_ids then
			conn_id = rpc_ids[f]
		end
		if not owner_id then
			owner_id = conn_id or M.owners[f]
		end
		if conn_id and owner_id then
			break
		end
	end
	return conn_id, owner_id
end

---Enter `owner_id` (no-op when nil) while invoking `fn`, propagating Lua errors cleanly.
function M.as_owner(prims, owner_id, fn)
	if not owner_id then
		return fn()
	end
	local results = prims.owner.enter(owner_id, function()
		return table.pack(pcall(fn))
	end)
	if not results[1] then
		error(results[2], 0)
	end
	return table.unpack(results, 2, results.n)
end

---The fibers from `fiber` up, nearest first, as an iterator. The walk ends at `stop` (exclusive) and
---at the root: a fiber without a runtime, or one whose parent context's fiber is itself (cordis'
---root, fiber.lua `Fiber.__get.name`).
function M.fibers_up(fiber, stop)
	return function()
		local current = fiber
		if not current or current == stop or not current.runtime then
			return nil
		end
		local parent = current.parent and current.parent.fiber
		fiber = not rawequal(parent, current) and parent or nil
		return current
	end
end

---The name of the plugin a fiber belongs to: the nearest fiber up to `stop` (exclusive) whose
---runtime is named (a row, a host plugin, an rpc connection); an anonymous fiber (an inject
---follower, a with_service scope, a child service) is its plugin's.
function M.plugin_of(fiber, stop)
	for f in M.fibers_up(fiber, stop) do
		local rt = f.runtime
		local name = rt.name or (type(rt.callback) == "table" and rawget(rt.callback, "name"))
		if type(name) == "string" then
			return name
		end
	end
	return nil
end

---The owner record of each plugin row's fiber, from the row's `prims.owner.open` (before its
---apply runs) to its close; weak, a disposed fiber drops out. A caller's owner is the first fiber
---up its chain (fibers_up) that has one: a child service or follower is its row's.
M.owners = setmetatable({}, { __mode = "k" })

---One slot per service: a name already provided in `ctx` is refused with a code, never
---with cordis's message text (mare.Service's constructor).
function M.refuse_taken(ctx, name, level)
	if ctx.reflect:getImpl(name, false) then
		error(M.coded("already_provided", ("service `%s` is already provided"):format(name)), level + 1)
	end
end

---The effect metas (`ctx:effect` disposer's `[cordis.Context.effect]`) of the kernel's own
---bookkeeping inside a plugin's fiber (component.lua's owner effect); weak. The stall report
---(boot.lua `booted.stalled`) leaves them out: they are not what the plugin waits for.
M.kernel_effects = setmetatable({}, { __mode = "k" })

---Whether a fiber has gone (or is going) away.
function M.inactive(fiber)
	return fiber.uid == nil or fiber.state == UNLOADING or fiber.state == DISPOSED
end

---Whether a fiber has completely finished disposal (after UNLOADING effects ran).
function M.disposed(fiber)
	return fiber.state == DISPOSED or (fiber.uid == nil and fiber.state ~= UNLOADING)
end

---Format a cordis/Lua error value into a message string.
function M.error_message(err)
	return tostring(type(err) == "table" and err.message or err)
end

---Visit every fiber currently in `root`'s registry (`root` is any cordis Context).
function M.each_fiber(root, fn)
	root.registry:forEach(function(rt)
		for _, fiber in ipairs(rt.fibers:values()) do
			fn(fiber)
		end
	end)
end

-- Per-root parent -> { [childFiber] = true } index maintained via `internal/plugin`, so
-- `loadingChildren`, `settle`, and `dump` walk only a fiber's subtree instead of scanning the
-- entire registry (O(subtree) instead of O(N^2)).
local child_indices = setmetatable({}, { __mode = "k" })

---Return (installing once per root context) the parent->children fiber index for `rootCtx`.
function M.child_index(rootCtx)
	local root = rootCtx[cordis.Context.root] or rootCtx
	local idx = child_indices[root]
	if idx then
		return idx
	end
	idx = setmetatable({}, { __mode = "k" })
	child_indices[root] = idx
	local function track(fiber)
		local parent = fiber.parent and fiber.parent.fiber
		if not parent or rawequal(parent, fiber) then
			return
		end
		if fiber.uid ~= nil then
			local bucket = idx[parent]
			if not bucket then
				bucket = setmetatable({}, { __mode = "k" })
				idx[parent] = bucket
			end
			bucket[fiber] = true
		else
			local bucket = idx[parent]
			if bucket then
				bucket[fiber] = nil
				if next(bucket) == nil then
					idx[parent] = nil
				end
			end
		end
	end
	M.each_fiber(root, track)
	root:on("internal/plugin", track, { global = true })
	return idx
end

---Visit every active descendant fiber of `parentFiber` (excluding `parentFiber` itself)
---in deterministic ascending `uid` order.
function M.each_descendant(rootCtx, parentFiber, fn)
	local idx = M.child_index(rootCtx)
	local direct = idx[parentFiber]
	if not direct then
		return
	end
	local stack = {}
	local top = 0
	for child in pairs(direct) do
		top = top + 1
		stack[top] = child
	end
	local out = {}
	local n = 0
	while top > 0 do
		local cur = stack[top]
		stack[top] = nil
		top = top - 1
		n = n + 1
		out[n] = cur
		local kids = idx[cur]
		if kids then
			for child in pairs(kids) do
				top = top + 1
				stack[top] = child
			end
		end
	end
	if n > 1 then
		table.sort(out, function(a, b)
			return (a.uid or 0) < (b.uid or 0)
		end)
	end
	for i = 1, n do
		fn(out[i])
	end
end

---Whether `fiber` is a child component (`ctx:plugin(Service)` or named component) rather than an
---anonymous `ctx:inject` follower.
function M.is_child_component(fiber)
	local rt = fiber.runtime
	return rt ~= nil and (rt.name ~= nil or cordis.class.is(rt.callback) or fiber.provisions:size() > 0)
end

return M
