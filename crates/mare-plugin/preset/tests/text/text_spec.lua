local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

describe("mare.json", function()
	it("round-trips nested tables and arrays", function()
		local value = {
			name = "mare",
			items = { 1, 2, 3 },
			nested = { ok = true },
		}
		local encoded = mare.json.encode(value)
		local decoded = mare.json.decode(encoded)
		expect.eq(decoded, value)
	end)

	it("returns nil and an error on invalid JSON decode", function()
		local val, err = mare.json.decode("{invalid")
		expect.eq(val, nil)
		expect.truthy(err)
	end)

	it("raises an error on encode_strict with a function", function()
		expect.error(function()
			mare.json.encode_strict({ fn = function() end })
		end)
	end)
end)

describe("mare.text", function()
	it("compiles and matches regexes", function()
		local re = mare.text.regex("^mare-[0-9]+$")
		expect.truthy(re:is_match("mare-42"))
		expect.falsy(re:is_match("mare-abc"))
	end)

	it("accumulates 4096 UTF-8 chunks in mare.stream.buffer equivalently", function()
		local buf = mare.stream.buffer()
		local parts = {}
		local chunk_a = string.rep("x", 256)
		local chunk_u = string.rep("雪", 85) .. "!"
		for i = 1, 4096 do
			local c = (i % 19 == 0) and chunk_u or chunk_a
			parts[i] = c
			buf:append(c)
		end
		local expected = table.concat(parts)
		expect.eq(buf:len(), #expected)
		expect.eq(buf:snapshot(), expected)
	end)

	it("parses move and multi-hunk patches via patch_paths and apply_patch", function()
		local patch = table.concat({
			"*** Begin Patch",
			"*** Update File: a.txt",
			"*** Move to: b.txt",
			"@@",
			"-old1",
			"+new1",
			"*** Update File: a.txt",
			"@@",
			"-old2",
			"+new2",
			"*** End Patch",
		}, "\n")
		local parsed, err = mare.text.patch_paths(patch)
		expect.falsy(err)
		expect.eq(#parsed, 2)
		expect.eq(parsed[1].kind, "update")
		expect.eq(parsed[1].path, "a.txt")
		expect.eq(parsed[1].move_to, "b.txt")
		expect.eq(parsed[2].kind, "update")
		expect.eq(parsed[2].path, "a.txt")
	end)

	it("slices empty-query fuzzy matches up to limit", function()
		local items = { "a.lua", "b.lua", "c.lua", "d.lua" }
		expect.eq(mare.text.fuzzy("", items, 2), { "a.lua", "b.lua" })
	end)
end)
