-- llm.provider_accessor: projecting, usage-scoped, generation-bound accessor returned by llm:provider(name).
-- Direct calls through an accessor never bypass projection, auxiliary cancellation ownership,
-- provider-replacement invalidation, or single-counted usage attribution.
local projection_mod
local function get_projection()
	if not projection_mod then
		projection_mod = require("llm.projection")
	end
	return projection_mod
end

return function(D)
	local ctx = D.ctx
	local side_calls = D.side_calls
	local active_by_coro = setmetatable({}, { __mode = "k" })

	local function activate(entry)
		entry._live = true
		entry._active_cancels = {}
		entry._wrapped_stream = nil
		entry._wrapped_stream_src = nil
		entry._wrapped_compact = nil
		entry._wrapped_compact_src = nil
		entry._accessor = nil
	end

	local function deactivate(entry)
		if not entry then
			return
		end
		entry._live = false
		local cancels = entry._active_cancels
		if cancels then
			for tok in pairs(cancels) do
				tok:cancel()
			end
		end
	end

	local function ensure_live(pname, entry)
		if not entry._live then
			error("provider " .. tostring(pname) .. " was replaced or withdrawn", 0)
		end
	end

	local function model_ref_of(pname, req)
		local m = type(req) == "table" and req.model or nil
		if type(m) == "table" then
			if type(m.ref) == "string" and m.ref ~= "" then
				return m.ref
			end
			local mid = m.id or m.model_id
			if type(mid) == "string" and mid ~= "" then
				return pname .. "/" .. mid
			end
		end
		return pname
	end

	local function emit_usage(pname, req, res, default_purpose, usage_owner, usage_at)
		if type(res) == "table" and type(res.usage) == "table" then
			ctx.bus:emit("llm/usage", {
				usage = res.usage,
				model_ref = model_ref_of(pname, req),
				provider_id = pname,
				subscription = res.subscription,
				purpose = (type(req) == "table" and req.purpose) or default_purpose,
				request_id = type(req) == "table" and req.request_id or nil,
				session_id = usage_owner,
				leaf_entry_id = usage_at,
			})
		end
	end

	local function run_scoped(pname, entry, req, default_purpose, body)
		ensure_live(pname, entry)
		local co = coroutine.running()
		local scoped = co and active_by_coro[co] or nil
		if scoped and scoped[entry] then
			-- Transparent delegation (e.g. wrapper calling saved original): keep outer scope.
			local out = table.pack(body(req, nil, true))
			ensure_live(pname, entry)
			return table.unpack(out, 1, out.n)
		end
		local failure = mare.task.session_error()
		if failure then
			error(failure, 0)
		end
		local owned = type(req) == "table" and table.copy(req) or {}
		local session = D.follow.session()
		local info = session and session.info and session:info() or nil
		local usage_owner = info and info.session_id or nil
		local usage_at = info and info.leaf_entry_id or nil
		local caller_cancel = type(req) == "table" and req.cancel or nil
		local call_cancel = caller_cancel and caller_cancel:child() or mare.task.token()
		owned.cancel = call_cancel
		owned._usage_owner = usage_owner
		owned._usage_at = usage_at
		local side_entry = { owner = usage_owner, token = call_cancel }
		side_calls[side_entry] = true
		entry._active_cancels[call_cancel] = true
		if co then
			scoped = scoped or {}
			active_by_coro[co] = scoped
			scoped[entry] = true
		end
		local _guard <close> = setmetatable({}, {
			__close = function()
				side_calls[side_entry] = nil
				if entry._active_cancels then
					entry._active_cancels[call_cancel] = nil
				end
				if scoped then
					scoped[entry] = nil
				end
			end,
		})
		return get_projection().call(function()
			local projected_req = get_projection().request(owned)
			local results = table.pack(body(projected_req, call_cancel, false))
			ensure_live(pname, entry)
			local safe = { n = results.n }
			for i = 1, results.n do
				safe[i] = mare.projection.project(results[i])
			end
			if type(safe[1]) == "table" then
				emit_usage(pname, owned, safe[1], default_purpose, usage_owner, usage_at)
			elseif safe[1] == nil and type(safe[4]) == "table" then
				emit_usage(
					pname,
					owned,
					{ usage = safe[4], subscription = safe[5] },
					default_purpose,
					usage_owner,
					usage_at
				)
			end
			return table.unpack(safe, 1, safe.n)
		end)
	end

	local function build_stream_wrapper(pname, entry, fn)
		return function(req, sink)
			return run_scoped(pname, entry, req, "other", function(r, _, nested)
				if nested then
					return fn(r, sink)
				end
				local filtered = get_projection().sink()
				local function wrapped_sink(ev)
					local safe = filtered:push(ev)
					if safe and sink then
						sink(safe)
					end
				end
				local ok, res = pcall(fn, r, wrapped_sink)
				local flushed, flush_err = pcall(function()
					if
						ok
						and entry._live
						and type(res) == "table"
						and res.stop_reason ~= "aborted"
						and res.stop_reason ~= "error"
					then
						for _, ev in ipairs(filtered:finish()) do
							if sink then
								sink(ev)
							end
						end
					else
						filtered:close()
					end
				end)
				if not flushed then
					error(flush_err, 0)
				end
				if not ok then
					error(res, 0)
				end
				return res
			end)
		end
	end

	local function build_compact_wrapper(pname, entry, fn)
		return function(req)
			return run_scoped(pname, entry, req, "compact", function(r)
				return fn(r)
			end)
		end
	end

	local function stream_fn(pname, entry)
		if type(entry.stream) ~= "function" then
			return entry.stream
		end
		if entry._wrapped_stream == nil or entry._wrapped_stream_src ~= entry.stream then
			entry._wrapped_stream_src = entry.stream
			entry._wrapped_stream = build_stream_wrapper(pname, entry, entry.stream)
		end
		return entry._wrapped_stream
	end

	local function compact_fn(pname, entry)
		if type(entry.compact) ~= "function" then
			return entry.compact
		end
		if entry._wrapped_compact == nil or entry._wrapped_compact_src ~= entry.compact then
			entry._wrapped_compact_src = entry.compact
			entry._wrapped_compact = build_compact_wrapper(pname, entry, entry.compact)
		end
		return entry._wrapped_compact
	end

	local function wrap(pname, entry)
		if not entry then
			return nil
		end
		if entry._accessor then
			return entry._accessor
		end
		local proxy = setmetatable({}, {
			__index = function(_, k)
				if k == "stream" then
					return stream_fn(pname, entry)
				elseif k == "compact" then
					return compact_fn(pname, entry)
				end
				return entry[k]
			end,
			__newindex = function(_, k, v)
				entry[k] = v
				if k == "stream" then
					entry._wrapped_stream = nil
					entry._wrapped_stream_src = nil
				elseif k == "compact" then
					entry._wrapped_compact = nil
					entry._wrapped_compact_src = nil
				end
			end,
			__pairs = function()
				local k = nil
				return function()
					local v
					k, v = next(entry, k)
					while type(k) == "string" and k:sub(1, 1) == "_" do
						k, v = next(entry, k)
					end
					if k == "stream" then
						return k, stream_fn(pname, entry)
					elseif k == "compact" then
						return k, compact_fn(pname, entry)
					end
					return k, v
				end
			end,
		})
		entry._accessor = proxy
		return proxy
	end

	return { activate = activate, deactivate = deactivate, wrap = wrap }
end
