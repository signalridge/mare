-- Google Generative Language wire: encode requests (generating the model URL) and decode SSE
-- and plain-JSON responses. Sending and error classification live in llm.send.
return function(D)
	local M = D.M

	local function build_url(base, model_id)
		local raw = tostring(base or ""):gsub("/+$", "")
		return ("%s/models/%s:streamGenerateContent?alt=sse"):format(raw, tostring(model_id or ""))
	end

	local function encode(req, endpoint)
		local model = req.model
		local base = endpoint.url or endpoint.base_url or ""
		local schema_sanitize = require("mare_lib.schema_sanitize")
		local dialect =
			schema_sanitize.detect_dialect(endpoint.dialect, endpoint.base_url or base, model.id, { route = "google" })
		local body = {
			contents = M.convert_messages(req.messages, model.id, endpoint.provider),
			generationConfig = { maxOutputTokens = req.max_tokens or model.max_output },
		}
		if req.output_schema ~= nil then
			local out_dialect = schema_sanitize.detect_dialect(
				endpoint.dialect,
				endpoint.base_url or base,
				model.id,
				{ route = "google", output_schema = true }
			)
			body.generationConfig.responseMimeType = "application/json"
			body.generationConfig.responseJsonSchema = schema_sanitize.sanitize(req.output_schema, out_dialect)
		end
		if req.system and req.system ~= "" then
			body.systemInstruction = { parts = { { text = req.system } } }
		end
		body.tools = M.convert_tools(req.tools, dialect)
		if req.tool_choice ~= nil then
			local mode, allowed
			if type(req.tool_choice) == "string" then
				local lc = req.tool_choice:lower()
				if lc == "none" then
					mode = "NONE"
				elseif lc == "auto" then
					mode = "AUTO"
				elseif lc == "any" or lc == "required" then
					mode = "ANY"
				end
			elseif
				type(req.tool_choice) == "table"
				and type(req.tool_choice.name) == "string"
				and req.tool_choice.name ~= ""
			then
				mode = "ANY"
				allowed = { req.tool_choice.name }
			end
			if mode and (body.tools ~= nil or mode == "NONE") then
				local fc_cfg = { mode = mode }
				if allowed then
					fc_cfg.allowedFunctionNames = allowed
				end
				body.toolConfig = { functionCallingConfig = fc_cfg }
			end
		end
		return {
			method = "POST",
			url = build_url(base, model.id),
			headers = { accept = "text/event-stream" },
			body = body,
		}
	end

	local function decoder(sink, _context)
		local P = M.new_parser(sink, function(e)
			return { source = "stream", payload = e }
		end)
		return {
			event = function(ev)
				P.feed(ev)
			end,
			body = function(text, _headers)
				-- a 200 with a plain JSON body (alt=sse ignored, e.g. by a proxy or :generateContent):
				-- the chunks as one array, a single response, or an error object
				local ok, j = pcall(mare.json.decode_lossless, text)
				if not ok or type(j) ~= "table" then
					error({ message = "Google: response is neither SSE nor JSON", kind = "malformed_response" }, 0)
				end
				for _, chunk in ipairs(j[1] ~= nil and j or { j }) do
					P.feed({ event = "message", data = mare.json.encode_strict(chunk) })
				end
			end,
			finish = function()
				return P.finish_message()
			end,
		}
	end

	return {
		name = "google-generate",
		route = "google",
		path = "",
		auth = { "x-goog-api-key", "bearer" },
		errors = M.ERRORS,
		estimate_hints = {
			image_mime_keys = { "mimeType", "mime_type" },
		},
		encode = encode,
		decoder = decoder,
	}
end
