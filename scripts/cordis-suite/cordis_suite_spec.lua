-- Run cordis-lua's own test suite on the mlua executor against the vendored copy.
-- `require("cordis")` is served by the kernel (crates/mare-kernel/lua/cordis/, enabled
-- for `mare test`); the specs and their kit come from cordis-lua's `test/` at the commit in
-- VENDOR, exported by scripts/cordis-suite.sh into $CORDIS_SUITE_TESTS.
local tests = assert(os.getenv("CORDIS_SUITE_TESTS"), "run through scripts/cordis-suite.sh")
local here = assert(os.getenv("CORDIS_SUITE_DIR"), "run through scripts/cordis-suite.sh")
local report = os.getenv("CORDIS_SUITE_REPORT")

-- mare's `require` consults `package.preload` first and never reads `package.path` or
-- `package.searchers`: register the test kit and spec helper modules in `package.preload`.
local function normalize_path(path)
	return (path:gsub("\\", "/"))
end

local function preload_tree(root)
	local prefix = normalize_path(root):gsub("/+$", "") .. "/"
	local p = assert(io.popen('find "' .. root .. '" -name "*.lua" | sort'))
	for line in p:lines() do
		local norm = normalize_path(line)
		if norm:sub(1, #prefix) == prefix and not norm:match("_spec%.lua$") then
			local rel = norm:sub(#prefix + 1)
			local mod = rel:gsub("/init%.lua$", ""):gsub("%.lua$", ""):gsub("/", ".")
			if mod ~= "cordis" and mod:sub(1, 7) ~= "cordis." and package.preload[mod] == nil then
				local file_path = line
				package.preload[mod] = function(name)
					return assert(loadfile(file_path))(name, file_path)
				end
			end
		end
	end
	p:close()
end

preload_tree(tests)
preload_tree(here)

local runner = require("kit.runner")
local scheduler = require("mare_scheduler")

local files = {}
local p = io.popen('find "' .. tests .. '" -name "*_spec.lua" | sort')
for line in p:lines() do
	local norm = normalize_path(line)
	-- lib/host_spec.lua tests cordis-lua's reference scheduler (cordis.host.coroutine), which is
	-- not vendored: inside mare the host is mare's own
	-- lib/capture_spec.lua drives that scheduler's labelled contexts to test extension X1; on
	-- mare, X1 is covered by crates/mare-kernel/lua-specs/cordis_host/ against the real capture binding
	if not norm:find("/test/lib/host_spec.lua", 1, true) and not norm:find("/test/lib/capture_spec.lua", 1, true) then
		files[#files + 1] = line
	end
end
p:close()

mare.test.describe("cordis-lua suite on mlua", function()
	mare.test.it("runs with no failures", function()
		local result = runner.run({ files = files, filter = os.getenv("CORDIS_FILTER"), scheduler = scheduler.new })
		local lines = { string.format("passed %d failed %d skipped %d", result.passed, result.failed, #result.skipped) }
		for _, f in ipairs(result.failures) do
			lines[#lines + 1] = "FAIL " .. f
		end
		for _, s in ipairs(result.skipped) do
			lines[#lines + 1] = string.format("SKIP %s > %s (%s)", s.file:gsub("^.*/test/", ""), s.name, s.reason)
		end
		local text = table.concat(lines, "\n")
		if report then
			local out = assert(io.open(report, "w"))
			out:write(text, "\n")
			out:close()
		end
		print(lines[1])
		mare.test.expect.eq(result.failed, 0, text)
	end)

	mare.test.it("starts yielding effect disposers in reverse order and runs them concurrently", function()
		local cordis = require("cordis")
		local host = scheduler.new().host
		local root = cordis.Context.new({ host = host })
		local started, finished = {}, {}
		local gates = { mare.task.token(), mare.task.token(), mare.task.token() }
		local fiber = cordis.await(root:use(function(ctx)
			for i = 1, 3 do
				ctx:effect(function()
					return function()
						started[#started + 1] = i
						gates[i]:wait()
						finished[#finished + 1] = i
					end
				end)
			end
		end))
		local disposing = fiber:dispose()
		while #started < 3 do
			cordis.tick(host)
		end
		mare.test.expect.eq(started, { 3, 2, 1 })
		mare.test.expect.eq(finished, {})
		gates[1]:cancel()
		while #finished < 1 do
			cordis.tick(host)
		end
		mare.test.expect.eq(finished, { 1 })
		gates[3]:cancel()
		while #finished < 2 do
			cordis.tick(host)
		end
		mare.test.expect.eq(finished, { 1, 3 })
		gates[2]:cancel()
		cordis.await(disposing)
		mare.test.expect.eq(finished, { 1, 3, 2 })
		mare.test.expect.eq(fiber.state, cordis.FiberState.DISPOSED)
		cordis.await(root:close())
	end)
end)
