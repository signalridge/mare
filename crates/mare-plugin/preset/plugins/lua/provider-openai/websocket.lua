-- provider-openai.websocket: Responses over a reused WebSocket and the HTTP fallback state;
-- part of the `provider-openai` plugin (plugins/provider-openai.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, M = D.ctx, D.cfg, D.M
	local llm = D.auth.llm

	-- Responses over WebSocket (
	-- core/src/client.rs): `response.create` frames, the same JSON events as SSE, one
	-- connection per (session, provider, credential, proxy, endpoint) reused across turns.
	local WS_BETA = "responses_websockets=2026-02-06"
	local ws_conns = {} -- scope_key → { conn, key, session_id, provider, gen, busy, pooled }
	local active_entries = {} -- active unpooled/in-use entries → true (closed on scope/plugin retirement)
	local pending_dials = {} -- pending dial tokens → { key, gen, session_id, provider, cancelled }
	local scope_gen = {} -- scope_key → monotonic generation invalidated on fresh/close_scopes/dispose
	local ws_off = {} -- scope_key → reason (fell back to HTTP for this scope)
	local ws_off_until = {} -- scope_key → monotonic clock deadline (nil = permanent for scope)
	local ws_failures = {} -- scope_key → consecutive transport/connect failures
	local scope_meta = {} -- scope_key → { session_id, provider }
	local disposed = false
	local PERMANENT_WS_STATUS = { [400] = true, [403] = true, [404] = true, [405] = true, [426] = true, [501] = true }
	local function is_permanent_ws_rejection(why)
		if type(why) ~= "string" then
			return false
		end
		local code = tonumber(why:match("HTTP (%d%d%d)"))
		return code ~= nil and PERMANENT_WS_STATUS[code] == true
	end
	local TERMINAL = {
		["response.completed"] = true,
		["response.done"] = true,
		["response.incomplete"] = true,
		["response.failed"] = true,
	}
	local function normalize_headers(raw_hdrs)
		-- Match `crates/mare-binding/src/ws.rs` last-string-wins case-insensitive header semantics.
		-- `x-codex-turn-state` is per-request frame `client_metadata` rather than a connection key.
		local last_ci = {}
		local turn_state_val = nil
		if type(raw_hdrs) == "table" then
			for k, v in pairs(raw_hdrs) do
				if type(k) == "string" and type(v) == "string" then
					local lk = k:lower()
					if lk == "x-codex-turn-state" then
						if v ~= "" then
							turn_state_val = v
						end
					elseif lk ~= "accept" and lk ~= "content-type" and lk ~= "openai-beta" then
						last_ci[lk] = { name = k, value = v }
					end
				end
			end
		end
		local keys = {}
		for lk in pairs(last_ci) do
			keys[#keys + 1] = lk
		end
		table.sort(keys)
		local norm = {}
		local fp_pairs = {}
		for _, lk in ipairs(keys) do
			local entry = last_ci[lk]
			norm[entry.name] = entry.value
			fp_pairs[#fp_pairs + 1] = { lk, entry.value }
		end
		norm["openai-beta"] = WS_BETA
		return norm, fp_pairs, turn_state_val
	end
	local function effective_proxy(prov_name, ev_proxy, wire)
		local p = ev_proxy
			or (type(wire) == "table" and wire.proxy)
			or (prov_name and llm:provider_config(prov_name).proxy)
		return type(p) == "string" and p or ""
	end
	local is_ws_target = D.is_ws_target
	local function ws_url(url)
		return (url:gsub("^https://", "wss://"):gsub("^http://", "ws://"))
	end
	local function ws_pool_key(ev, wire)
		ev = type(ev) == "table" and ev or {}
		wire = type(wire) == "table" and wire or (type(ev.request) == "table" and ev.request or {})
		local prov_name = tostring(ev.provider or "")
		local proxy_part = effective_proxy(ev.provider, ev.proxy, wire)
		local cred_part = (type(ev.credential_id) == "string" and ev.credential_id ~= "") and ev.credential_id or ""
		local _, hdr_pairs = normalize_headers(wire.headers)
		local hdr_part = "hdr:" .. mare.crypto.sha256(mare.json.encode_strict(hdr_pairs))
		return mare.json.encode_strict({
			tostring(ev.session_id or ""),
			prov_name,
			cred_part,
			hdr_part,
			proxy_part,
			tostring(wire.url or ""),
		})
	end
	local function ensure_scope_meta(key, ev)
		scope_meta[key] = {
			session_id = tostring((type(ev) == "table" and ev.session_id) or ""),
			provider = tostring((type(ev) == "table" and ev.provider) or ""),
		}
	end
	local function current_gen(key)
		return scope_gen[key] or 0
	end
	local function prune_gen(k)
		if ws_conns[k] ~= nil or ws_off[k] ~= nil or ws_failures[k] ~= nil or scope_meta[k] ~= nil then
			return
		end
		for dial in pairs(pending_dials) do
			if dial.key == k then
				return
			end
		end
		for act in pairs(active_entries) do
			if act.key == k then
				return
			end
		end
		scope_gen[k] = nil
	end
	local function bump_gen(key)
		local next_gen = current_gen(key) + 1
		scope_gen[key] = next_gen
		if ws_conns[key] then
			ws_conns[key].retired = true
		end
		for act in pairs(active_entries) do
			if act.key == key then
				act.retired = true
			end
		end
		for dial in pairs(pending_dials) do
			if dial.key == key then
				dial.cancelled = true
			end
		end
		return next_gen
	end
	local function evict_conn(key, entry, close_socket)
		if entry then
			entry.busy = false
			active_entries[entry] = nil
		end
		if close_socket and entry and entry.conn then
			pcall(entry.conn.close, entry.conn)
		end
		if entry == nil or ws_conns[key] == entry then
			ws_conns[key] = nil
		end
		prune_gen(key)
	end
	local function close_scopes(pred)
		local keys = {}
		local seen = {}
		local function add_key(k)
			if not seen[k] then
				seen[k] = true
				keys[#keys + 1] = k
			end
		end
		for k in pairs(ws_conns) do
			add_key(k)
		end
		for k in pairs(ws_off) do
			add_key(k)
		end
		for k in pairs(ws_failures) do
			add_key(k)
		end
		for k in pairs(scope_meta) do
			add_key(k)
		end
		for dial in pairs(pending_dials) do
			add_key(dial.key)
		end
		for entry in pairs(active_entries) do
			add_key(entry.key)
		end
		for _, k in ipairs(keys) do
			local entry = ws_conns[k]
			local meta = scope_meta[k]
				or (entry and { session_id = entry.session_id, provider = entry.provider })
				or { session_id = "", provider = "" }
			if pred(meta, k) then
				bump_gen(k)
				if entry then
					evict_conn(k, entry, true)
				end
				local active_copy = {}
				for act in pairs(active_entries) do
					if act.key == k then
						active_copy[#active_copy + 1] = act
					end
				end
				for _, act in ipairs(active_copy) do
					evict_conn(k, act, true)
				end
				ws_off[k] = nil
				ws_off_until[k] = nil
				ws_failures[k] = nil
				scope_meta[k] = nil
				prune_gen(k)
			end
		end
	end
	local function ws_wanted(ev_or_prov, url)
		local ev = (type(ev_or_prov) == "table" and ev_or_prov.request ~= nil) and ev_or_prov
			or {
				provider = type(ev_or_prov) == "table" and ev_or_prov.name or ev_or_prov,
				endpoint = type(ev_or_prov) == "table" and ev_or_prov or nil,
				transport = cfg.transport,
				request = { url = url },
			}
		local prov_name = ev.provider
		local target_url = type(ev.request) == "table" and ev.request.url or url
		if not is_ws_target(ev) then
			return false
		end
		local t = (ev.transport ~= nil and ev.transport) or cfg.transport or "auto"
		local scope = ws_pool_key(ev, ev.request)
		if t == "http" then
			return false
		end
		if ws_off[scope] then
			local until_ms = ws_off_until[scope]
			if until_ms and mare.task.clock() >= until_ms then
				ws_off[scope] = nil
				ws_off_until[scope] = nil
				ws_failures[scope] = 0
			else
				return false
			end
		end
		if t == "websocket" then
			return true
		end
		local websocket_ok = (ev.endpoint and ev.endpoint.websocket) or prov_name == "openai"
		if t ~= "auto" or not websocket_ok then
			return false
		end
		-- the WebSocket client doesn't go through HTTP proxies: keep HTTP when one is set
		if effective_proxy(prov_name, ev.proxy, ev.request) ~= "" then
			return false
		end
		for _, k in ipairs({ "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy" }) do
			if (mare.env(k) or "") ~= "" then
				return false
			end
		end
		local host = type(target_url) == "string" and target_url:match("^https?://([^/:]+)") or nil
		return host == "api.openai.com" or host == "chatgpt.com"
	end
	--- a response-like object over a (reused) WebSocket, or nil + reason to fall back to HTTP
	local function ws_response(ev_or_prov, opt_wire, opt_req, opt_can_cap_reply)
		local ev = (type(ev_or_prov) == "table" and ev_or_prov.request ~= nil) and ev_or_prov
			or {
				provider = type(ev_or_prov) == "table" and ev_or_prov.name or ev_or_prov,
				label = type(ev_or_prov) == "table" and ev_or_prov.label or nil,
				request = opt_wire,
				session_id = opt_req and opt_req.session_id or nil,
				cancel = opt_req and opt_req.cancel or nil,
				fresh = opt_req and opt_req.fresh_connection or nil,
				idle_timeout_ms = opt_req and opt_req.idle_timeout_ms or nil,
				connect_timeout_ms = opt_req and opt_req.connect_timeout_ms or nil,
				no_reply_left = opt_can_cap_reply == false,
			}
		local prov_name = ev.provider or "openai"
		local prov_label = ev.label or prov_name
		local wire = ev.request
		local key = ws_pool_key(ev, wire)
		ensure_scope_meta(key, ev)
		-- repeated mid-stream failures: give up on WebSocket for this scope (falls back to HTTPS
		-- once WebSocket retries are used up)
		if (ws_failures[key] or 0) >= 2 then
			return nil, "the WebSocket failed twice in a row"
		end
		local headers, _, turn_state_val = normalize_headers(wire.headers)
		local frame = { type = "response.create" }
		for k, v in pairs(wire.body or {}) do
			frame[k] = v
		end
		if turn_state_val then
			local meta = type(frame.client_metadata) == "table" and table.copy(frame.client_metadata) or {}
			meta["x-codex-turn-state"] = turn_state_val
			frame.client_metadata = meta
		end
		local text = mare.json.encode(frame)
		if disposed then
			return nil, { aborted = true, message = "openai websocket pool disposed" }
		end
		local cancel = ev.cancel ---@type mare.Token|nil
		if cancel and cancel:is_cancelled() then
			return nil, { aborted = true, message = "request cancelled" }
		end
		if ev.fresh then
			bump_gen(key)
		end
		local gen_at_start = current_gen(key)
		local entry = ws_conns[key]
		if ev.fresh and entry then
			if entry.busy then
				-- Detach from pool without closing the active caller's socket so its stream
				-- completes cleanly through its terminal frame and closes on release.
				entry.pooled = false
				ws_conns[key] = nil
			else
				evict_conn(key, entry, true)
			end
			entry = nil
		end
		if entry and entry.conn:is_closed() then
			evict_conn(key, entry, false)
			entry = nil
		end
		-- Lease active pooled sockets through their terminal frame so overlapping requests never
		-- interleave reads/writes on the same WebSocket stream. Guard setup across yielding send/dial
		-- so coroutine aborts never leak `entry.busy` or `pending_dials`.
		local setup_entry
		local _setup_guard <close> = require("mare_lib").guard(function()
			if setup_entry then
				local se = setup_entry
				setup_entry = nil
				se.busy = false
				active_entries[se] = nil
				evict_conn(key, se, true)
			end
		end)
		local overflow = entry ~= nil and entry.busy == true
		if entry and not overflow then
			entry.busy = true
			active_entries[entry] = true
			setup_entry = entry
			-- a connection the server dropped between turns: reconnect once (unless retired)
			if not pcall(entry.conn.send, entry.conn, text) then
				local retired_now = disposed
					or entry.retired
					or current_gen(key) ~= gen_at_start
					or (cancel and cancel:is_cancelled())
				setup_entry = nil
				evict_conn(key, entry, true)
				entry = nil
				if retired_now then
					return nil, { aborted = true, message = "websocket scope retired during send" }
				end
			end
		end
		if not entry or overflow then
			local dial = {
				key = key,
				gen = gen_at_start,
				session_id = tostring(ev.session_id or ""),
				provider = prov_name,
				cancelled = false,
			}
			pending_dials[dial] = true
			local _dial_guard <close> = require("mare_lib").guard(function()
				pending_dials[dial] = nil
				prune_gen(key)
			end)
			local ok, c = pcall(mare.http.websocket, {
				url = ws_url(wire.url),
				headers = headers,
				timeout_ms = ev.connect_timeout_ms
					or (type(wire) == "table" and wire.connect_timeout_ms)
					or cfg.websocket_connect_timeout_ms,
			})
			pending_dials[dial] = nil
			_dial_guard:disarm()
			local cancelled_now = (cancel and cancel:is_cancelled()) or false
			if not ok then
				local retired = disposed or dial.cancelled or current_gen(key) ~= gen_at_start or cancelled_now
				prune_gen(key)
				if retired then
					return nil, { aborted = true, message = "websocket scope retired during connect" }
				end
				return nil, tostring(c)
			end
			if disposed or dial.cancelled or current_gen(key) ~= gen_at_start or cancelled_now then
				pcall(c.close, c)
				prune_gen(key)
				return nil, { aborted = true, message = "websocket scope retired during connect" }
			end
			local can_pool = (ws_conns[key] == nil) and not overflow
			entry = {
				conn = c,
				key = key,
				session_id = dial.session_id,
				provider = prov_name,
				gen = gen_at_start,
				busy = true,
				pooled = can_pool,
			}
			active_entries[entry] = true
			setup_entry = entry
			if can_pool then
				ws_conns[key] = entry
			end
			local sok, serr = pcall(c.send, c, text)
			if not sok then
				local retired_now = disposed
					or dial.cancelled
					or entry.retired
					or current_gen(key) ~= gen_at_start
					or (cancel and cancel:is_cancelled())
				setup_entry = nil
				evict_conn(key, entry, true)
				if retired_now then
					return nil, { aborted = true, message = "websocket scope retired during send" }
				end
				return nil, tostring(serr)
			end
		end
		setup_entry = nil
		_setup_guard:disarm()
		local conn = entry.conn
		local done = false
		local resp = { ok = true, status = 200, headers = {}, transport = "websocket" }
		local function release_entry(close_socket)
			entry.busy = false
			active_entries[entry] = nil
			if close_socket or not entry.pooled then
				evict_conn(key, entry, true)
			end
		end
		function resp:close()
			if not done then
				release_entry(true)
				done = true
			end
		end
		function resp:next_event()
			if done then
				return nil
			end
			local rok, msg, why = pcall(
				conn.recv,
				conn,
				{ idle_ms = ev.idle_timeout_ms or cfg.stream_idle_timeout_ms, cancel = ev.cancel }
			)
			if not rok then
				local was_retired = entry.retired == true
				release_entry(true)
				if
					not disposed
					and not was_retired
					and current_gen(key) == entry.gen
					and not tostring(msg):find("aborted", 1, true)
				then
					ws_failures[key] = (ws_failures[key] or 0) + 1
				end
				error(msg, 0)
			end
			if not msg then
				local was_retired = entry.retired == true
				if not disposed and not was_retired and current_gen(key) == entry.gen then
					ws_failures[key] = (ws_failures[key] or 0) + 1
				end
				-- closed before a terminal event: the parser reports a retryable stream_closed
				release_entry(true)
				done = true
				mare.log("debug", "openai websocket closed: " .. tostring(why))
				return nil
			end
			local j = mare.json.decode(msg)
			if type(j) == "table" then
				-- An error event ends the stream; invalidate and close the cached connection even
				-- when there is no status/status_code so trailing frames are never reused.
				if j.type == "error" then
					release_entry(true)
					done = true
					if j.status or j.status_code then
						local err = type(j.error) == "table" and j.error or {}
						local st = j.status or j.status_code
						error(llm:stream_error(prov_label, {
							message = err.message or msg,
							code = err.code,
							type = err.type,
							status = st,
							headers = err.headers,
							plan_type = err.plan_type,
							resets_at = err.resets_at,
						}, {
							source = "transport",
							provider = prov_name,
							wire_errors = M.ERRORS,
							no_reply_left = ev.no_reply_left == true,
						}))
					end
				end
				if TERMINAL[j.type] then
					done = true
					local was_retired = entry.retired == true
					release_entry(not entry.pooled)
					if not disposed and not was_retired and current_gen(key) == entry.gen then
						ws_failures[key] = 0
					end
				end
			end
			return { event = "message", data = msg }
		end
		return resp
	end

	local function handle_transport(ev)
		if type(ev) ~= "table" or not ws_wanted(ev) then
			return nil
		end
		local scope = ws_pool_key(ev, ev.request)
		local gen_before = current_gen(scope)
		local r, why = ws_response(ev)
		if r then
			return r
		end
		if type(why) == "table" and why.aborted then
			return { aborted = true, message = why.message or "websocket request aborted" }
		end
		if type(why) == "string" and why:match("HTTP 401") then
			return {
				ok = false,
				status = 401,
				headers = {},
				transport = "websocket",
				text = function()
					return why
				end,
				close = function() end,
			}
		end
		-- Permanent upgrade rejections (HTTP 4xx/501) disable WS for this scope; transient connect
		-- or repeated mid-stream failures allow retry and expire after a monotonic cooldown.
		local gen_expected = ev.fresh and (gen_before + 1) or gen_before
		if not disposed and current_gen(scope) == gen_expected then
			ensure_scope_meta(scope, ev)
			if is_permanent_ws_rejection(why) then
				ws_off[scope] = why
				ws_off_until[scope] = nil
			else
				local fails = (ws_failures[scope] or 0) + 1
				ws_failures[scope] = fails
				if fails >= 2 then
					ws_off[scope] = why
					ws_off_until[scope] = mare.task.clock() + (M.WS_FALLBACK_COOLDOWN_MS or 60000)
				end
			end
		end
		mare.log("info", ("openai: WebSocket unavailable (%s), using HTTP"):format(why))
		return nil
	end

	ctx:on("session/changed", function(ev)
		if type(ev) == "table" and ev.previous_id ~= nil then
			local prev = tostring(ev.previous_id)
			close_scopes(function(meta)
				return meta.session_id == prev
			end)
		elseif type(ev) == "table" and ev.session_id ~= nil then
			local cur = tostring(ev.session_id)
			close_scopes(function(meta)
				return meta.session_id ~= cur
			end)
		else
			close_scopes(function()
				return true
			end)
		end
	end)

	ctx:on("llm/logout", function(ev)
		local prov = type(ev) == "table" and ev.provider or nil
		if prov ~= nil and prov ~= "" then
			close_scopes(function(meta)
				return meta.provider == tostring(prov)
			end)
		else
			close_scopes(function()
				return true
			end)
		end
	end)

	ctx:effect(function()
		return function()
			disposed = true
			close_scopes(function()
				return true
			end)
		end
	end, "openai websocket pool cleanup")

	return { handle_transport = handle_transport, ws_off = ws_off, ws_wanted = ws_wanted, ws_response = ws_response }
end
