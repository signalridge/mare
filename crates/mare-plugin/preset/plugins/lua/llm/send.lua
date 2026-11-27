-- The send path for spec-based model wires. Retry and fallback between attempts remain in llm.stream.
return function(D)
	local ctx, llm, lib = D.ctx, D.llm, D.lib

	return function(spec, endpoint_arg, req, sink)
		local endpoint, wire, state, method, guard_tools, err_opts, label
		local function encode_and_prepare()
			local resolved = type(endpoint_arg) == "function" and endpoint_arg(req) or endpoint_arg
			-- Give the encoder a request-local view of configuration; it must not read changing
			-- process configuration while encoding a request.
			local view = {}
			for k, v in pairs(resolved) do
				view[k] = v
			end
			view.provider_config = D.api_keys.provider_cfg(resolved.provider)
			view.user_provider_config = D.directory.user_provider_config(resolved.provider)
			view.preset_config = D.directory.preset_config(resolved.provider)
			view.global_config = { cache_retention = mare.config.cache_retention }
			view.wire_config = spec.config
			endpoint = view

			wire, state = spec.encode(req, endpoint)
			wire.method = wire.method or "POST"
			method = wire.method
			wire.provider = endpoint.provider
			wire.model = wire.model or req.model.id
			wire.headers = wire.headers or {}
			guard_tools = lib.guard_tool_declarations(wire.body)
			if req.params_overlay then
				D.params.merge_overlay(wire, req.params_overlay)
			else
				llm.apply_params(nil, nil, req, wire)
			end

			-- Parameter mappings must not retain or replace credentials. Configured provider headers
			-- are merged afterward, as they were in the legacy providers.
			local headers = {}
			for name, value in pairs(wire.headers) do
				headers[name] = value
			end
			local auth = endpoint.auth or (type(spec.auth) == "table" and spec.auth[1])
			if endpoint.key ~= nil and auth then
				local auth_name = auth == "bearer" and "authorization" or auth
				for name in pairs(headers) do
					if type(name) == "string" and string.lower(name) == string.lower(auth_name) then
						headers[name] = nil
					end
				end
				headers[auth_name] = auth == "bearer" and ("Bearer " .. endpoint.key) or endpoint.key
			end
			for _, field in ipairs(endpoint.drop_fields or {}) do
				wire.body[field] = nil
			end
			for name, value in pairs(endpoint.headers or {}) do
				headers[name] = value
			end
			wire.headers = headers
			if spec.finalize then
				spec.finalize(req, endpoint, wire, state)
			end
			-- A reply-room retry reuses the overlay, but must honor the server's lower cap.
			if req._reply_cap then
				D.params.cap_output(wire.body, req._reply_cap)
			end
			wire = ctx.bus:waterfall("provider/request", wire) or wire
			guard_tools(wire)

			err_opts = {
				provider = endpoint.provider,
				wire_errors = spec.errors,
				no_reply_left = type(state) == "table" and state.no_reply_left == true or nil,
			}
			if spec.error then
				err_opts.extract = spec.error
			end
			label = endpoint.label or endpoint.provider or "provider"
		end

		encode_and_prepare()

		local function dispatch()
			local custom = ctx.bus:bail("provider/transport", {
				provider = endpoint.provider,
				label = label,
				transport = endpoint.transport,
				endpoint = endpoint,
				request = wire,
				cancel = req.cancel,
				fresh = req.fresh_connection,
				session_id = req.session_id,
				credential_id = endpoint.credential_id,
				proxy = wire.proxy,
				idle_timeout_ms = req.idle_timeout_ms or endpoint.idle_timeout_ms,
				connect_timeout_ms = wire.connect_timeout_ms or endpoint.connect_timeout_ms,
				no_reply_left = err_opts.no_reply_left,
			})
			guard_tools(wire)
			if type(custom) == "table" and custom.aborted then
				error({ message = custom.message or "request aborted", kind = "aborted", aborted = true }, 0)
			end
			if custom ~= nil then
				return custom
			end
			local ok, response = pcall(mare.http.request, {
				method = wire.method or method,
				url = wire.url,
				headers = wire.headers,
				json = wire.body,
				cancel = req.cancel,
				fresh = req.fresh_connection,
				proxy = wire.proxy,
				connect_timeout_ms = wire.connect_timeout_ms,
				idle_timeout_ms = req.idle_timeout_ms or endpoint.idle_timeout_ms,
				header_timeout_ms = req.header_timeout_ms,
				sse_max_line_bytes = req.sse_max_line_bytes,
				sse_max_event_bytes = req.sse_max_event_bytes,
				redirect = "same-origin",
			})
			if not ok then
				error({ message = tostring(response) })
			end
			return response
		end

		local response = dispatch()
		if not response.ok and response.status == 401 and type(endpoint.on_unauthorized) == "function" then
			local should_retry = endpoint.on_unauthorized({
				status = response.status,
				headers = response.headers,
				endpoint = endpoint,
				wire = wire,
			})
			if should_retry then
				pcall(D.with_response, response, function() end)
				encode_and_prepare()
				response = dispatch()
			end
		end
		if not response.ok then
			local err = llm.http_error(nil, nil, label, response, err_opts)
			if spec.retry and spec.retry(wire, err) then
				pcall(D.with_response, response, function() end)
				response = dispatch()
			else
				error(err, 0)
			end
		end
		if not response.ok then
			error(llm.http_error(nil, nil, label, response, err_opts), 0)
		end

		local ok, msg = pcall(D.with_response, response, function(resp)
			local decoder = spec.decoder(sink, {
				endpoint = endpoint,
				request = wire,
				response = resp,
				state = state,
			})
			local ctype = ""
			for name, value in pairs(resp.headers or {}) do
				if type(name) == "string" and string.lower(name) == "content-type" then
					ctype = string.lower(tostring(value))
					break
				end
			end
			if ctype ~= "" and not ctype:find("text/event-stream", 1, true) then
				if not decoder.body then
					error({ message = label .. ": unexpected non-streaming response", kind = "malformed_response" }, 0)
				end
				decoder.body(resp:text(), resp.headers or {})
			else
				local bus, waterfall = ctx.bus, ctx.bus.waterfall
				while true do
					local ev = resp:next_event()
					if not ev then
						break
					end
					local wf = waterfall(bus, "provider/event", { event = ev, provider = endpoint.provider })
					ev = (type(wf) == "table" and wf.event ~= nil and wf.event) or ev
					if decoder.event(ev) then
						break
					end
				end
			end
			return decoder.finish()
		end)
		if not ok then
			if type(msg) == "table" and msg.source == "stream" and msg.payload ~= nil then
				local payload = (spec.error and spec.error(msg.payload)) or msg.payload
				msg = llm.stream_error(nil, nil, label, payload, err_opts)
			end
			error(msg, 0)
		end
		return msg
	end
end
