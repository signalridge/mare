-- Web search and fetch provider registry.
return function(ctx, cfg, svc)
	local lib = require("mare_lib")
	local registries = { search = {}, fetch = {} }
	local conn_counts = {}
	local service = {}
	local function registry(kind)
		return assert(registries[kind], "web kind must be search or fetch")
	end
	local function fail_web_error(err)
		if type(err) == "table" and type(err.code) == "string" then
			error(err, 0)
		end
		local msg = tostring(err)
		local first = msg:match("^([^\r\n]+)") or msg
		local stripped = first:match("(WEB_[%u_]+.*)$") or first:match("^.-:%d+: (.*)$") or first
		local code
		if stripped:match("^WEB_BLOCKED_URL") or stripped:match("^WEB_DENIED") then
			code = "denied"
		elseif stripped:match("^WEB_CANCELLED") or stripped:match("^WEB_ABORTED") then
			code = "cancelled"
		elseif
			stripped:match("^WEB_TIMEOUT")
			or stripped:match("^WEB_CALLBACK_TIMEOUT")
			or stripped:match("^WEB_FETCH_TIMEOUT")
		then
			code = "timed_out"
		elseif
			stripped:match("^WEB_INVALID_RESULT")
			or stripped:match("^WEB_DECODE_ERROR")
			or stripped:match("^WEB_HTML_ERROR")
			or stripped:match("^WEB_RESPONSE_TOO_LARGE")
			or stripped:match("^WEB_RESULT_TOO_LARGE")
		then
			code = "invalid_result"
		elseif stripped:match("^WEB_INVALID_") or stripped:match("^WEB_DUPLICATE_PROVIDER") then
			code = "invalid_arguments"
		elseif stripped:match("^WEB_UNSUPPORTED_") then
			code = "unsupported"
		elseif stripped:match("^WEB_") then
			code = "unavailable"
		else
			error(err, 0)
		end
		require("mare_lib").fail(code, stripped)
	end
	local function available(entry, model)
		if not entry.active then
			return false
		end
		if not entry.available then
			return true
		end
		local ok, value = pcall(entry.available, model)
		return entry.active and ok and value == true
	end
	local function selection(kind, model)
		local entries = registry(kind)
		local configured = cfg[kind .. "_provider"]
		if cfg[kind] == false or (kind == "search" and cfg.web_search == "disabled") then
			return nil, "disabled"
		end
		if configured then
			local e = entries[configured]
			if not e then
				return nil, "configured_missing"
			end
			if not available(e, model) then
				return nil, "configured_unavailable"
			end
			return e
		end
		local found
		for _, entry in pairs(entries) do
			if available(entry, model) then
				if found then
					return nil, "ambiguous"
				end
				found = entry
			end
		end
		return found, found and nil or "unavailable"
	end
	local function register(kind, caller, backend)
		if
			type(backend) ~= "table"
			or type(backend.provider_id) ~= "string"
			or not backend.provider_id:match("^[%w_.-]+$")
			or #backend.provider_id > 64
		then
			lib.fail("invalid_arguments", "web backend requires a valid provider_id")
		end
		if type(backend[kind]) ~= "function" then
			lib.fail("invalid_arguments", "web backend requires " .. kind)
		end
		if backend.available ~= nil and type(backend.available) ~= "function" then
			lib.fail("invalid_arguments", "available must be a function")
		end
		local conn_id = lib.rpc_caller_id(caller)
		local unconfined = backend.unconfined == true
		local run_fn = backend[kind]
		if conn_id ~= nil then
			if backend.unconfined == false then
				lib.fail("invalid_arguments", "remote web backend must remain unconfined")
			end
			if (conn_counts[conn_id] or 0) >= 64 then
				lib.fail("invalid_arguments", "connection registration limit exceeded")
			end
			unconfined = true
			local raw_fn = run_fn
			run_fn = function(input)
				local ok, res = pcall(raw_fn, input)
				if not ok then
					if type(res) == "table" and type(res.code) == "string" then
						if res.code == "disconnected" then
							lib.fail("unavailable", "WEB_CALLBACK_DISCONNECTED")
						elseif res.code == "timed_out" then
							lib.fail("timed_out", "WEB_CALLBACK_TIMEOUT: " .. tostring(res.message or "timed out"))
						elseif res.code == "cancelled" then
							lib.fail("cancelled", "WEB_CANCELLED")
						end
						error(res, 0)
					end
					error("WEB_CALLBACK_ERROR: " .. tostring(res), 0)
				end
				return res
			end
		end
		local entries = registry(kind)
		if entries[backend.provider_id] then
			lib.fail("invalid_arguments", "WEB_DUPLICATE_PROVIDER: " .. backend.provider_id)
		end
		local entry = {
			id = backend.provider_id,
			available = backend.available,
			run = run_fn,
			active = true,
			calls = {},
			unconfined = unconfined,
		}
		entries[entry.id] = entry
		if conn_id ~= nil then
			conn_counts[conn_id] = (conn_counts[conn_id] or 0) + 1
		end
		local function dispose()
			if not entry.active then
				return
			end
			entry.active = false
			entries[entry.id] = nil
			if conn_id ~= nil and conn_counts[conn_id] then
				conn_counts[conn_id] = conn_counts[conn_id] - 1
				if conn_counts[conn_id] <= 0 then
					conn_counts[conn_id] = nil
				end
			end
			-- Remote registrations defer callback cancellation across disconnect so
			-- `conn.cancel_pending` can report `"disconnected"` (`WEB_CALLBACK_DISCONNECTED`)
			-- instead of racing ahead with `WEB_PROVIDER_REMOVED`.
			if conn_id == nil then
				for token in pairs(entry.calls) do
					token:cancel()
				end
			end
			ctx.bus:emit("web/providers-changed", { kind = kind, provider_id = entry.id, action = "removed" })
		end
		dispose = lib.owned(caller, dispose, "web " .. kind .. " " .. entry.id)
		entry.dispose = dispose
		ctx.bus:emit("web/providers-changed", { kind = kind, provider_id = entry.id, action = "added" })
		return dispose
	end
	function service.register_search(_, caller, backend)
		return register("search", caller, backend)
	end
	function service.register_fetch(_, caller, backend)
		return register("fetch", caller, backend)
	end
	function service.list_providers(_, _, kind)
		local model = svc.llm and svc.llm:current()
		local chosen, reason = selection(kind, model)
		local data = mare.json.array({})
		for _, e in pairs(registry(kind)) do
			data[#data + 1] = {
				provider_id = e.id,
				available = available(e, model),
				unconfined = e.unconfined,
				selected = chosen ~= nil and chosen.id == e.id,
			}
		end
		table.sort(data, function(a, b)
			return a.provider_id < b.provider_id
		end)
		if chosen then
			return { data = data }
		end
		return { data = data, reason = reason }
	end
	local function run(kind, request, opts)
		local lib = require("mare_lib")
		opts = opts or {}
		local default_timeout = kind == "fetch" and cfg.fetch_timeout_ms or cfg.search_timeout_ms
		local timeout_ms = opts.timeout_ms == nil and (default_timeout or 30000) or opts.timeout_ms
		if type(timeout_ms) ~= "number" or timeout_ms % 1 ~= 0 or timeout_ms < 1 or timeout_ms > 300000 then
			lib.fail("invalid_arguments", "timeout_ms must be in 1..=300000")
		end
		if kind == "fetch" then
			if type(request.url) ~= "string" or not request.url:match("%S") then
				lib.fail("invalid_arguments", "WEB_INVALID_URL: web fetch requires a nonblank url")
			end
		else
			if
				type(request.queries) ~= "table"
				or #request.queries == 0
				or #request.queries > cfg.search_max_queries
			then
				lib.fail("invalid_arguments", "WEB_INVALID_QUERIES: web search exceeds configured query limit")
			end
			local count = 0
			for key, query in pairs(request.queries) do
				if type(key) ~= "number" or key % 1 ~= 0 or key < 1 or key > #request.queries then
					lib.fail("invalid_arguments", "WEB_INVALID_QUERIES: queries must be an array")
				end
				if type(query) ~= "string" or not query:match("%S") then
					lib.fail("invalid_arguments", "WEB_INVALID_QUERIES: queries must be nonblank strings")
				end
				count = count + 1
			end
			if count ~= #request.queries then
				lib.fail("invalid_arguments", "WEB_INVALID_QUERIES: queries must be a dense array")
			end
		end
		local caller_cancel = opts.cancel and opts.cancel()
		if caller_cancel and caller_cancel:is_cancelled() then
			lib.fail("cancelled", "WEB_CANCELLED")
		end
		local current_model = svc.llm and svc.llm:current()
		local e, reason = selection(kind, current_model)
		if not e then
			lib.fail("unavailable", "WEB_PROVIDER_" .. tostring(reason):upper())
		end
		if not e.active then
			lib.fail("unavailable", "WEB_PROVIDER_REMOVED: " .. e.id)
		end
		if e.unconfined then
			-- an unconfined backend (e.g. an RPC callback) escapes the active sandbox: refuse it there
			local ok_conf, err_conf = pcall(mare.exec.refuse_unconfinable, "web provider " .. e.id)
			if not ok_conf then
				fail_web_error("WEB_UNCONFINED_DENIED: " .. tostring(err_conf))
			end
		end
		local token = caller_cancel and caller_cancel:child() or mare.task.token()
		local call = {}
		local current = mare.exec.current()
		call.cwd = (current and current.cwd) or mare.cwd
		call.tool_call_id = current and current.tool_call_id
		call.cancel = token
		call.model = current_model
		local input = lib.copy_deep(request)
		if kind == "search" then
			input.mode = cfg.web_search
			input.options = {
				context_size = cfg.context_size,
				allowed_domains = cfg.allowed_domains,
				blocked_domains = cfg.blocked_domains,
				location = cfg.location,
				max_uses = cfg.native_max_uses,
				version = cfg.native_version,
			}
		end
		e.calls[token] = true
		-- Closing the scope (including RPC $/cancel_request dropping the coroutine) cancels `token`
		-- so any active reverse `web.call` cancels immediately alongside the service call.
		local _lease <close> = setmetatable({}, {
			__close = function()
				e.calls[token] = nil
				token:cancel()
			end,
		})
		local ok, result = pcall(function()
			local winner, value = mare.task.select(function()
				assert(e.active, "WEB_PROVIDER_REMOVED: " .. e.id)
				assert(not token:is_cancelled(), "WEB_ABORTED")
				if kind == "search" then
					return require("web.search")(e.run, input, call, cfg.search_max_results)
				end
				return e.run(input, call)
			end, function()
				token:wait()
			end, function()
				mare.task.sleep(timeout_ms)
			end)
			if winner == 3 then
				token:cancel()
				error("WEB_TIMEOUT", 0)
			end
			if not e.active then
				error("WEB_PROVIDER_REMOVED: " .. e.id, 0)
			end
			if winner ~= 1 or token:is_cancelled() then
				error("WEB_ABORTED", 0)
			end
			assert(type(value) == "table", "WEB_INVALID_RESULT: expected table")
			if kind == "fetch" and value.requested_url == nil then
				value.requested_url = request.url
			end
			assert(#mare.json.encode(value) <= 2000000, "WEB_RESULT_TOO_LARGE")
			return value
		end)
		local is_disconnect = not ok and tostring(result):find("WEB_CALLBACK_DISCONNECTED", 1, true) ~= nil
		if not e.active and not is_disconnect then
			fail_web_error("WEB_PROVIDER_REMOVED: " .. e.id)
		end
		if not ok then
			fail_web_error(result)
		end
		return result
	end
	function service.search(_, _, queries, opts)
		return run("search", { queries = queries }, opts)
	end
	function service.fetch(_, _, url, opts)
		return run("fetch", { url = url }, opts)
	end
	ctx:effect(function()
		return function()
			local disposers = {}
			for _, entries in pairs(registries) do
				for _, entry in pairs(entries) do
					disposers[#disposers + 1] = entry.dispose
				end
			end
			for _, dispose in ipairs(disposers) do
				dispose()
			end
		end
	end, "cleanup")
	return service, function(kind, model)
		return selection(kind, model) ~= nil
	end
end
