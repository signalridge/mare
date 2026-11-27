-- Host plugin `task`: `ctx:after`, `ctx:every` and `ctx:spawn` as fiber effects on
-- mare.task. They replace the Ctx userdata methods of the same names: a task started through them
-- belongs to the caller's fiber and is aborted when that fiber unloads.
local cordis = require("cordis")

local TaskService = cordis.class("TaskService", cordis.Service)

-- a timer or task callback that raises is reported through a logger named after the caller's
-- fiber (its plugin; boot.lua turns it into `kernel/error`) and, for `every`, does not stop the
-- timer (as the Ctx methods did). The name is given: `ctx.logger` read here would name the logger
-- after its caller, this service
local function report(ctx, err)
	ctx.logger(ctx.fiber.name):error(err)
end

-- a task owned by `ctx`'s fiber: aborted when the fiber unloads; its effect is undone when the
-- task ends however it ends (returns, raises, is aborted — also before it ever ran: a watcher
-- waits for it), and a normal end never aborts the task it ends. The task acts for the owner its
-- caller acts for (prims.owner.current(): a service it creates, an event it emits is
-- attributed to that plugin), on top of the grant and tool context mare.task.spawn carries.
local function owned(current_owner, enter_owner, ctx, body)
	local owner = current_owner()
	local handle, dispose
	local ended = false
	dispose = ctx:effect(function()
		handle = mare.task.spawn(function()
			if owner then
				return enter_owner(owner, body)
			end
			return body()
		end)
		mare.task.spawn(function()
			pcall(handle.wait, handle)
			ended = true
			dispose()
		end)
		return function()
			if not ended and not handle:done() then
				handle:abort()
			end
		end
	end, "ctx.task()")
	return handle
end

---Run `fn` once after `ms`; returns the mare.Task (`:abort()` cancels it early).
local function after(current_owner, enter_owner, self, ms, fn)
	local ctx = self.ctx
	return owned(current_owner, enter_owner, ctx, function()
		mare.task.sleep(ms)
		local ok, err = pcall(fn)
		if not ok then
			report(ctx, err)
		end
	end)
end

---Run `fn` every `ms` until it returns false or the fiber unloads.
local function every(current_owner, enter_owner, self, ms, fn)
	local ctx = self.ctx
	return owned(current_owner, enter_owner, ctx, function()
		while true do
			mare.task.sleep(ms)
			local ok, result = pcall(fn)
			if not ok then
				report(ctx, result)
			elseif result == false then
				return
			end
		end
	end)
end

---Start `fn(...)` as a task owned by the caller's fiber; returns the mare.Task.
local function spawn(current_owner, enter_owner, self, fn, ...)
	local args = table.pack(...)
	local ctx = self.ctx
	return owned(current_owner, enter_owner, ctx, function()
		local r = table.pack(pcall(fn, table.unpack(args, 1, args.n)))
		if not r[1] then
			report(ctx, r[2])
			error(r[2], 0) -- the task's own handle still sees it fail
		end
		return table.unpack(r, 2, r.n)
	end)
end

function TaskService:constructor(ctx, config)
	cordis.Service.constructor(self, ctx, "task")
	local current_owner, enter_owner = config.prims.owner.current, config.prims.owner.enter
	rawset(self, "after", function(service, ms, fn)
		return after(current_owner, enter_owner, service, ms, fn)
	end)
	rawset(self, "every", function(service, ms, fn)
		return every(current_owner, enter_owner, service, ms, fn)
	end)
	rawset(self, "spawn", function(service, fn, ...)
		return spawn(current_owner, enter_owner, service, fn, ...)
	end)
	ctx:mixin("task", { "after", "every", "spawn" })
end

return TaskService
