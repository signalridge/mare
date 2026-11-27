-- Independent Lua projection of the same registered schemas used by native calls.
return function()
	local function keys(value)
		local result = {}
		for key in pairs(value or {}) do
			result[#result + 1] = key
		end
		table.sort(result)
		return result
	end
	local function kind(schema, depth)
		if depth > 32 then
			return "any"
		end
		if schema.const ~= nil then
			return mare.json.encode_strict(schema.const)
		end
		if schema.enum then
			local result = {}
			for _, v in ipairs(schema.enum) do
				result[#result + 1] = mare.json.encode_strict(v)
			end
			return table.concat(result, "|")
		end
		if schema.oneOf then
			local result = {}
			for _, v in ipairs(schema.oneOf) do
				result[#result + 1] = kind(v, depth + 1)
			end
			return table.concat(result, "|")
		end
		local t = schema.type
		if t == "integer" then
			return "integer"
		elseif t == "number" then
			return "number"
		elseif t == "boolean" then
			return "boolean"
		elseif t == "string" then
			return "string"
		elseif t == "null" then
			return "json.null"
		end
		if t == "array" then
			return "(" .. kind(schema.items or {}, depth + 1) .. ")[]"
		end
		if t == "object" or schema.properties then
			local required = {}
			for _, name in ipairs(schema.required or {}) do
				required[name] = true
			end
			local fields = {}
			for _, name in ipairs(keys(schema.properties)) do
				fields[#fields + 1] = mare.json.encode_strict(name)
					.. (required[name] and ":" or "?:")
					.. kind(schema.properties[name], depth + 1)
			end
			if schema.additionalProperties ~= false then
				fields[#fields + 1] = "[string]:"
					.. (
						type(schema.additionalProperties) == "table"
							and kind(schema.additionalProperties, depth + 1)
						or "any"
					)
			end
			return "{" .. table.concat(fields, ",") .. "}"
		end
		return "any"
	end
	local envelope = {
		type = "object",
		properties = { content = { oneOf = { { type = "string" }, { type = "array", items = {} } } }, details = {} },
		required = { "content" },
	}
	return function(declaration, definition)
		local output = definition.output_schema or envelope
		return mare.json.decode_lossless(mare.json.encode_strict({
			name = declaration.name,
			description = declaration.description,
			parameters = declaration.parameters,
			output_schema = output,
			declaration = "tools[" .. mare.json.encode_strict(declaration.name) .. "](args:" .. kind(
				declaration.parameters,
				0
			) .. ")->" .. kind(output, 0),
		}))
	end
end
