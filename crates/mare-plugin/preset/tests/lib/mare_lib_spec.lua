local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect
local lib = require("mare_lib")

describe("mare_lib", function()
	describe("parse_args", function()
		it("parses valid JSON objects", function()
			local args, err = lib.parse_args('{"path":"foo.txt","count":3}')
			expect.falsy(err)
			expect.eq(args, { path = "foo.txt", count = 3 })
		end)

		it("returns nil and an error on nil or empty input", function()
			local args, err = lib.parse_args(nil)
			expect.eq(args, nil)
			expect.truthy(err)
			expect.match(tostring(err), "missing or empty JSON")
		end)

		it("returns nil and an error instead of silently returning {}", function()
			local args, err = lib.parse_args("{not json")
			expect.eq(args, nil)
			expect.truthy(err)
		end)
	end)

	describe("retry_after_from_text", function()
		it("parses seconds, milliseconds, and fractional seconds", function()
			expect.eq(lib.retry_after_from_text("try again in 1.5s"), 1500)
			expect.eq(lib.retry_after_from_text("try again in 20ms"), 20)
			expect.eq(lib.retry_after_from_text("try again in 2 seconds"), 2000)
		end)

		it("returns nil for non-matching or non-string inputs", function()
			expect.eq(lib.retry_after_from_text("no delay here"), nil)
			expect.eq(lib.retry_after_from_text(nil), nil)
		end)
	end)

	describe("copy_deep", function()
		it("clones nested tables without sharing references", function()
			local orig = { a = { b = { 1, 2 } }, c = "x" }
			local cloned = lib.copy_deep(orig)
			expect.eq(cloned, orig)
			cloned.a.b[1] = 99
			expect.eq(orig.a.b[1], 1)
		end)
	end)

	describe("truncate_utf8", function()
		it("never splits multi-byte UTF-8 characters", function()
			local text = string.rep("a", 10) .. "中文测试"
			local out, truncated = lib.truncate_utf8(text, { max_bytes = 12 })
			expect.truthy(truncated)
			expect.truthy(utf8.len(out) ~= nil)
		end)

		it("returns input unchanged when within limits", function()
			local out, truncated = lib.truncate_utf8("hello", { max_bytes = 20 })
			expect.falsy(truncated)
			expect.eq(out, "hello")
		end)
	end)
end)
