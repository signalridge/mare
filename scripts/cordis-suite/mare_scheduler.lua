-- A cordis-lua test scheduler (test/kit/runner.lua) on mare.task: real time,
-- deferred spawn, no virtual time. mare-side code; not part of cordis-lua.
local M = {}

-- the host contract of cordis-lua docs/api-core.md §1 on mare.task
local function host(errors)
	return {
		spawn = function(fn)
			mare.task.spawn(function()
				local ok, err = pcall(fn)
				if not ok and errors then
					errors[#errors + 1] = tostring(err)
				end
			end)
		end,
		latch = function()
			local token = mare.task.token()
			return {
				open = function()
					token:cancel()
				end,
				wait = function()
					token:wait()
				end,
			}
		end,
		sleep = function(ms)
			-- mare.task.sleep takes whole milliseconds; round a positive fraction up
			if ms > 0 then
				mare.task.sleep(math.ceil(ms))
			else
				mare.task.sleep(0)
			end
		end,
		now = function()
			return mare.task.clock()
		end,
	}
end
M.host = host

-- how long one test may take before it counts as stalled
M.timeout = 10000

function M.new(_)
	local sched = { errors = {}, virtual = false }
	-- boot.lua's host: the library's tasks on spawn_root, user callbacks through capture;
	-- errors raised by tasks are still collected for the runner
	local booted = require("mare_kernel.test").boot().host
	local capture = booted.capture
	sched.host = setmetatable({
		spawn = function(fn)
			booted.spawn(function()
				local ok, err = pcall(fn)
				if not ok then
					sched.errors[#sched.errors + 1] = tostring(err)
				end
			end)
		end,
		capture = function()
			local cap = capture()
			return {
				run = cap.run,
				spawn = function(fn)
					return cap.spawn(function()
						local ok, err = pcall(fn)
						if not ok then
							sched.errors[#sched.errors + 1] = tostring(err)
						end
					end)
				end,
			}
		end,
	}, { __index = booted })
	function sched.setTimeout(fn, ms)
		local cancelled = false
		sched.host.spawn(function()
			sched.host.sleep(ms)
			if not cancelled then
				fn()
			end
		end)
		return function()
			cancelled = true
		end
	end
	function sched.drive(fn)
		local done = mare.task.token()
		local finished, ok, err = false, nil, nil
		mare.task.spawn(function()
			ok, err = pcall(fn)
			finished = true
			done:cancel()
		end)
		mare.task.spawn(function()
			mare.task.sleep(M.timeout)
			done:cancel()
		end)
		done:wait()
		if not finished then
			return false, "stalled: the test task did not complete within " .. M.timeout .. " ms"
		end
		return ok, err
	end
	return sched
end

return M
