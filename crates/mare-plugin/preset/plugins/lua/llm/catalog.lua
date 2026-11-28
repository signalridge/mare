-- llm.catalog: model overrides, the cached provider model lists, models.dev and catalogue refresh;
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local providers, models, overrides, llm = D.providers, D.models, D.overrides, D.llm
	local lib = require("mare_lib")

	local model_sources = {}
	local SOURCE_FIELDS = {
		"name",
		"context_window",
		"max_output",
		"reasoning",
		"efforts",
		"default_effort",
		"truncation",
		"input",
		"images",
		"cost",
		"protocol",
		"edit_tool",
		"tool_update",
		"hidden",
	}

	local function wire_specs_of(p)
		if type(p) ~= "table" or not (D.routes and D.routes.wire_specs_for_provider) then
			return {}
		end
		return D.routes.wire_specs_for_provider(p) or {}
	end

	local function wire_model_fields(p)
		local out = {}
		for _, ws in ipairs(wire_specs_of(p)) do
			if type(ws) == "table" and type(ws.model_fields) == "table" then
				for k, v in pairs(ws.model_fields) do
					out[k] = v
				end
			end
		end
		return out
	end

	local function init_sources(ref, m, base_source, p)
		local src = {}
		for _, k in ipairs(SOURCE_FIELDS) do
			if m[k] ~= nil then
				src[k] = base_source
			end
		end
		for k in pairs(wire_model_fields(p)) do
			if m[k] ~= nil then
				src[k] = base_source
			end
		end
		if m.context_window_measured == true and m.context_window ~= nil then
			src.context_window = "measured"
		end
		model_sources[ref] = src
		return src
	end

	--- `max_output_default`: the model's max_output is the provider's fallback, not a known limit
	--- (the Responses wire then sends no output cap); a configured max_output clears it
	local function apply_overrides(m, src_map)
		local src = src_map or model_sources[m.ref] or {}
		local p = m.provider and providers[m.provider]
		if p and m.tool_update == nil and src.tool_update == nil then
			if p.tool_update == "addition_only" then
				m.tool_update = "addition_only"
			elseif type(p.model_defaults) == "table" and D.matching_model_globs then
				local matches = D.matching_model_globs(p.model_defaults, m, true)
				for i = #matches, 1, -1 do
					local v = type(matches[i].value) == "table" and matches[i].value.tool_update
					if v == "addition_only" or v == false then
						m.tool_update = v
						break
					end
				end
			end
		end
		local o = overrides[m.ref]
		if o then
			for k, v in pairs(o) do
				m[k] = v
				src[k] = "user"
			end
			if o.max_output ~= nil then
				m.max_output_default = nil
			end
		end
		if m.tool_update == false then
			m.tool_update = nil
		end
		local derived = D.lib.model_images(m)
		if derived ~= nil then
			m.images = derived
			if src.images == nil and src.input ~= nil then
				src.images = src.input
			end
		end
		if m.ref then
			model_sources[m.ref] = src
		end
		return m
	end

	-- model catalogue: listed models add to the static table; same-name built-in models merge
	-- server-side reasoning capabilities and defaults while keeping their curated metadata.
	local catalog_dir = mare.paths.state .. "/models-cache"
	local MAX_CACHED_SCOPES = 4
	local static_models = {}
	local refresh_gen = {}
	local active_scopes = {}
	local pending_catalog = {}
	local scheduled_catalog = {}
	local hydrated_gen = {}
	local catalog_locks = {}

	local function has_catalog(p)
		return p ~= nil and (type(p.catalog) == "function" or type(p.list_models) == "function")
	end

	local function with_catalog_lock(name, fn)
		local l = catalog_locks[name]
		if not l then
			local tx, rx = mare.task.channel()
			tx:send(true)
			l = { tx = tx, rx = rx }
			catalog_locks[name] = l
		end
		local done_tx, done_rx = mare.task.channel()
		mare.task.spawn(function()
			l.rx:recv()
			local release_guard <close> = setmetatable({}, {
				__close = function()
					l.tx:send(true)
				end,
			})
			local ok, res = pcall(fn)
			pcall(done_tx.send, done_tx, { ok = ok, res = res })
		end)
		local msg = done_rx:recv()
		if not msg.ok then
			error(msg.res, 0)
		end
		return msg.res
	end

	local function clone_model(src)
		local copy = {}
		for k, v in pairs(src) do
			copy[k] = v
		end
		return copy
	end

	local function catalog_path(name)
		return ("%s/%s.json"):format(catalog_dir, (name:gsub("[^%w_%-]", "_")))
	end

	local function normalize_url(raw)
		if type(raw) ~= "string" or raw == "" then
			return nil
		end
		local ok, norm = pcall(mare.http.build_url, raw)
		return ok and type(norm) == "string" and norm or raw
	end

	local function catalog_target(p)
		if type(p.catalog_target) == "function" then
			local ok, t = pcall(p.catalog_target)
			if ok and t ~= nil then
				return t
			end
		end
		if type(p.catalog) == "function" then
			local ok, desc = pcall(p.catalog, false)
			if ok and type(desc) == "table" then
				if desc.target ~= nil then
					return desc.target
				end
				if type(desc.url) == "string" and desc.url ~= "" then
					return {
						url = normalize_url(desc.url),
						headers = desc.target_headers or desc.headers,
						query_params = desc.query_params,
					}
				end
			end
		end
		local pc = llm.provider_config and llm.provider_config(llm, nil, p.name) or {}
		local base = llm.base_url and llm.base_url(llm, nil, p.name, p.base_url) or pc.base_url or p.base_url
		return {
			base_url = normalize_url(base),
			headers = pc.headers,
			query_params = pc.query_params,
		}
	end

	local function hash_scope(auth_scope, target)
		if not auth_scope then
			return nil
		end
		return mare.crypto.sha256(mare.json.encode_strict({
			auth = auth_scope,
			target = target,
		}))
	end

	local function catalog_scope(p)
		if not p then
			return nil
		end
		local auth_scope
		local custom_scope = type(p.catalog_scope) == "function" and p.catalog_scope() or nil
		if custom_scope ~= nil then
			auth_scope = custom_scope
		else
			if type(p.is_configured) == "function" and not p.is_configured() then
				return nil
			end
			if type(D.credential_scope) == "function" then
				auth_scope = D.credential_scope(p.name)
			end
			-- If `D.credential_scope` returns nil because an api_key command is unresolved, do NOT
			-- synthesize a "default" scope (unparsed command credentials must not hit stale cache).
			local pcfg = llm.provider_config and llm.provider_config(llm, nil, p.name) or {}
			if not auth_scope and type(pcfg.api_key) == "table" and pcfg.api_key.cmd then
				return nil
			end
			if not auth_scope then
				if type(p.auth_mode) == "function" then
					auth_scope = p.auth_mode() or "default"
				else
					auth_scope = "default"
				end
			end
		end
		return hash_scope(auth_scope, catalog_target(p))
	end

	local function parse_catalog(raw, scope)
		if type(raw) ~= "string" or raw == "" or not scope then
			return nil
		end
		local ok, j = pcall(mare.json.decode, lib.strip_utf8_bom(raw))
		if not (ok and type(j) == "table") then
			return nil
		end
		if type(j.scopes) == "table" then
			local entry = j.scopes[scope]
			return type(entry) == "table" and type(entry.models) == "table" and entry or nil
		end
		if type(j.models) == "table" then
			if j.scope == nil or j.scope == scope then
				return j
			end
		end
		return nil
	end

	local function read_catalog(name, scope)
		if not scope then
			return nil
		end
		local ok, raw = pcall(mare.fs.read, catalog_path(name))
		if not ok then
			return nil
		end
		return parse_catalog(raw, scope)
	end

	local function write_catalog(name, scope, list)
		if mare.mode == "child" or not scope then
			return
		end
		local ok, existing = pcall(function()
			return mare.json.decode(lib.strip_utf8_bom(mare.fs.read(catalog_path(name)) or ""))
		end)
		local scopes = ok and type(existing) == "table" and type(existing.scopes) == "table" and existing.scopes or {}
		scopes[scope] = { fetched_at = os.time(), models = list }
		local others = {}
		for k in pairs(scopes) do
			if k ~= scope then
				others[#others + 1] = k
			end
		end
		if #others >= MAX_CACHED_SCOPES then
			table.sort(others, function(a, b)
				local ta = type(scopes[a]) == "table" and tonumber(scopes[a].fetched_at) or 0
				local tb = type(scopes[b]) == "table" and tonumber(scopes[b].fetched_at) or 0
				if ta ~= tb then
					return ta > tb
				end
				return a < b
			end)
			for i = MAX_CACHED_SCOPES, #others do
				scopes[others[i]] = nil
			end
		end
		mare.fs.write(catalog_path(name), mare.json.encode({ scopes = scopes }), { atomic = true, mode = 384 })
	end

	local function deep_equal(a, b, seen)
		if a == b then
			return true
		end
		if type(a) ~= "table" or type(b) ~= "table" then
			return false
		end
		seen = seen or {}
		if seen[a] and seen[a][b] then
			return true
		end
		seen[a] = seen[a] or {}
		seen[a][b] = true
		for k, v in pairs(a) do
			if not deep_equal(v, b[k], seen) then
				return false
			end
		end
		for k in pairs(b) do
			if a[k] == nil then
				return false
			end
		end
		return true
	end

	local function model_changed(prev, next_m)
		return not deep_equal(prev, next_m)
	end

	-- models.dev v1 SDK packages → wire protocols (v2 protocol keys map the same way)
	local NPM_PROTOCOL = {
		["@ai-sdk/openai"] = "responses",
		["@ai-sdk/anthropic"] = "messages",
		["@ai-sdk/openai-compatible"] = "chat-completions",
		["@ai-sdk/google"] = "generate-content",
	}

	local function models_dev_id(p, fallback_name)
		if not p or p.models_dev == false then
			return nil
		end
		if type(p.models_dev) == "string" and p.models_dev ~= "" then
			return p.models_dev
		end
		return fallback_name or p.name
	end

	-- models.dev: only read
	-- when a provider list is refreshed (background), never on the startup path
	local mdev_path = catalog_dir .. "/models.dev.json"
	local mdev_mem
	local function models_dev(force)
		if cfg.models_dev == false then
			return nil
		end
		local c = mdev_mem
		if not c then
			local ok, j = pcall(function()
				return mare.json.decode(lib.strip_utf8_bom(mare.fs.read(mdev_path) or ""))
			end)
			c = ok and type(j) == "table" and type(j.providers) == "table" and j or nil
		end
		local delta = c and (os.time() - (tonumber(c.fetched_at) or 0)) or nil
		local age = (delta and delta >= 0) and (delta * 1000) or math.huge
		-- development builds (tests) don't reach the real models.dev unless pointed elsewhere
		local default_url = cfg.models_dev_url == "https://models.dev/api.json"
		if
			mare.mode ~= "child"
			and (force or age >= (cfg.catalog_refresh_ms or 0))
			and not (mare.debug_build and default_url)
		then
			local ok, r = pcall(mare.http.request, { method = "GET", url = cfg.models_dev_url, timeout_ms = 15000 })
			local j = ok and r.ok and r:json()
			if type(j) == "table" then
				local trimmed = {}
				for _, p in pairs(providers) do
					local pid = models_dev_id(p)
					local pv = pid and j[pid] or nil
					if type(pv) == "table" and type(pv.models) == "table" then
						local ms = {}
						for id, e in pairs(pv.models) do
							if type(e) == "table" then
								-- a model's own `provider.npm` overrides the provider's
								local npm = type(e.provider) == "table" and e.provider.npm or pv.npm
								ms[id] = {
									protocol = NPM_PROTOCOL[npm or ""],
									name = e.name,
									reasoning = e.reasoning,
									interleaved = e.interleaved,
									limit = e.limit,
									input = type(e.modalities) == "table" and e.modalities.input or nil,
									cost = e.cost,
								}
							end
						end
						trimmed[pid] = ms
					end
				end
				c = { fetched_at = os.time(), providers = trimmed }
				if mare.mode ~= "child" then
					pcall(mare.fs.write, mdev_path, mare.json.encode(c), { atomic = true, mode = 384 })
				end
			elseif not ok or not (r and r.ok) then
				mare.log("info", "models.dev: " .. tostring(ok and r.status or r))
			end
		end
		mdev_mem = c
		return c
	end
	--- fill what a listing didn't say from a models.dev entry (never overriding the listing)
	local function enrich(p, l, e, src_map)
		if type(e) ~= "table" then
			return
		end
		local function mark(k)
			if src_map and src_map[k] == nil then
				src_map[k] = "models_dev"
			end
		end
		local lim = type(e.limit) == "table" and e.limit or {}
		if l.name == nil and type(e.name) == "string" then
			l.name = e.name
			mark("name")
		end
		if l.context_window == nil and tonumber(lim.context) then
			l.context_window = tonumber(lim.context)
			mark("context_window")
		end
		if l.max_output == nil and tonumber(lim.output) then
			l.max_output = tonumber(lim.output)
			mark("max_output")
		end
		if l.protocol == nil and type(e.protocol) == "string" then
			l.protocol = e.protocol
			mark("protocol")
		end
		if l.reasoning == nil and e.reasoning ~= nil then
			l.reasoning = e.reasoning == true
			mark("reasoning")
		end
		for _, ws in ipairs(wire_specs_of(p)) do
			if type(ws) == "table" and type(ws.enrich_model) == "function" then
				ws.enrich_model(l, e, mark)
			end
		end
		if l.input == nil and type(e.input) == "table" then
			local inp = {}
			for _, x in ipairs(e.input) do
				if x == "text" or x == "image" then
					inp[#inp + 1] = x
				end
			end
			if #inp > 0 then
				l.input = inp
				mark("input")
			end
		end
		if l.cost == nil and type(e.cost) == "table" then
			l.cost = {
				input = tonumber(e.cost.input),
				output = tonumber(e.cost.output),
				cache_read = tonumber(e.cost.cache_read),
				cache_write = tonumber(e.cost.cache_write),
			}
			mark("cost")
		end
	end

	local function merge_static_capabilities(p, base, l, md_entry)
		local m = clone_model(base)
		local src = init_sources(m.ref, base, "static", p)
		if type(l.hidden) == "boolean" then
			m.hidden = l.hidden or nil
			src.hidden = "server"
		end
		if not (base.context_window_measured == true and base.context_window ~= nil) and tonumber(l.context_window) then
			m.context_window = tonumber(l.context_window)
			src.context_window = "server"
		end
		if tonumber(l.max_output) then
			m.max_output = tonumber(l.max_output)
			m.max_output_default = nil
			src.max_output = "server"
		end
		if type(l.input) == "table" then
			local copy = {}
			for i, v in ipairs(l.input) do
				copy[i] = v
			end
			m.input = copy
			src.input = "server"
		end
		if type(l.images) == "boolean" then
			m.images = l.images
			src.images = "server"
		end
		if type(l.truncation) == "table" then
			m.truncation = l.truncation
			src.truncation = "server"
		end
		if type(l.edit_tool) == "string" then
			m.edit_tool = l.edit_tool
			src.edit_tool = "server"
		end
		if type(l.cost) == "table" then
			m.cost = l.cost
			src.cost = "server"
		end
		if type(l.protocol) == "string" then
			m.protocol = l.protocol
			src.protocol = "server"
		end
		if l.tool_update == "addition_only" or l.tool_update == false then
			m.tool_update = l.tool_update
			src.tool_update = "server"
		end
		if l.reasoning == false then
			m.reasoning = false
			m.efforts = nil
			m.default_effort = nil
			src.reasoning = "server"
			src.efforts = "server"
			src.default_effort = "server"
		elseif l.reasoning == true then
			m.reasoning = true
			src.reasoning = "server"
			if type(l.efforts) == "table" then
				local copy = {}
				for i, v in ipairs(l.efforts) do
					copy[i] = v
				end
				m.efforts = copy
				src.efforts = "server"
			end
			if type(l.default_effort) == "string" then
				m.default_effort = l.default_effort
				src.default_effort = "server"
			end
		else
			if type(l.efforts) == "table" then
				local copy = {}
				for i, v in ipairs(l.efforts) do
					copy[i] = v
				end
				m.efforts = copy
				src.efforts = "server"
			end
		end
		if
			l.reasoning ~= false
			and l.efforts == nil
			and type(l.default_effort) == "string"
			and l.default_effort ~= ""
			and type(m.efforts) == "table"
		then
			local seen = false
			local copy = {}
			for i, v in ipairs(m.efforts) do
				copy[i] = v
				if v == l.default_effort then
					seen = true
				end
			end
			if not seen then
				copy[#copy + 1] = l.default_effort
				m.efforts = copy
				src.efforts = "server"
			end
		end
		for k in pairs(wire_model_fields(p)) do
			if l[k] ~= nil then
				m[k] = l[k]
				src[k] = "server"
			end
		end
		if type(md_entry) == "table" then
			enrich(p, m, md_entry, src)
		end
		return apply_overrides(m, src)
	end

	--- a listed model keeps what the listing (merged with models.dev) says — reasoning, efforts,
	--- input modalities, price, limits; only what nobody knows gets the conservative defaults.
	--- A static entry keeps its curated metadata and merges server capabilities/defaults.
	local function add_listed(p, list, replace, pm)
		if has_catalog(p) and active_scopes[p.name] == nil and not replace then
			active_scopes[p.name] = catalog_scope(p)
		end
		local added, removed, changed = 0, 0, 0
		if replace then
			local keep = {}
			for _, l in ipairs(list or {}) do
				if type(l) == "table" and type(l.id) == "string" then
					keep[p.name .. "/" .. l.id] = true
				end
			end
			for ref, m in pairs(models) do
				if m.provider == p.name and not keep[ref] then
					if m.listed then
						models[ref] = nil
						model_sources[ref] = nil
						removed = removed + 1
					elseif static_models[ref] then
						local restored = apply_overrides(
							clone_model(static_models[ref]),
							init_sources(ref, static_models[ref], "static", p)
						)
						if model_changed(m, restored) then
							changed = changed + 1
						end
						models[ref] = restored
					end
				end
			end
		end
		local wfields = wire_model_fields(p)
		for _, l in ipairs(list or {}) do
			local ref = type(l) == "table" and type(l.id) == "string" and (p.name .. "/" .. l.id)
			if ref and static_models[ref] then
				local prev = models[ref]
				local merged = merge_static_capabilities(p, static_models[ref], l, pm and pm[l.id])
				if model_changed(prev, merged) then
					changed = changed + 1
				end
				models[ref] = merged
			elseif ref and (not models[ref] or models[ref].listed) then
				local prev = models[ref]
				if not prev then
					added = added + 1
				end
				local images
				if type(l.images) == "boolean" then
					images = l.images
				end
				local src = init_sources(ref, l, "server", p)
				for k, sname in pairs(l._sources or {}) do
					src[k] = sname
				end
				if l.name == nil then
					src.name = "default"
				end
				if tonumber(l.context_window) == nil then
					src.context_window = "default"
				end
				if tonumber(l.max_output) == nil then
					src.max_output = "default"
				end
				local next_entry = {
					id = l.id,
					name = l.name or l.id,
					provider = p.name,
					ref = ref,
					listed = true,
					hidden = l.hidden == true or nil,
					context_window = tonumber(l.context_window) or p.default_context or 128000,
					max_output = tonumber(l.max_output) or p.default_max_output or 16384,
					max_output_default = tonumber(l.max_output) == nil or nil,
					reasoning = l.reasoning,
					truncation = type(l.truncation) == "table" and l.truncation or nil,
					efforts = type(l.efforts) == "table" and l.efforts or nil,
					default_effort = type(l.default_effort) == "string" and l.default_effort or nil,
					edit_tool = type(l.edit_tool) == "string" and l.edit_tool or nil,
					input = type(l.input) == "table" and l.input or nil,
					images = images,
					cost = type(l.cost) == "table" and l.cost or nil,
					-- models.dev's wire protocol for the model (llm/routes layer 4)
					protocol = type(l.protocol) == "string" and l.protocol or nil,
					tool_update = (l.tool_update == "addition_only" or l.tool_update == false) and l.tool_update or nil,
				}
				for k in pairs(wfields) do
					if l[k] ~= nil then
						next_entry[k] = l[k]
					end
				end
				local next_m = apply_overrides(next_entry, src)
				if prev and model_changed(prev, next_m) then
					changed = changed + 1
				end
				models[ref] = next_m
			end
		end
		return added, removed, changed
	end

	local function is_array(t)
		if type(t) ~= "table" then
			return false
		end
		local n = #t
		local count = 0
		for k in pairs(t) do
			if type(k) ~= "number" or k < 1 or k > n or k % 1 ~= 0 then
				return false
			end
			count = count + 1
		end
		return count == n
	end

	--- Complete all yieldable scope/identity reads before checking provider + generation without yields.
	local function same_refresh_state(name, p, gen, scope)
		local cur_scope = catalog_scope(p)
		return providers[name] == p and (refresh_gen[name] or 0) == (gen or 0) and cur_scope == scope
	end

	local function invalidate(name)
		if not name then
			return
		end
		pending_catalog[name] = nil
		scheduled_catalog[name] = nil
		refresh_gen[name] = (refresh_gen[name] or 0) + 1
		local inv_gen = refresh_gen[name]
		local changed = false
		for ref, m in pairs(models) do
			if m.provider == name then
				if m.listed then
					models[ref] = nil
					changed = true
				elseif static_models[ref] then
					models[ref] = apply_overrides(clone_model(static_models[ref]))
					changed = true
				end
			end
		end
		local p = providers[name]
		if has_catalog(p) then
			local scope = catalog_scope(p)
			local c = read_catalog(name, scope)
			if same_refresh_state(name, p, inv_gen, scope) then
				active_scopes[name] = scope
				local pid = models_dev_id(p, name)
				local pm = pid and mdev_mem and mdev_mem.providers and mdev_mem.providers[pid] or nil
				local added = add_listed(p, c and c.models, false, pm)
				if added > 0 then
					changed = true
				end
			end
		else
			active_scopes[name] = nil
		end
		if changed then
			hydrated_gen[name] = (hydrated_gen[name] or 0) + 1
			ctx.bus:emit("llm/providers-changed")
		end
	end
	ctx:on("llm/login", function(ev)
		invalidate(type(ev) == "table" and ev.provider or nil)
	end)
	ctx:on("llm/logout", function(ev)
		invalidate(type(ev) == "table" and ev.provider or nil)
	end)

	function llm.invalidate_catalog(_, _, name)
		if name then
			invalidate(name)
		else
			for pname in pairs(providers) do
				invalidate(pname)
			end
		end
	end

	function llm.model_sources(_, caller, ref)
		local m
		if ref == nil then
			m = llm.current(llm, caller)
		elseif type(ref) == "string" then
			m = llm.resolve(llm, caller, ref)
		elseif type(ref) == "table" and ref.ref then
			m = ref
		end
		if not m or not m.ref then
			return nil
		end
		local src = model_sources[m.ref]
		if not src then
			return nil
		end
		local copy = {}
		for k, v in pairs(src) do
			copy[k] = v
		end
		return copy
	end

	ctx:on("kernel/doctor", function(checks)
		local m = llm.current(llm, ctx)
		local src = m and m.ref and model_sources[m.ref]
		if not src then
			return
		end
		local keys = {}
		for k in pairs(src) do
			keys[#keys + 1] = k
		end
		table.sort(keys)
		local parts = {}
		for _, k in ipairs(keys) do
			parts[#parts + 1] = ("%s=%s"):format(k, src[k])
		end
		checks[#checks + 1] = {
			name = "model sources",
			ok = true,
			detail = m.ref .. ": " .. table.concat(parts, ", "),
		}
	end)

	local function origin_of(url)
		if type(url) ~= "string" then
			return nil
		end
		local scheme, rest = url:match("^([%a][%w+.-]*)://([^/?#]+)")
		if not scheme or not rest then
			return nil
		end
		local host, port = rest:match("^([^:]+):?(.*)$")
		if not host or host == "" then
			return nil
		end
		scheme = scheme:lower()
		host = host:lower()
		if (scheme == "http" and port == "80") or (scheme == "https" and port == "443") then
			port = ""
		end
		return port ~= "" and (scheme .. "://" .. host .. ":" .. port) or (scheme .. "://" .. host)
	end

	local function fetch_catalog(p)
		if type(p.catalog) ~= "function" then
			return p.list_models()
		end
		local desc = p.catalog(true)
		if type(desc) ~= "table" or type(desc.url) ~= "string" or desc.url == "" then
			return nil
		end
		if desc.auth ~= nil and not desc.key and not desc.no_key then
			return nil
		end
		local headers = { accept = "application/json" }
		for k, v in pairs(desc.headers or {}) do
			headers[k] = v
		end
		local auto_auth
		if desc.key and desc.key ~= "" and desc.auth then
			local auth_name = desc.auth == "bearer" and "authorization" or tostring(desc.auth)
			local auth_val = desc.auth == "bearer" and ("Bearer " .. desc.key) or desc.key
			auto_auth = desc.auth == "bearer" and auth_val or nil
			for existing in pairs(headers) do
				if string.lower(tostring(existing)) == string.lower(auth_name) then
					headers[existing] = nil
				end
			end
			headers[auth_name] = auth_val
		end
		local wire = {
			provider = p.name,
			method = desc.method or "GET",
			url = desc.url,
			headers = headers,
		}
		wire = ctx.bus:waterfall("provider/request", wire) or wire
		if auto_auth and type(wire) == "table" and type(wire.headers) == "table" then
			for k, v in pairs(wire.headers) do
				if k ~= "authorization" and v ~= nil and string.lower(tostring(k)) == "authorization" then
					if wire.headers["authorization"] == auto_auth then
						wire.headers["authorization"] = nil
					end
					break
				end
			end
		end
		local label = desc.label or (p.name .. " GET /models")
		if desc.check_origin then
			local expected = origin_of(desc.url)
			local actual = origin_of(wire.url)
			if desc.validate_url then
				local ok_url, uerr = desc.validate_url(wire.url)
				if not ok_url then
					error(uerr or (label .. ": invalid url"), 0)
				end
			end
			if not expected or not actual or expected ~= actual then
				error(label .. ": provider/request changed origin", 0)
			end
		end
		local resp = mare.http.request({
			method = wire.method or "GET",
			url = wire.url,
			headers = wire.headers,
			proxy = wire.proxy,
			connect_timeout_ms = wire.connect_timeout_ms,
			timeout_ms = wire.timeout_ms or desc.timeout_ms or 15000,
			redirect = "same-origin",
		})
		if not resp.ok then
			error(("%s: HTTP %s"):format(label, tostring(resp.status)), 0)
		end
		local list = desc.decode(resp:text(), resp, wire)
		local snapshot = desc.snapshot and desc.snapshot(wire, auto_auth, desc) or nil
		return list, snapshot
	end

	-- model catalogue refresh: in the background after ui/ready (never on the startup path),
	-- for providers whose cache is older than catalog_refresh_ms; a failure keeps what's there
	function llm.refresh_models(_, _, name, opts)
		if mare.mode == "child" then
			return 0
		end
		local p = providers[name]
		if not has_catalog(p) then
			return 0
		end
		local scope = catalog_scope(p)
		local gen = (refresh_gen[name] or 0) + 1
		refresh_gen[name] = gen
		local list, snapshot = fetch_catalog(p)
		if type(list) ~= "table" or not is_array(list) then
			return 0
		end
		if type(snapshot) == "table" then
			if type(snapshot.scope) == "string" then
				scope = snapshot.scope
			elseif snapshot.auth ~= nil or snapshot.target ~= nil then
				scope = hash_scope(snapshot.auth, snapshot.target or catalog_target(p))
			end
		else
			scope = scope or catalog_scope(p)
		end
		if not scope or not same_refresh_state(name, p, gen, scope) then
			return 0
		end
		local md = (opts and opts._models_dev) or models_dev(opts and opts.force)
		if not same_refresh_state(name, p, gen, scope) then
			return 0
		end
		local pid = models_dev_id(p, name)
		local pm = pid and md and md.providers[pid] or nil
		local disk_list = {}
		for i, l in ipairs(list) do
			if type(l) == "table" and type(l.id) == "string" then
				local ref = name .. "/" .. l.id
				if not static_models[ref] then
					l._sources = {}
					for _, k in ipairs(SOURCE_FIELDS) do
						if l[k] ~= nil then
							l._sources[k] = "server"
						end
					end
					for k in pairs(wire_model_fields(p)) do
						if l[k] ~= nil then
							l._sources[k] = "server"
						end
					end
					enrich(p, l, pm and pm[l.id], l._sources)
				end
				local clean = {}
				for k, v in pairs(l) do
					if k ~= "_sources" then
						clean[k] = v
					end
				end
				disk_list[i] = clean
			else
				disk_list[i] = l
			end
		end
		return with_catalog_lock(name, function()
			if not same_refresh_state(name, p, gen, scope) then
				return 0
			end
			write_catalog(name, scope, disk_list)
			if not same_refresh_state(name, p, gen, scope) then
				return 0
			end
			pending_catalog[name] = nil
			active_scopes[name] = scope
			local added, removed, changed = add_listed(p, list, true, pm)
			if added > 0 or removed > 0 or changed > 0 then
				hydrated_gen[name] = (hydrated_gen[name] or 0) + 1
				ctx.bus:emit("llm/providers-changed")
			end
			return added
		end)
	end
	--- refresh_catalog{ name?, min_age_ms?, force?, configured?, on_open? } -> { [provider] = added | err }:
	--- the lists older than min_age_ms (default catalog_refresh_ms); `configured` only the
	--- providers with credentials (a listing needs them). Refresh points: after
	--- ui/ready, after /login, when /model opens, `mare model refresh`.
	function llm.refresh_catalog(_, caller, opts)
		if mare.mode == "child" then
			return {}
		end
		opts = opts or {}
		local out = {}
		local min_age = opts.force and 0
			or opts.min_age_ms
			or (opts.on_open and cfg.catalog_open_refresh_ms)
			or cfg.catalog_refresh_ms
			or 0
		local forced_mdev
		if opts.force then
			local ok_md, md_val = pcall(models_dev, true) -- once for all providers
			if ok_md then
				forced_mdev = md_val
			end
		end
		for name, p in pairs(providers) do
			local match = has_catalog(p) and (not opts.name or opts.name == name)
			local scope = match and catalog_scope(p) or nil
			if match and active_scopes[name] ~= scope then
				invalidate(name)
			end
			local wanted = match and (not opts.configured or not p.is_configured or p.is_configured())
			local c = wanted and read_catalog(name, scope)
			local delta = c and (os.time() - (tonumber(c.fetched_at) or 0)) or nil
			local age = (delta and delta >= 0) and (delta * 1000) or math.huge
			if wanted and age >= min_age then
				local ok, res =
					pcall(llm.refresh_models, llm, caller, name, forced_mdev and { _models_dev = forced_mdev } or nil)
				out[name] = ok and res or tostring(res)
				if not ok then
					mare.log("info", ("model catalogue %s: %s"):format(name, tostring(res)))
				end
			end
		end
		return out
	end

	local function hydrate_one(pname)
		if not pending_catalog[pname] then
			return false
		end
		local p = providers[pname]
		if not p or not has_catalog(p) then
			pending_catalog[pname] = nil
			return false
		end
		local before_hgen = hydrated_gen[pname] or 0
		with_catalog_lock(pname, function()
			if not pending_catalog[pname] then
				return
			end
			local cur_p = providers[pname]
			if cur_p ~= p or not has_catalog(cur_p) then
				pending_catalog[pname] = nil
				return
			end
			local gen = refresh_gen[pname] or 0
			local ok_read, raw = pcall(mare.fs.read, catalog_path(pname))
			if not ok_read then
				pending_catalog[pname] = nil
				mare.log("warn", ("model catalogue cache %s: %s"):format(pname, tostring(raw)))
				return
			end
			if type(raw) ~= "string" or raw == "" then
				if providers[pname] == cur_p and (refresh_gen[pname] or 0) == gen then
					pending_catalog[pname] = nil
				end
				return
			end
			local ok_scope, scope = pcall(catalog_scope, cur_p)
			if not ok_scope then
				pending_catalog[pname] = nil
				mare.log("warn", ("model catalogue cache %s: %s"):format(pname, tostring(scope)))
				return
			end
			if not scope then
				if providers[pname] == cur_p and (refresh_gen[pname] or 0) == gen then
					pending_catalog[pname] = nil
				end
				return
			end
			local c = parse_catalog(raw, scope)
			if not same_refresh_state(pname, cur_p, gen, scope) then
				return
			end
			pending_catalog[pname] = nil
			if c and c.models then
				active_scopes[pname] = scope
				local pid = models_dev_id(cur_p, pname)
				local pm = pid and mdev_mem and mdev_mem.providers and mdev_mem.providers[pid] or nil
				local added, removed, changed = add_listed(cur_p, c.models, false, pm)
				if added > 0 or removed > 0 or changed > 0 then
					hydrated_gen[pname] = (hydrated_gen[pname] or 0) + 1
					ctx.bus:emit("llm/providers-changed")
				end
			end
		end)
		return (hydrated_gen[pname] or 0) ~= before_hgen
	end

	local function ensure_loaded(name)
		if name ~= nil then
			return hydrate_one(name)
		end
		local names = {}
		for pname in pairs(pending_catalog) do
			names[#names + 1] = pname
		end
		table.sort(names)
		local any_changed = false
		for _, pname in ipairs(names) do
			if hydrate_one(pname) then
				any_changed = true
			end
		end
		return any_changed
	end

	local function schedule_one(pname)
		if not pending_catalog[pname] or scheduled_catalog[pname] or not D.startup_done then
			return
		end
		scheduled_catalog[pname] = true
		ctx:spawn(function()
			local _guard <close> = setmetatable({}, {
				__close = function()
					scheduled_catalog[pname] = nil
				end,
			})
			hydrate_one(pname)
		end)
	end

	local function schedule_hydrate(name)
		if name ~= nil then
			schedule_one(name)
			return
		end
		local names = {}
		for pname in pairs(pending_catalog) do
			names[#names + 1] = pname
		end
		table.sort(names)
		for _, pname in ipairs(names) do
			schedule_one(pname)
		end
	end

	return {
		apply_overrides = apply_overrides,
		read_catalog = read_catalog,
		add_listed = add_listed,
		catalog_scope = catalog_scope,
		mark_pending = function(name)
			pending_catalog[name] = true
			scheduled_catalog[name] = nil
		end,
		ensure_loaded = ensure_loaded,
		schedule_hydrate = schedule_hydrate,
		register_static = function(ref, m)
			local copy = clone_model(m)
			static_models[ref] = copy
			init_sources(ref, copy, "static", providers[m.provider])
		end,
		clear_provider = function(name)
			pending_catalog[name] = nil
			scheduled_catalog[name] = nil
			refresh_gen[name] = (refresh_gen[name] or 0) + 1
			active_scopes[name] = nil
			for ref, m in pairs(static_models) do
				if m.provider == name then
					static_models[ref] = nil
				end
			end
			for ref in pairs(model_sources) do
				if ref:match("^([^/]+)/") == name then
					model_sources[ref] = nil
				end
			end
		end,
	}
end
