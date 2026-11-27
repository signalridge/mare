local t = require("mare.test")
local describe = t.describe
local it = t.it
local expect = t.expect

describe("tools:capabilities", function()
	it("advertises result_pipeline = 1 on the public tools contract", function()
		local host = t.host({ builtins = { "tools" } })
		expect.settled(host)
		---@type mare.ToolsService
		local tools = assert(host:get("tools"), "missing tools service")
		local caps = tools:capabilities()
		expect.eq(caps.result_pipeline, 1)
		host:dispose()
	end)
end)
