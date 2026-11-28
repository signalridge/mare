-- llm.api_errors: classify / transform / retry-after methods, response closing, http_error and
-- stream_error; part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
return function(D)
	local lib, llm = D.lib, D.llm
	local classify, classify_raw = D.errors.classify, D.errors.classify_raw
	local transform_messages = D.history.transform_messages

	llm.classify = function(_, _, err, cancelled)
		if type(err) == "table" and (err._classified or err.source ~= nil) then
			local _, kind, retryable = classify_raw(err, { cancelled = cancelled }, D.providers)
			return kind, retryable
		end
		return classify(err, cancelled)
	end
	llm.transform_messages = function(_, _, messages, model, tools, policy, route, default_route, wire)
		return transform_messages(messages, model, tools, policy, route, default_route, wire)
	end

	--- close_response(resp): release a provider response's connection now instead of at GC. A
	--- response without `close` (a transport plugin's, a reused WebSocket) is left alone.
	function llm.close_response(_, _, resp)
		if resp == nil then
			return
		end
		local okc, close = pcall(function()
			return resp.close
		end)
		if okc and type(close) == "function" then
			pcall(close, resp)
		end
	end
	-- the response is closed on every exit (including coroutine abort) and errors re-raised unchanged
	local function with_response(resp, fn)
		local _close <close> = require("mare_lib").guard(function()
			llm.close_response(nil, nil, resp)
		end)
		return fn(resp)
	end
	D.with_response = with_response

	local MAX_RAW_BODY_BYTES = 16384

	local REQUEST_ID_HEADERS = {
		"x-request-id",
		"request-id",
		"x-trace-id",
		"cf-ray",
		"x-amzn-requestid",
		"apim-request-id",
		"x-ms-request-id",
		"anthropic-request-id",
		"x-correlation-id",
	}

	local function extract_request_id(headers, j, api)
		if type(headers) == "table" then
			for _, h in ipairs(REQUEST_ID_HEADERS) do
				local v = headers[h]
				if type(v) == "string" and v ~= "" then
					return v
				end
			end
			for k, v in pairs(headers) do
				local lk = string.lower(tostring(k))
				for _, h in ipairs(REQUEST_ID_HEADERS) do
					if lk == h and type(v) == "string" and v ~= "" then
						return v
					end
				end
			end
		end
		if type(api) == "table" then
			local v = api.request_id or api.trace_id
			if type(v) == "string" and v ~= "" then
				return v
			end
		end
		if type(j) == "table" then
			local v = j.request_id or j.trace_id
			if type(v) == "string" and v ~= "" then
				return v
			end
		end
		return nil
	end

	local function classify_opts(opts, api, j)
		return {
			provider = opts.provider,
			wire_errors = opts.wire_errors,
			codes = opts.codes,
			overflow = opts.overflow,
			image_limit = opts.image_limit,
			image_unsupported = opts.image_unsupported,
			map = opts.map,
			no_reply_left = opts.no_reply_left,
			api_err = api,
			body = j,
		}
	end

	--- http_error(label, resp, opts?) -> error table for a non-2xx provider response (
	--- part one): builds the bounded raw error `{source = "http", status, headers, api_code,
	--- api_type, message, body}` and classifies it through `llm/errors.lua`.
	function llm.http_error(_, _, label, resp, opts)
		opts = opts or {}
		-- a failed body read (reset, idle) raises a retryable `transport:` error
		local text = with_response(resp, function()
			return resp:text()
		end)
		local j = mare.json.decode(text)
		local api = {}
		if type(j) == "table" then
			if type(j.error) == "table" then
				api = j.error
			elseif type(j[1]) == "table" and type(j[1].error) == "table" then
				api = j[1].error -- Google returns an array
			end
		end
		if opts.extract then
			local extracted = opts.extract(j)
			if type(extracted) == "table" then
				local merged = {}
				for k, v in pairs(api) do
					merged[k] = v
				end
				for k, v in pairs(extracted) do
					merged[k] = v
				end
				if extracted.api_code ~= nil and extracted.code == nil then
					merged.code = extracted.api_code
				end
				if extracted.api_type ~= nil and extracted.type == nil then
					merged.type = extracted.api_type
				end
				api = merged
			end
		end
		local msg = api.message
			or (type(j) == "table" and type(j.error) == "string" and j.error)
			or (type(j) == "table" and j.detail and tostring(j.detail))
			or text
		local code = api.code ~= nil and tostring(api.code) or nil
		local status = resp.status
		local headers = resp.headers or {}
		local rid = extract_request_id(headers, j, api)
		local raw = {
			source = "http",
			provider = opts.provider,
			label = label,
			status = status,
			headers = headers,
			api_code = code,
			api_type = api.type or api.status,
			code = code,
			type = api.type or api.status,
			message = ("%s %d: %s"):format(label, status, tostring(msg)),
			raw_message = tostring(msg),
			body = type(text) == "string" and (#text > MAX_RAW_BODY_BYTES and text:sub(1, MAX_RAW_BODY_BYTES) or text)
				or nil,
			request_id = rid,
			retry_after_ms = mare.http.retry_after(headers),
			plan_type = api.plan_type,
			resets_at = api.resets_at,
		}
		local err = classify_raw(raw, classify_opts(opts, api, j), D.providers)
		if type(err) == "table" then
			if type(err.message) == "string" then
				err.message = mare.text.redact(err.message)
			end
			if type(err.raw_message) == "string" then
				err.raw_message = mare.text.redact(err.raw_message)
			end
			if type(err.body) == "string" then
				err.body = mare.text.redact(err.body)
			end
		end
		return err
	end

	--- stream_error(label, e, opts?) -> error table for an error event inside a 200 stream
	--- (Anthropic `error`, Google / Chat Completions / Responses `{"error": …}`): builds the raw
	--- error and classifies it through `llm/errors.lua`.
	function llm.stream_error(_, _, label, e, opts)
		opts = opts or {}
		e = type(e) == "table" and e or { message = tostring(e) }
		local msg = type(e.message) == "string" and e.message ~= "" and e.message or D.errors.summarize_error(e)
		local raw_code = e.code ~= nil and e.code or e.api_code
		local code = raw_code ~= nil and tostring(raw_code) or nil
		-- an HTTP status, given as `status` or as a numeric `code` (Google: code 429)
		local sn = tonumber(opts.status) or tonumber(e.status) or tonumber(raw_code)
		local status = sn and math.tointeger(sn) or nil
		if status and (status < 100 or status > 599) then
			status = nil
		end
		local etype = e.type ~= nil and e.type or e.api_type
		if etype == nil and type(e.status) == "string" and not tonumber(e.status) then
			etype = e.status -- Google: "RESOURCE_EXHAUSTED"
		end
		local headers = type(e.headers) == "table" and e.headers or nil
		local rid = extract_request_id(headers, e, e)
		local raw = {
			source = opts.source or "stream",
			provider = opts.provider,
			label = label,
			status = status,
			headers = headers,
			api_code = code,
			api_type = etype,
			code = code,
			type = etype,
			message = status and ("%s %d: %s"):format(label, status, tostring(msg))
				or ("%s: %s"):format(label, tostring(msg)),
			raw_message = tostring(msg),
			request_id = rid,
			retry_after_ms = headers and mare.http.retry_after(headers) or nil,
			plan_type = e.plan_type,
			resets_at = e.resets_at,
		}
		local err = classify_raw(raw, classify_opts(opts, e, nil), D.providers)
		if type(err) == "table" then
			if err.retry_after_ms == nil and err.kind == "rate_limit" then
				err.retry_after_ms = lib.retry_after_from_text(msg)
			end
			if type(err.message) == "string" then
				err.message = mare.text.redact(err.message)
			end
			if type(err.raw_message) == "string" then
				err.raw_message = mare.text.redact(err.raw_message)
			end
		end
		return err
	end
end
