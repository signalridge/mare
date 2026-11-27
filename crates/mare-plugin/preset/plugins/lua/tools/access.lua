-- One evaluation and canonicalization path for tool-declared file access.
return function(D)
	local mare_lib = require("mare_lib")
	local clone = D.results.clone
	local evaluating = false
	local OPS = { read = true, write = true, create = true, delete = true }
	local FIELDS = { kind = true, op = true, path = true, recursive = true }
	local function evaluate(def, args, cwd)
		if def.access == nil then
			return { state = "unknown" }
		end
		assert(type(def.access) == "function", "invalid access declaration")
		assert(not evaluating, "access declaration re-entered")
		local copied = clone(args)
		local original = mare.json.encode_strict(copied)
		evaluating = true
		local _guard <close> = setmetatable({}, {
			__close = function()
				evaluating = false
			end,
		})
		local co = coroutine.create(def.access)
		local ok, value = coroutine.resume(co, copied, { cwd = cwd })
		assert(ok and coroutine.status(co) == "dead", "access declaration failed or yielded")
		assert(mare.json.encode_strict(copied) == original, "access declaration changed its arguments")
		assert(type(value) == "table" and not rawequal(value, copied), "access declaration must return a list")
		local dense, count = mare_lib.is_dense_list(value)
		assert(dense, "access must be a dense list")
		mare.json.encode_strict(value) -- enforce the shared JSON budget before path inspection
		local entries, seen = mare.json.array(), {}
		for i = 1, count do
			local item = value[i]
			assert(type(item) == "table", "access entry must be an object")
			for key in pairs(item) do
				assert(FIELDS[key], "unknown access field")
			end
			assert(item.kind == "file" and OPS[item.op], "invalid access kind or operation")
			assert(type(item.path) == "string" and item.path ~= "" and not item.path:find("%z"), "invalid access path")
			assert(not mare.projection.contains(item.path), "access path contains a hidden value")
			assert(type(item.recursive) == "boolean", "access recursive must be boolean")
			local path = mare.fs.resolve_path(item.path, cwd)
			local entry = {
				kind = "file",
				op = item.op,
				path = path.logical,
				logical = path.logical,
				physical = path.physical,
				identity = path.identity,
				recursive = item.recursive,
			}
			local key = mare.json.encode_strict(entry)
			if not seen[key] then
				entries[#entries + 1], seen[key] = entry, true
			end
		end
		-- Strict JSON encoding enforces the same depth and node budget as other tool data.
		mare.json.encode_strict(entries)
		return { state = "declared", entries = entries }
	end
	return { evaluate = evaluate }
end
