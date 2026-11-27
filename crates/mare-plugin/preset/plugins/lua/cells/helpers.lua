-- Script output and small JSON memory: no service/ctx objects enter the capability VM.
return function(cfg, memory, _stop, options)
	local items, writes, visible, entries = mare.json.array(), {}, {}, 0
	local ledger =
		mare.sandbox.output_ledger((cfg.max_output_bytes ~= 0 and cfg.max_output_bytes) or cfg.max_bytes or 1048576)
	local exited = false
	local generation = memory.generation
	-- Snapshot JSON memory before starting a cell.
	local snapshot = mare.json.decode_lossless(mare.json.encode_strict(memory.values))
	local function clone_json(value)
		return mare.json.decode_lossless(mare.json.encode_strict(value))
	end
	local function add(block, publish)
		block = clone_json(block)
		if not ledger:admit(block) then
			error("script output byte limit exceeded", 0)
		end
		entries = entries + 1
		visible[entries] = publish ~= false
		if publish == false then
			return block
		end
		items[#items + 1] = block
		if options.on_output then
			options.on_output(block)
		end
	end
	local function text(value)
		add({
			type = "text",
			text = value == nil and "nil" or type(value) == "string" and value or mare.json.encode_strict(value),
		})
	end
	local function image(value, detail)
		local data, mime
		if type(value) == "table" and value.type == "image" then
			data = value.data
			mime = value.mimeType or value.mime
			detail = detail or value.detail or (value._meta and value._meta["codex/imageDetail"])
		else
			local uri = type(value) == "string" and value or type(value) == "table" and value.image_url
			detail = detail or (type(value) == "table" and value.detail)
			if type(uri) ~= "string" then
				error("image expects an inline data URI or image block", 0)
			end
			mime, data = uri:match("^data:([^;]+);base64,(.+)$")
		end
		if mime ~= "image/png" and mime ~= "image/jpeg" and mime ~= "image/gif" and mime ~= "image/webp" then
			error("image requires PNG/JPEG/GIF/WebP base64 data; remote URLs are unavailable", 0)
		end
		if type(data) ~= "string" or data == "" then
			error("image requires nonempty base64 data", 0)
		end
		assert(mare.crypto.base64_decode(data), "invalid image base64 data")
		if detail and detail ~= "auto" and detail ~= "low" and detail ~= "high" and detail ~= "original" then
			error("invalid image detail", 0)
		end
		add({ type = "image", data = data, mime = mime, detail = detail or "high" })
	end
	local function audio(value)
		local data, mime
		if type(value) == "table" and value.type == "audio" then
			data = value.data
			mime = value.mimeType or value.mime
		else
			local uri = type(value) == "string" and value or type(value) == "table" and value.audio_url
			if type(uri) ~= "string" then
				error("audio expects an inline data URI or audio block", 0)
			end
			mime, data = uri:match("^data:([^;]+);base64,(.+)$")
		end
		if type(mime) ~= "string" or not mime:match("^audio/") or type(data) ~= "string" or data == "" then
			error("audio requires nonempty inline audio base64; remote URLs are unavailable", 0)
		end
		assert(mare.crypto.base64_decode(data), "invalid audio base64 data")
		add({ type = "audio", data = data, mime = mime })
	end
	local function store(key, value)
		if type(key) ~= "string" or #key > (cfg.max_bytes or 1048576) then
			error("store key exceeds the configured message byte limit", 0)
		end
		local encoded = mare.json.encode_strict(value)
		if #encoded > (cfg.max_bytes or 1048576) then
			error("store value exceeds the configured message byte limit", 0)
		end
		local previous = writes[key]
		writes[key] = mare.json.decode_lossless(encoded)
		local merged = table.copy(memory.values)
		for k, v in pairs(writes) do
			merged[k] = v
		end
		if #mare.json.encode_strict(merged) > (cfg.max_bytes or 1048576) then
			writes[key] = previous
			error("store exceeds the configured message byte limit", 0)
		end
	end
	local expose = {
		text = text,
		notify = function(value)
			local message = value == nil and "nil"
				or type(value) == "string" and value
				or mare.json.encode_strict(value)
			if not message:find("%S") then
				error("notify expects non-empty text", 0)
			end
			local block = add({ type = "text", text = message }, false)
			if not options.on_notify then
				error("notify has no active output channel", 0)
			end
			options.on_notify(block)
		end,
		yield_control = function()
			if options.on_yield then
				options.on_yield()
			end
		end,
		image = image,
		audio = audio,
		generatedImage = function(result)
			image(result)
			if result.output_hint then
				text(result.output_hint)
			end
		end,
		store = store,
		load = function(key)
			local value = writes[key]
			if value == nil then
				value = snapshot[key]
			end
			return clone_json(value == nil and mare.json.null or value)
		end,
		exit = function()
			exited = true
			return mare.sandbox.complete()
		end,
	}
	return {
		expose = expose,
		items = items,
		settle = function(value, err)
			local settled = ledger:finish(value, err)
			-- Observers may already have seen admitted entries. Only retained final
			-- output shrinks; streamed delivery cannot be recalled.
			for i = #items, 1, -1 do
				items[i] = nil
			end
			for i, block in ipairs(settled.items) do
				if visible[i] then
					items[#items + 1] = block
				end
			end
			return settled
		end,
		print = function(value)
			add({ type = "text", text = value })
		end,
		exited = function()
			return exited
		end,
		output = function()
			local text = {}
			for _, b in ipairs(items) do
				if b.type == "text" then
					text[#text + 1] = b.text
				end
			end
			return table.concat(text)
		end,
		finish = function(cancelled)
			if not cancelled and generation == memory.generation then
				for key, value in pairs(writes) do
					memory.values[key] = value
				end
			end
			return exited
		end,
		append_return = function(value)
			if value ~= mare.json.null then
				text(value)
			end
		end,
	}
end
