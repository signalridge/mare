-- agent.service_validate: media input and output schema validation for agent prompts.
return function(ctx)
	local VIDEO_EXTS =
		{ mp4 = true, mov = true, m4v = true, webm = true, mkv = true, avi = true, mpeg = true, mpg = true }
	local AUDIO_EXTS =
		{ mp3 = true, wav = true, ogg = true, flac = true, aac = true, m4a = true, opus = true, weba = true }

	local function list_has_media_kind(list, prefix)
		if type(list) ~= "table" then
			return false
		end
		local mime_prefix = prefix .. "/"
		local data_prefix = "data:" .. prefix .. "/"
		local exts = prefix == "video" and VIDEO_EXTS or (prefix == "audio" and AUDIO_EXTS or nil)
		for _, item in ipairs(list) do
			if type(item) == "table" then
				local kind = item.kind or item.type
				local mime = type(item.mime) == "string" and item.mime:lower() or ""
				local url = type(item.url) == "string" and item.url:lower() or ""
				local path = type(item.path) == "string" and item.path:lower() or ""
				local ext = path:match("%.([a-z0-9]+)$")
				if
					kind == prefix
					or mime:sub(1, #mime_prefix) == mime_prefix
					or url:sub(1, #data_prefix) == data_prefix
					or (exts and ext and exts[ext])
				then
					return true
				end
			end
		end
		return false
	end

	local function has_video_input(opts)
		if type(opts) ~= "table" then
			return false
		end
		if type(opts.videos) == "table" and #opts.videos > 0 then
			return true
		end
		return list_has_media_kind(opts.attachments, "video")
			or list_has_media_kind(opts.content, "video")
			or list_has_media_kind(opts.images, "video")
	end

	local function has_audio_input(opts)
		if type(opts) ~= "table" then
			return false
		end
		if type(opts.audio) == "table" and next(opts.audio) ~= nil then
			return true
		end
		return list_has_media_kind(opts.attachments, "audio")
			or list_has_media_kind(opts.content, "audio")
			or list_has_media_kind(opts.images, "audio")
	end

	local function check_media_support(opts)
		if has_video_input(opts) then
			return "video input is unsupported: no video-capable backend or model is available", "video"
		end
		if has_audio_input(opts) then
			return "audio input in prompt is unsupported: no audio-capable backend or model is available", "audio"
		end
		return nil, nil
	end

	local function validate_output_schema(opts)
		if not opts or opts.output_schema == nil then
			return nil
		end
		local ok, compiled = pcall(mare.schema.compile_output, opts.output_schema)
		if not ok then
			return "invalid output_schema: " .. tostring(compiled)
		end
		opts.output_schema = compiled
		return nil
	end

	local function clone_list(src)
		if src == nil then
			return nil
		end
		local lib = require("mare_lib")
		local out = {}
		for i = 1, #src do
			out[i] = lib.copy_deep(src[i])
		end
		return out
	end

	local function normalize_attachments(attachments, existing_images, existing_blocks)
		local images = clone_list(existing_images)
		local blocks = clone_list(existing_blocks)
		if attachments == nil then
			return images, blocks
		end
		local lib = require("mare_lib")
		if type(attachments) ~= "table" or not lib.is_dense_list(attachments) then
			lib.fail("invalid_arguments", "attachments must be a dense array")
		end
		if #attachments == 0 then
			return (images and #images > 0) and images or nil, blocks and mare.json.array(blocks) or nil
		end
		images = images or {}
		blocks = blocks or {}
		for _, att in ipairs(attachments) do
			if type(att) ~= "table" then
				lib.fail("invalid_arguments", "each attachment must be an object")
			end
			if att.kind ~= nil and att.type ~= nil and att.kind ~= att.type then
				lib.fail("invalid_arguments", "attachment `kind` and `type` must match when both are present")
			end
			local kind = att.kind or att.type
			if type(kind) ~= "string" then
				lib.fail("invalid_arguments", "attachment requires a string `kind`")
			end
			if kind == "image" then
				if type(att.mime) ~= "string" or att.mime == "" or type(att.data) ~= "string" or att.data == "" then
					lib.fail("invalid_arguments", "image attachment requires non-empty `mime` and `data`")
				end
				local img = {
					type = "image",
					mime = att.mime,
					data = att.data,
					path = type(att.path) == "string" and att.path or nil,
				}
				images[#images + 1] = img
				blocks[#blocks + 1] = {
					type = "image",
					mime = att.mime,
					data = att.data,
					path = type(att.path) == "string" and att.path or nil,
				}
			elseif kind == "file" then
				if type(att.path) ~= "string" or att.path == "" then
					lib.fail("invalid_arguments", "file attachment requires non-empty `path`")
				end
				blocks[#blocks + 1] = {
					type = "mention",
					path = att.path,
					mime = type(att.mime) == "string" and att.mime or nil,
					name = type(att.name) == "string" and att.name or nil,
				}
			elseif kind == "audio" then
				blocks[#blocks + 1] = {
					type = "audio",
					mime = type(att.mime) == "string" and att.mime ~= "" and att.mime or nil,
					data = type(att.data) == "string" and att.data or nil,
					duration_secs = type(att.duration_secs) == "number" and att.duration_secs or nil,
					transcription = type(att.transcription) == "string" and att.transcription or nil,
				}
			elseif kind == "video" then
				blocks[#blocks + 1] = {
					type = "video",
					mime = type(att.mime) == "string" and att.mime ~= "" and att.mime or nil,
					data = type(att.data) == "string" and att.data or nil,
					duration_secs = type(att.duration_secs) == "number" and att.duration_secs or nil,
				}
			else
				lib.fail("invalid_arguments", "unsupported attachment kind: " .. tostring(kind))
			end
		end
		return (#images > 0 and images or nil), (#blocks > 0 and mare.json.array(blocks) or nil)
	end

	local function normalize_input(raw_input, opts)
		local lib = require("mare_lib")
		if opts and opts.queued == true and (raw_input == nil or raw_input == "") then
			return "", opts.images, opts.image_markers, opts.content
		end
		if type(raw_input) == "string" then
			local images = clone_list(opts and opts.images or nil)
			local blocks = clone_list(opts and opts.content or nil)
			if opts and opts.attachments ~= nil then
				if type(opts.attachments) == "table" and #opts.attachments > 0 and blocks == nil then
					blocks = {}
					if raw_input ~= "" then
						blocks[#blocks + 1] = { type = "text", text = raw_input }
					end
					for _, img in ipairs(images or {}) do
						blocks[#blocks + 1] = {
							type = "image",
							mime = img.mime,
							data = img.data,
							path = type(img.path) == "string" and img.path or nil,
						}
					end
				end
				images, blocks = normalize_attachments(opts.attachments, images, blocks)
			end
			return raw_input, images, opts and opts.image_markers or nil, blocks
		end
		if type(raw_input) ~= "table" or #raw_input == 0 then
			lib.fail("invalid_arguments", "input must be a string or non-empty ContentPart array")
		end
		local text_parts, images, blocks = {}, {}, {}
		for _, part in ipairs(raw_input) do
			if type(part) ~= "table" or type(part.type) ~= "string" then
				lib.fail("invalid_arguments", "each ContentPart must be an object with a string `type`")
			end
			if part.type == "text" then
				if type(part.text) ~= "string" then
					lib.fail("invalid_arguments", "text part requires string `text`")
				end
				text_parts[#text_parts + 1] = part.text
				blocks[#blocks + 1] = { type = "text", text = part.text }
			elseif part.type == "image" then
				if type(part.mime) ~= "string" or part.mime == "" or type(part.data) ~= "string" or part.data == "" then
					lib.fail("invalid_arguments", "image part requires non-empty `mime` and `data`")
				end
				images[#images + 1] = {
					type = "image",
					mime = part.mime,
					data = part.data,
					path = type(part.path) == "string" and part.path or nil,
				}
				blocks[#blocks + 1] = {
					type = "image",
					mime = part.mime,
					data = part.data,
					path = type(part.path) == "string" and part.path or nil,
				}
			elseif part.type == "file" then
				if type(part.path) ~= "string" or part.path == "" then
					lib.fail("invalid_arguments", "file part requires non-empty `path`")
				end
				blocks[#blocks + 1] = {
					type = "mention",
					path = part.path,
					mime = type(part.mime) == "string" and part.mime or nil,
					name = type(part.name) == "string" and part.name or nil,
				}
			elseif part.type == "audio" then
				blocks[#blocks + 1] = {
					type = "audio",
					mime = type(part.mime) == "string" and part.mime ~= "" and part.mime or nil,
					data = type(part.data) == "string" and part.data or nil,
					duration_secs = type(part.duration_secs) == "number" and part.duration_secs or nil,
					transcription = type(part.transcription) == "string" and part.transcription or nil,
				}
			elseif part.type == "video" then
				blocks[#blocks + 1] = {
					type = "video",
					mime = type(part.mime) == "string" and part.mime ~= "" and part.mime or nil,
					data = type(part.data) == "string" and part.data or nil,
					duration_secs = type(part.duration_secs) == "number" and part.duration_secs or nil,
				}
			else
				lib.fail("invalid_arguments", "unsupported ContentPart type: " .. tostring(part.type))
			end
		end
		local base_images = images
		if opts and type(opts.images) == "table" and #opts.images > 0 then
			local merged_images = clone_list(images) or {}
			for _, img in ipairs(opts.images) do
				merged_images[#merged_images + 1] = lib.copy_deep(img)
				blocks[#blocks + 1] = {
					type = "image",
					mime = img.mime,
					data = img.data,
					path = type(img.path) == "string" and img.path or nil,
				}
			end
			base_images = merged_images
		end
		images, blocks = normalize_attachments(opts and opts.attachments or nil, base_images, blocks)
		return table.concat(text_parts, "\n"), images, opts and opts.image_markers or nil, mare.json.array(blocks)
	end

	return {
		check_media_support = check_media_support,
		validate_output_schema = validate_output_schema,
		normalize_input = normalize_input,
	}
end
