-- llm.models: provider registry, model lookup and aliases, current model and effort;
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
return function(D)
	local ctx = D.ctx
	local providers, models, overrides, state, lib, llm = D.providers, D.models, D.overrides, D.state, D.lib, D.llm
	local EFFORT_INDEX = D.EFFORT_INDEX
	local params = D.params
	local apply_overrides = D.catalog.apply_overrides
	local validate = require("llm.validate")
	local provider_accessor_mod
	local function provider_accessor()
		if not provider_accessor_mod then
			provider_accessor_mod = require("llm.provider_accessor")(D)
		end
		return provider_accessor_mod
	end

	local function activate_provider(entry)
		entry._live = true
		entry._active_cancels = {}
		entry._wrapped_stream = nil
		entry._wrapped_stream_src = nil
		entry._wrapped_compact = nil
		entry._wrapped_compact_src = nil
		entry._accessor = nil
	end

	local function deactivate_provider(entry)
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

	function llm.provider_from_config(_, _, id, pc, opts)
		local wire_name = (type(pc) == "table" and pc.wire) or (type(opts) == "table" and opts.wire)
		local wire_obj = type(wire_name) == "string" and D.routes.get_wire(wire_name) or nil
		return validate.provider_from_config(id, pc, opts, wire_obj and wire_obj.spec or nil)
	end

	local function sanitize_wire_request(self_llm, caller, req)
		local out = {
			messages = {},
		}
		if type(req.model) == "table" then
			local desc = self_llm:capabilities(caller, req.model)
			if type(desc) == "table" then
				out.model = desc
			end
		end
		for _, k in ipairs({
			"system",
			"messages",
			"tools",
			"tool_choice",
			"max_tokens",
			"effort",
			"output_schema",
			"params",
			"native",
			"cache",
			"cache_retention",
			"session_id",
			"request_id",
			"no_fallback",
			"purpose",
			"subagent",
			"attempt",
			"header_timeout_ms",
			"idle_timeout_ms",
			"sse_max_event_bytes",
			"sse_max_line_bytes",
			"fresh_connection",
		}) do
			local v = req[k]
			if v ~= nil and type(v) ~= "function" and type(v) ~= "userdata" and type(v) ~= "thread" then
				out[k] = v
			end
		end
		return out
	end

	local function clear_provider_models(pname)
		D.catalog.clear_provider(pname)
		for ref, m in pairs(models) do
			if m.provider == pname then
				models[ref] = nil
			end
		end
	end

	local function populate_provider(pname, entry)
		providers[pname] = entry
		for _, m in ipairs(entry.models or {}) do
			m.provider = pname
			m.ref = pname .. "/" .. m.id
			D.catalog.register_static(m.ref, m)
			models[m.ref] = apply_overrides(m)
		end
		-- user-defined models for this provider: [models."provider/id"]
		for ref, o in pairs(overrides) do
			local prov, id = string.match(ref, "^([^/]+)/(.+)$")
			if prov == pname and not models[ref] then
				local m = {
					id = id,
					name = o.name or id,
					provider = prov,
					ref = ref,
					context_window = 128000,
					max_output = 16384,
					max_output_default = true,
				}
				models[ref] = apply_overrides(m)
			end
		end
		-- Defer disk catalog/auth reads out of apply(): hydrate lazily on first use or ui/ready.
		if entry.catalog or entry.list_models then
			D.catalog.mark_pending(pname)
		end
	end

	local function install_provider(pname, entry)
		if providers[pname] ~= nil then
			deactivate_provider(providers[pname])
			clear_provider_models(pname)
		end
		activate_provider(entry)
		populate_provider(pname, entry)
	end

	local function uninstall_provider(pname)
		local prev = providers[pname]
		providers[pname] = nil
		deactivate_provider(prev)
		clear_provider_models(pname)
	end

	ctx:effect(function()
		return function()
			for _, entry in pairs(providers) do
				deactivate_provider(entry)
			end
		end
	end, "llm provider accessors")

	local function normalize_entry(pname, p, is_rpc, self_llm, caller)
		local entry = table.copy(p)
		entry.name = pname
		if type(p.models) == "table" then
			local normalized_models = {}
			for i, src_m in ipairs(p.models) do
				local nm = table.copy(src_m)
				nm.id = nm.model_id
				normalized_models[i] = nm
			end
			entry.models = normalized_models
		end
		if is_rpc then
			entry.is_configured = function()
				return true
			end
			if type(entry.stream) == "function" then
				---@type fun(req: table, sink: fun(ev: mare.StreamEvent), cancel?: mare.Token): table
				local remote_stream = entry.stream
				entry.stream = function(req, sink)
					local wire_req = sanitize_wire_request(self_llm, caller, req)
					local msg = remote_stream(wire_req, sink, req.cancel)
					if type(msg) ~= "table" then
						error("remote provider stream did not return an assistant message", 0)
					end
					msg = table.copy(msg)
					msg.role = msg.role or "assistant"
					msg.content = type(msg.content) == "table" and msg.content or {}
					msg.provider_id = msg.provider_id or pname
					msg.model_id = msg.model_id or (req.model and req.model.id) or nil
					return msg
				end
			end
		end
		return entry
	end

	local function check_declared(caller_plugin, pname, replace)
		if not caller_plugin then
			return true
		end
		if not validate.is_declared_provider(caller_plugin, pname) then
			local owners = mare.plugins and mare.plugins.declaring and mare.plugins.declaring("providers", pname) or {}
			if #owners > 0 and not replace then
				local reason = ("provider %q is declared by plugin `%s`; set `replace = true` to override it"):format(
					pname,
					owners[1]
				)
				validate.report_problem(caller_plugin, "undeclared-provider:" .. pname, reason)
				return false, reason
			end
			validate.report_problem(
				caller_plugin,
				"undeclared-provider:" .. pname,
				("provider %q registered by plugin `%s` is not declared in `providers = { ... }`"):format(
					pname,
					caller_plugin
				)
			)
		else
			validate.clear_problem(caller_plugin, "undeclared-provider:" .. pname)
		end
		return true
	end

	local function register_one(self, caller, is_rpc, caller_plugin, p)
		local is_data = type(p) == "table"
			and p.stream == nil
			and p.endpoint == nil
			and (p.wire ~= nil or p.wire_api ~= nil)

		if is_data then
			local pname = p.provider
			if type(pname) ~= "string" or pname == "" then
				local reason = "register_provider needs a non-empty provider name"
				if is_rpc then
					lib.fail("invalid_arguments", reason)
				end
				validate.report_problem(caller_plugin, "provider:unnamed", reason)
				mare.log("warn", reason)
				return nil, reason
			end
			if providers[pname] ~= nil and p.replace ~= true then
				local reason = ("provider %q is already registered (set `replace = true` to replace it)"):format(pname)
				if is_rpc then
					lib.fail("invalid_arguments", reason)
				end
				validate.report_problem(caller_plugin, "provider:" .. pname, reason)
				mare.log("warn", reason)
				return nil, reason
			end
			local decl_ok, decl_err = check_declared(caller_plugin, pname, p.replace == true)
			if not decl_ok then
				mare.log("warn", tostring(decl_err))
				return nil, decl_err
			end
			return D.directory.push_data(pname, p, {
				origin = is_rpc and "rpc" or "plugin",
				plugin = caller_plugin,
				replace = p.replace == true,
			})
		end

		local wire_specs = D.routes.wire_specs_for_provider(p)
		local ok, err_key, reason, unknown = validate.provider(p, providers, wire_specs)
		if not ok then
			if is_rpc then
				lib.fail("invalid_arguments", reason)
			end
			validate.report_problem(caller_plugin, err_key, reason)
			mare.log("warn", reason)
			return nil, reason
		end
		local pname = p.provider
		if caller_plugin then
			validate.clear_problem(caller_plugin, "provider:" .. pname)
			for _, field in ipairs(unknown or {}) do
				validate.report_problem(
					caller_plugin,
					"provider-field:" .. pname .. ":" .. field,
					("provider %q: unknown field `%s`"):format(pname, field)
				)
			end
			local decl_ok, decl_err = check_declared(caller_plugin, pname, p.replace == true)
			if not decl_ok then
				mare.log("warn", tostring(decl_err))
				return nil, decl_err
			end
		end
		local entry = normalize_entry(pname, p, is_rpc, self, caller)
		return D.directory.push_code(pname, entry, {
			origin = is_rpc and "rpc" or "plugin",
			plugin = caller_plugin,
			replace = p.replace == true,
		})
	end

	local function register_many(self, caller, entries, effect_label)
		local is_rpc = lib.rpc_caller_id(caller) ~= nil
		local caller_plugin = not is_rpc and validate.plugin_caller(caller, ctx, lib.caller_name(caller)) or nil
		local removers, errors = {}, {}
		local disposed = false
		local function dispose_all(silent)
			if disposed then
				return false
			end
			disposed = true
			local any_removed_changed = false
			for i = #removers, 1, -1 do
				local _, did_change = removers[i]()
				if did_change then
					any_removed_changed = true
				end
			end
			if any_removed_changed and silent ~= true then
				D.directory.emit_changed()
			end
			return #removers > 0
		end
		-- Attach the cleanup effect before pushing entries so a mid-batch raise cannot leak
		-- earlier registrations even if the caller fiber is disposed afterwards.
		local effect_dispose
		if #entries > 0 and caller and caller.effect then
			effect_dispose = caller:effect(function()
				return function()
					dispose_all()
				end
			end, effect_label)
		end
		local any_changed = false
		local ok_loop, loop_err = pcall(function()
			for i = 1, #entries do
				local entry = entries[i]
				local remove, changed_or_err = register_one(self, caller, is_rpc, caller_plugin, entry)
				if remove then
					removers[#removers + 1] = remove
					if changed_or_err then
						any_changed = true
					end
				else
					errors[#errors + 1] = {
						index = i,
						provider = type(entry) == "table" and entry.provider or nil,
						error = changed_or_err,
					}
				end
			end
		end)
		if not ok_loop then
			dispose_all(true)
			if type(effect_dispose) == "function" then
				pcall(effect_dispose)
			end
			error(loop_err, 0)
		end
		if #removers == 0 and type(effect_dispose) == "function" then
			pcall(effect_dispose)
		end
		if any_changed then
			D.directory.emit_changed()
		end
		return dispose_all, (#errors > 0 and errors or nil), #removers
	end

	function llm.register_provider(self, caller, p)
		local label = "llm provider " .. (type(p) == "table" and tostring(p.provider or "") or "")
		local dispose, errs, registered = register_many(self, caller, { p }, label)
		if registered == 0 then
			return nil, errs and errs[1] and errs[1].error or "invalid provider"
		end
		return dispose
	end

	function llm.register_providers(self, caller, entries)
		if type(entries) ~= "table" then
			lib.fail("invalid_arguments", "register_providers needs a list of provider entries")
		end
		if lib.rpc_caller_id(caller) ~= nil then
			lib.fail("invalid_arguments", "register_providers is only available to Lua callers")
		end
		local dispose, errs = register_many(self, caller, entries, "llm providers (" .. #entries .. ")")
		return dispose, errs
	end

	--- why provider `name` is missing when its plugin/wire failed, is disabled, or is unresolved, else nil
	local function provider_failure(name)
		local st = D.directory.status(name)
		if not st or st.state == "registered" then
			return nil
		end
		return st.reason
	end

	function llm.provider(_, _, name)
		return provider_accessor().wrap(name, providers[name])
	end
	-- the registered providers themselves (their functions included), for this plugin's own code
	local function all_providers()
		local out = {}
		for _, p in pairs(providers) do
			out[#out + 1] = p
		end
		table.sort(out, function(a, b)
			return a.name < b.name
		end)
		return out
	end
	D.all_providers = all_providers

	--- providers() -> a data projection of each provider (name, label, whether it can log in,
	--- whether it is configured), never the provider's functions: callers acting for a peer may
	--- read this, and a provider's stream/login functions must not leave the llm service
	function llm.providers(_, _)
		local out = {}
		for _, p in ipairs(all_providers()) do
			local configured = true
			if p.is_configured then
				local ok, result = pcall(p.is_configured)
				configured = ok and result == true
			end
			out[#out + 1] = {
				name = p.name,
				label = p.label,
				login = p.login ~= nil,
				login_label = p.login_label,
				default_model = p.default_model,
				no_key = p.no_key == true,
				configured = configured,
			}
		end
		return out
	end

	function llm.models(_, _)
		D.catalog.schedule_hydrate()
		local out = {}
		for _, m in pairs(models) do
			if not m.hidden then
				out[#out + 1] = m
			end
		end
		table.sort(out, function(a, b)
			return a.ref < b.ref
		end)
		return out
	end

	--- model aliases: `mare.model{ alias = "fast",... }` first, then each
	--- registered provider's declared `aliases` table (`string` target or `{ family = "..." }`)
	local function version_key(id)
		local out = {}
		for n in id:gmatch("%d+") do
			-- a date suffix (20250514) is not a version number
			if #n < 8 then
				out[#out + 1] = tonumber(n)
			end
		end
		return out
	end
	local function date_key(id)
		local d
		for n in id:gmatch("%d+") do
			if #n == 8 then
				d = tonumber(n)
			end
		end
		return d
	end
	local function newer_version(a, b)
		local x, y = version_key(a), version_key(b)
		for i = 1, math.max(#x, #y) do
			if (x[i] or -1) ~= (y[i] or -1) then
				return (x[i] or -1) > (y[i] or -1)
			end
		end
		-- Prefer undated aliases over dated snapshots of the same version; among dated snapshots,
		-- pick the newest date (pi model-resolver.ts:291-299).
		local da, db = date_key(a), date_key(b)
		if da ~= db then
			if da == nil then
				return true
			end
			if db == nil then
				return false
			end
			return da > db
		end
		return a < b
	end
	function llm.alias_target(_, _, name)
		local user = (mare.config.model_aliases or {})[name]
		if type(user) == "string" then
			return user
		end
		for _, p in ipairs(all_providers()) do
			local spec = type(p.aliases) == "table" and p.aliases[name]
			if type(spec) == "string" then
				D.catalog.schedule_hydrate(p.name)
				local prefixed = p.name .. "/" .. spec
				if models[prefixed] then
					return prefixed
				end
				if models[spec] then
					return spec
				end
			elseif type(spec) == "table" and type(spec.family) == "string" and spec.family ~= "" then
				D.catalog.schedule_hydrate(p.name)
				local fam = spec.family:gsub("[%^%$%(%)%%%.%[%]%+%-]", "%%%0")
				local best
				for _, m in pairs(models) do
					if
						m.provider == p.name
						and not m.hidden
						and m.id:find("%f[%w]" .. fam .. "%f[%W]")
						and (not best or newer_version(m.id, best.id))
					then
						best = m
					end
				end
				if best then
					return best.ref
				end
			end
		end
		return nil
	end
	--- Default field declared for `model` via its provider's `model_defaults` globs (longest
	--- pattern wins), or nil when none matches or the field is absent.
	function llm.model_default(_, _, model, field)
		if type(model) ~= "table" or type(field) ~= "string" then
			return nil
		end
		local p = model.provider and providers[model.provider]
		if not p or type(p.model_defaults) ~= "table" then
			return nil
		end
		local matches = D.matching_model_globs(p.model_defaults, model, true)
		for i = #matches, 1, -1 do
			local v = type(matches[i].value) == "table" and matches[i].value[field]
			if v ~= nil then
				return v
			end
		end
		return nil
	end
	local function provider_has_catalog(prov)
		local p = providers[prov]
		if type(p) == "table" and type(p.models) == "table" and #p.models > 0 then
			return true
		end
		for _, m in pairs(models) do
			if m.provider == prov and m.listed then
				return true
			end
		end
		return false
	end

	--- Single model-reference resolution and bare-id disambiguation shared across CLI, /model,
	--- ACP, and Lua (`pi` model-resolver.ts: collect all candidates deterministically before
	--- choosing; never depend on table iteration or registration order).
	--- Returns `model | nil, err_reason`.
	local function resolve_ref(caller, ref)
		if type(ref) ~= "string" or ref == "" then
			return nil, "unknown model: " .. tostring(ref)
		end
		local prov, id = string.match(ref, "^([^/]+)/(.+)$")
		if prov and providers[prov] then
			D.catalog.schedule_hydrate(prov)
		end
		if models[ref] then
			D.catalog.schedule_hydrate(models[ref].provider)
			return models[ref]
		end
		local target = llm.alias_target(llm, caller, ref)
		if target and target ~= ref then
			return resolve_ref(caller, target)
		end
		-- a model whose provider plugin/wire failed at startup, is disabled, or is unresolved
		-- stays selectable (config, /model, a resumed session) instead of silently falling back
		-- to another provider; requests with it are refused by the request/ready vote below —
		-- also before a bare id of another provider matches
		local why = prov and provider_failure(prov)
		if why then
			return apply_overrides({
				id = id,
				name = id,
				provider = prov,
				ref = ref,
				context_window = 128000,
				max_output = 16384,
				unavailable = why,
			})
		end
		if prov and providers[prov] then
			if provider_has_catalog(prov) then
				return nil, ("unknown model: %s (not in %s catalog)"):format(ref, prov)
			end
			local p = providers[prov]
			local m = {
				id = id,
				name = id,
				provider = prov,
				ref = ref,
				context_window = p.default_context or 128000,
				max_output = p.default_max_output or 16384,
				max_output_default = true,
				reasoning = p.default_reasoning,
			}
			return apply_overrides(m)
		end
		-- bare id (or gateway id containing `/`): collect all matches sorted by ref so
		-- registration/hash order never picks the provider
		D.catalog.schedule_hydrate()
		local matches = {}
		for _, m in pairs(models) do
			if m.id == ref and not m.hidden then
				matches[#matches + 1] = m
			end
		end
		if #matches == 1 then
			return matches[1]
		elseif #matches > 1 then
			table.sort(matches, function(a, b)
				return a.ref < b.ref
			end)
			local refs = {}
			for i, m in ipairs(matches) do
				refs[i] = m.ref
			end
			return nil,
				("ambiguous model %q: matches %s — specify provider/model"):format(ref, table.concat(refs, ", "))
		end
		if prov then
			return nil, ("unknown model: %s (unknown provider %q)"):format(ref, prov)
		end
		return nil, "unknown model: " .. ref
	end

	local function resolve_ref_hydrated(caller, ref)
		if type(ref) == "string" and ref ~= "" and not models[ref] then
			local prov = string.match(ref, "^([^/]+)/")
			D.catalog.ensure_loaded(prov and providers[prov] and prov or nil)
		end
		return resolve_ref(caller, ref)
	end

	function llm.resolve(_, caller, ref)
		if not ref or ref == "" then
			return nil
		end
		return (resolve_ref_hydrated(caller, ref))
	end

	function llm.current(self, caller)
		-- Only auto-select a provider default when no explicit model was specified.
		if type(state.model) == "string" and state.model ~= "" then
			return (resolve_ref(caller, state.model))
		end
		if not D.startup_done then
			return nil
		end
		-- fall back to the first provider that reports being configured
		for _, p in ipairs(all_providers()) do
			if p.default_model and (not p.is_configured or p.is_configured()) then
				local m = self:resolve(caller, p.name .. "/" .. p.default_model)
				if m and not m.unavailable and (not p.unavailable_for or not p.unavailable_for(m)) then
					return m
				end
			end
		end
		return nil
	end
	local function clone_state(value)
		if type(value) ~= "table" then
			return value
		end
		local out = {}
		for k, v in pairs(value) do
			out[clone_state(k)] = clone_state(v)
		end
		return out
	end
	-- Session restoration compares this exact mutable projection before and after a setter. If a
	-- setter emits a failing listener after changing state, the caller must poison the committed
	-- session; a pre-mutation error can safely fall back to the default.
	function llm.get_selection()
		return {
			model = state.model,
			default_model = state.default_model,
			recent = clone_state(state.recent),
			params = clone_state(state.params),
		}
	end
	local function apply_pinned(ref, model)
		local pinned = (mare.config.model_alias_params or {})[ref]
		if type(pinned) == "table" then
			for k, v in pairs(pinned) do
				state.params[model.ref] = state.params[model.ref] or {}
				state.params[model.ref][k] = v
			end
		end
	end

	local function resolve_target_model(self, caller, model)
		if model == nil then
			return self:current(caller)
		end
		if type(model) == "string" then
			return self:resolve(caller, model)
		end
		if type(model) == "table" and type(model.ref) == "string" then
			return model
		end
		return nil
	end

	--- params_only: the model stays and only its effort / effort scope changed — listeners
	--- record it, but it is not a model switch (no pruning, re-measuring or switch notes)
	local function emit_model_changed(self, caller, m, opts, scoped_only, params_only)
		local emit = opts and opts.checked and ctx.bus.emit_checked or ctx.bus.emit
		emit(ctx.bus, "llm/model-changed", {
			model = m,
			effort = self:effort(caller),
			global_effort = self:global_effort(caller, m),
			scope = state.active_scope,
			scoped_only = scoped_only or false,
			params_only = params_only or nil,
		})
	end

	local function resolve_save(opts, field, label)
		if opts and opts[field] ~= nil then
			local v = opts[field]
			if v ~= "session" and v ~= "global" then
				lib.fail("invalid_arguments", label .. " must be 'session' or 'global'")
			end
			local save = v == "global"
			if opts.save ~= nil and opts.save ~= save then
				lib.fail("invalid_arguments", "conflicting " .. label .. " and save options")
			end
			return save
		end
		return not (opts and opts.save == false)
	end

	--- select_model(ref, opts?) -> model: internal helper behind set_model and restore_selection.
	--- opts.save = false (or target = "session"): this session only (restored on resume),
	--- the default in state.json stays as it is.
	local function select_model(self, caller, ref, opts)
		local save = resolve_save(opts, "target", "model target")
		local failure = mare.task.session_error()
		if failure then
			error(failure, 0)
		end
		local m, why = resolve_ref_hydrated(caller, ref)
		if not m then
			lib.fail("llm.unknown_model", why or ("unknown model: " .. tostring(ref)))
		end
		local commit
		local pinned = (mare.config.model_alias_params or {})[ref]
		-- a listener refuses the switch with ev.refuse(message) (compaction's budget check): the
		-- caller gets llm.model_unavailable with that message. Any other listener error is
		-- unexpected and reaches outside callers as `internal`.
		local refusal
		local admitted, failure = pcall(ctx.bus.emit_checked, ctx.bus, "llm/model-changing", {
			previous = self:current(caller),
			model = m,
			effort = self:clamp_effort(caller, m, pinned and pinned.effort),
			restoring = opts and opts.restoring,
			source_unknown = opts and opts.source_unknown and true or nil,
			-- One owner may prepare a context replacement. All listeners must admit
			-- the switch before its single durable commit can publish the model.
			stage = function(fn)
				assert(type(fn) == "function", "staged commit must be a function")
				assert(not commit, "a model change already has a staged commit")
				commit = fn
			end,
			refuse = function(message)
				refusal = tostring(message)
				error(refusal, 0)
			end,
		})
		if not admitted then
			if refusal then
				lib.fail("llm.model_unavailable", refusal)
			end
			error(failure, 0)
		end
		if commit then
			commit(function()
				state.model = m.ref
			end)
		else
			state.model = m.ref
		end
		-- an alias with pinned parameters (`mare.model{ alias =, params = { effort = "deep" } }`)
		-- sets them for this session
		apply_pinned(ref, m)
		if save then
			state.default_model = m.ref
			lib.save_state(ctx, "model", m.ref, true)
			-- most recent first, at most 8 (the model-cycle key's list without favorites)
			local recent = { m.ref }
			for _, r in ipairs(state.recent) do
				if r ~= m.ref and #recent < 8 then
					recent[#recent + 1] = r
				end
			end
			state.recent = recent
			lib.save_state(ctx, "recent_models", recent, true)
		end
		local checked_restore = opts and opts.restoring and opts.checked
		emit_model_changed(self, caller, m, { checked = checked_restore }, false)
		-- Public consumers get only the JSON model identity. Local listeners needing the executable
		-- model data use the process-only `llm/model-changed` event above. A checked emission goes
		-- through the host's one entry (ctx.bus).
		local identity = { ref = m.ref, provider_id = m.provider, model_id = m.id }
		if checked_restore then
			ctx.bus:emit_checked("model/changed", identity)
		else
			ctx.bus:emit("model/changed", identity)
		end
		return m
	end

	--- set_model(ref, opts?) -> the detached descriptor of the selected model. The descriptor is
	--- built before anything changes, so a failure leaves the selection as it was.
	function llm.set_model(self, caller, ref, opts)
		local m, why = resolve_ref_hydrated(caller, ref)
		if not m then
			lib.fail("llm.unknown_model", why or ("unknown model: " .. tostring(ref)))
		end
		local descriptor, err = self:capabilities(caller, m)
		if not descriptor then
			lib.fail("llm.model_unavailable", tostring(err))
		end
		-- the result is encoded and checked against the contract before anything changes
		mare.contracts.validate("llm", "set_model", descriptor)
		select_model(
			self,
			caller,
			ref,
			opts and { target = opts.target, save = opts.save, source_unknown = opts.source_unknown }
		)
		return descriptor
	end

	--- recent() -> refs of recently chosen models, most recent first
	function llm.recent(_, _)
		return { table.unpack(state.recent) }
	end

	--- clamp_effort(model, effort?) -> the effort `model` runs at (nil: it has no effort
	--- parameter). The model-parameters engine (llm/params): a model's explicit mapping
	--- (effort_map: DeepSeek V4 reads xhigh as max) wins over the nearest-level rule, lower on a
	--- tie; without `effort`, the value chosen by layer (session, config, the model's default)
	function llm.clamp_effort(_, _, model, effort)
		if not model then
			return nil
		end
		local d = params.defs(model).effort
		if not d then
			return nil
		end
		if effort == nil then
			return params.select(model, { only = { effort = true } }).params.effort
		end
		return params.clamp(d, effort)
	end

	--- effort_options(model?) -> the effort values offered for `model` on its active route
	--- (same source as `param_list`), lowest first; nil when it has no effort parameter
	function llm.effort_options(self, caller, model)
		model = model or self:current(caller)
		local d = model and params.defs(model).effort
		if not d or not d.list then
			return nil
		end
		local out = {}
		for i, v in ipairs(d.list) do
			out[i] = v
		end
		return out
	end

	function llm.effort(self, caller)
		return self:clamp_effort(caller, self:current(caller))
	end
	--- The unscoped/global effort for `model` (default: current), ignoring `active_scope`.
	function llm.global_effort(self, caller, model)
		local m = resolve_target_model(self, caller, model)
		if not m then
			return nil
		end
		return params.select(m, { only = { effort = true }, ignore_scope = true }).params.effort
	end
	--- The selected model's effort before clamping; nil when it has no effort parameter.
	function llm.raw_effort(self, caller)
		local m = self:current(caller)
		return m and params.select(m, { only = { effort = true } }).wanted.effort or nil
	end
	-- Drop only the transient choice; remembered/configured defaults stay intact.
	function llm.reset_session_effort(self, caller)
		local m = self:current(caller)
		if m and state.params[m.ref] then
			state.params[m.ref].effort = nil
		end
		state.scoped_params = {}
		state.scoped_cleared = {}
	end
	--- the provider can't serve without credentials it lacks — the readiness rule
	--- (llm/readiness.lua missing_credentials): a provider that needs none (faux without a
	--- script, a local server) counts as configured for the model the user configured
	local function lacks_credentials(p, name)
		local pcfg = D.api_keys.provider_cfg(name)
		return require("llm.readiness").missing_credentials(p, pcfg, function()
			return ""
		end) ~= nil
	end

	function llm.reset_session_model(self, caller)
		local previous = self:current(caller)
		if previous and state.params[previous.ref] then
			state.params[previous.ref].effort = nil
		end
		state.scoped_params = {}
		state.scoped_cleared = {}
		state.model = state.default_model
		local has_default = type(state.default_model) == "string" and state.default_model ~= ""
		local m = self:current(caller)
		local p = m and providers[m.provider]
		if
			not m
			or m.unavailable
			or not p
			or lacks_credentials(p, m.provider)
			or (p.unavailable_for and p.unavailable_for(m))
		then
			if has_default then
				-- Explicit default_model is unresolvable, unavailable, or lacks credentials:
				-- keep state.model = state.default_model so session lifecycle warns and readiness
				-- reports the exact reason instead of silently falling back to another provider.
				return nil
			end
			state.model = nil
			m = self:current(caller)
			p = m and providers[m.provider]
		end
		if
			not m
			or m.unavailable
			or not p
			or lacks_credentials(p, m.provider)
			or (p.unavailable_for and p.unavailable_for(m))
		then
			return nil
		end
		local ref = state.default_model and self:resolve(caller, state.default_model) == m and state.default_model
			or m.ref
		state.model = m.ref
		if state.params[m.ref] then
			state.params[m.ref].effort = nil
		end
		apply_pinned(ref, m)
		emit_model_changed(self, caller, m, { checked = true }, false)
		-- Session restoration uses checked emission because this follows mutation in a committed
		-- switch. A failing listener keeps the host's poison barrier intact.
		ctx.bus:emit_checked("model/changed", { ref = m.ref, provider_id = m.provider, model_id = m.id })
		return m
	end
	--- select_effort(effort, opts?): internal helper behind set_effort and restore_selection.
	local function select_effort(self, caller, e, opts)
		local save = resolve_save(opts, "target", "effort target")
		local m = self:current(caller)
		if not m then
			lib.fail("llm.no_effort", "no model selected")
		end
		local d = params.defs(m).effort
		if not d then
			lib.fail("llm.no_effort", m.ref .. " has no effort parameter")
		end
		local offered = d.list or {}
		local valid = table.find(offered, e) ~= nil
			or (
				opts
				and opts.restoring == true
				and (EFFORT_INDEX[e] or params.known_option("effort", e) or d.index[e] ~= nil)
			)
		if not valid then
			lib.fail(
				"llm.unknown_effort",
				("unknown effort: %s (supported for %s: %s)"):format(tostring(e), m.ref, table.concat(offered, ", "))
			)
		end
		local scope = opts and opts.scope_name
		if scope ~= nil then
			if type(scope) ~= "string" or scope == "" then
				lib.fail("invalid_arguments", "effort scope must be a non-empty string")
			end
			state.scoped_params[scope] = state.scoped_params[scope] or {}
			state.scoped_params[scope][m.ref] = state.scoped_params[scope][m.ref] or {}
			state.scoped_params[scope][m.ref].effort = e
			if state.scoped_cleared and state.scoped_cleared[scope] then
				state.scoped_cleared[scope][m.ref] = nil
			end
			if save then
				D.remember_scoped_param(scope, m.ref, "effort", e)
			end
		end
		local updates_global = scope == nil or (opts and opts.also_global)
		if updates_global then
			state.params[m.ref] = state.params[m.ref] or {}
			state.params[m.ref].effort = e
			if save then
				self:remember_param(caller, m.ref, "effort", e)
			end
		end
		emit_model_changed(self, caller, m, opts, not updates_global, true)
	end

	--- set_effort(effort, opts?): the public setter (RPC `llm.set_effort`)
	function llm.set_effort(self, caller, e, opts)
		select_effort(self, caller, e, opts and {
			target = opts.target,
			scope_name = opts.scope_name,
			also_global = opts.also_global,
			save = opts.save,
		})
	end

	--- restore_selection(selection): restore session model and/or effort from history without
	--- running budget checks or persisting defaults; emits checked change events to preserve
	--- the poison barrier.
	function llm.restore_selection(self, caller, selection)
		if type(selection) ~= "table" then
			lib.fail("invalid_arguments", "restore_selection requires a selection table")
		end
		if selection.model ~= nil then
			select_model(self, caller, selection.model, { save = false, restoring = true, checked = true })
		end
		if selection.effort ~= nil then
			select_effort(self, caller, selection.effort, { save = false, restoring = true, checked = true })
		end
		return {}
	end

	function llm.scoped_effort(self, caller, scope, model)
		if type(scope) ~= "string" or scope == "" then
			return nil
		end
		local m = resolve_target_model(self, caller, model)
		if not m then
			return nil
		end
		if state.scoped_cleared and state.scoped_cleared[scope] and state.scoped_cleared[scope][m.ref] then
			return nil
		end
		local sess = state.scoped_params[scope] and state.scoped_params[scope][m.ref]
		local mem = state.scoped_remembered and state.scoped_remembered[scope] and state.scoped_remembered[scope][m.ref]
		local raw = (sess and sess.effort) or (mem and mem.effort)
		if raw == nil then
			return nil
		end
		return self:clamp_effort(caller, m, raw)
	end

	function llm.clear_scope(self, caller, scope, model, opts)
		if type(scope) ~= "string" or scope == "" then
			return
		end
		local m = resolve_target_model(self, caller, model)
		if m then
			if state.scoped_params[scope] and state.scoped_params[scope][m.ref] then
				state.scoped_params[scope][m.ref].effort = nil
			end
			if opts and opts.save == false then
				state.scoped_cleared = state.scoped_cleared or {}
				state.scoped_cleared[scope] = state.scoped_cleared[scope] or {}
				state.scoped_cleared[scope][m.ref] = true
			else
				if state.scoped_cleared and state.scoped_cleared[scope] then
					state.scoped_cleared[scope][m.ref] = nil
				end
				D.remember_scoped_param(scope, m.ref, "effort", nil)
			end
		end
		if state.active_scope == scope then
			state.active_scope = nil
			if m then
				emit_model_changed(self, caller, m, opts, true, true)
			end
		end
	end

	function llm.set_active_scope(self, caller, scope, opts)
		if scope ~= nil and (type(scope) ~= "string" or scope == "") then
			error("active scope must be nil or a non-empty string")
		end
		state.active_scope = scope
		local m = self:current(caller)
		if m then
			emit_model_changed(self, caller, m, opts, true, true)
		end
	end

	function llm.active_scope(_, _)
		return state.active_scope
	end

	local function copy_cost(cost)
		if type(cost) ~= "table" then
			return nil
		end
		local out = {}
		for _, k in ipairs({ "input", "output", "cache_read", "cache_write" }) do
			-- finite numbers only: a JSON result cannot carry inf or NaN
			local v = cost[k]
			if type(v) == "number" and v == v and v ~= math.huge and v ~= -math.huge then
				out[k] = v
			end
		end
		return next(out) and out or nil
	end

	local function detach_param_options(options)
		local out = {}
		for _, o in ipairs(options or {}) do
			local item = { value = o.value }
			if type(o.name) == "string" then
				item.name = o.name
			end
			if type(o.tooltip) == "string" then
				item.tooltip = o.tooltip
			end
			if type(o.cost) == "number" then
				item.cost = o.cost
			elseif type(o.cost) == "table" then
				item.cost = copy_cost(o.cost)
			end
			out[#out + 1] = item
		end
		return mare.json.array(out)
	end

	local function text(v)
		return type(v) == "string" and v or nil
	end
	local function flag(v)
		if type(v) == "boolean" then
			return v -- false is a value too (cycleable = false), not "unset"
		end
		return nil
	end
	local function whole(v)
		return math.type(v) == "integer" and v or nil
	end
	local function strings(list)
		local out = {}
		for _, v in ipairs(list) do
			if type(v) == "string" then
				out[#out + 1] = v
			end
		end
		return out
	end

	--- capabilities(ref?) -> detached JSON-safe model and parameter capability descriptor
	function llm.capabilities(self, caller, ref)
		local m
		if ref == nil then
			m = self:current(caller)
			if not m then
				return nil, "no model selected"
			end
		elseif type(ref) == "string" then
			local why
			m, why = resolve_ref_hydrated(caller, ref)
			if not m then
				return nil, why or ("unknown model: " .. ref)
			end
		elseif type(ref) == "table" and type(ref.ref) == "string" then
			m = self:resolve(caller, ref.ref) or ref
		else
			return nil, "model reference must be a string"
		end

		local route, _, wire_route, route_err = self:route_of(caller, m)
		local p = providers[m.provider]
		local unavailable_reason
		if type(m.unavailable) == "string" then
			unavailable_reason = m.unavailable
		elseif m.unavailable then
			unavailable_reason = "model is unavailable"
		elseif not p then
			unavailable_reason = provider_failure(m.provider)
				or ("provider " .. tostring(m.provider) .. " is not available")
		elseif p.unavailable_for then
			unavailable_reason = p.unavailable_for(m)
		end
		if not unavailable_reason and route_err then
			unavailable_reason = route_err
		end

		local ready, ready_reason = self:ready(caller, m)
		local effort_def = params.defs(m).effort
		local efforts = (effort_def and effort_def.list) or self:effort_options(caller, m) or {}
		local default_effort = effort_def and self:clamp_effort(caller, m, effort_def.default) or nil
		local parameters = {}
		for _, d in ipairs(self:param_list(caller, m)) do
			local values = {}
			for _, v in ipairs(d.list or {}) do
				values[#values + 1] = v
			end
			parameters[#parameters + 1] = {
				parameter_id = tostring(d.id),
				name = text(d.name),
				type = text(d.type),
				default = d.default,
				tooltip = text(d.tooltip),
				cycleable = flag(d.cycleable),
				statusline = d.statusline,
				visible = d.visible,
				values = mare.json.array(values),
				options = detach_param_options(d.options),
			}
		end

		-- The public descriptor (contract `llm` model): provider data enters only with the
		-- declared type, so a result built here always matches the contract.
		return {
			ref = tostring(m.ref),
			model_id = tostring(m.id),
			provider = tostring(m.provider),
			name = text(m.name),
			context_window = whole(m.context_window),
			max_output = whole(m.max_output),
			reasoning = m.reasoning == true,
			tool_update = m.tool_update == "addition_only" and "addition_only" or nil,
			images = lib.model_images(m) ~= false,
			edit_tool = (m.edit_tool == "apply_patch" or m.edit_tool == "replace") and m.edit_tool or nil,
			cost = copy_cost(m.cost),
			route = text(route),
			wire_route = text(wire_route),
			route_error = text(route_err),
			available = unavailable_reason == nil,
			unavailable_reason = text(unavailable_reason),
			ready = ready == true,
			ready_reason = text(ready_reason),
			efforts = mare.json.array(strings(efforts)),
			default_effort = text(default_effort),
			parameters = mare.json.array(parameters),
		}
	end

	--- list_models() -> {data = descriptors}: every visible model, sorted by ref
	function llm.list_models(self, caller)
		local data = {}
		for _, m in ipairs(self:models(caller)) do
			data[#data + 1] = self:capabilities(caller, m)
		end
		return { data = mare.json.array(data) }
	end

	--- get_model(ref?) -> the descriptor of the session's current model, or of the model `ref`
	--- resolves to (hidden and unlisted ones included); the selection never changes
	function llm.get_model(self, caller, ref)
		local descriptor, err = self:capabilities(caller, ref)
		if not descriptor then
			if type(err) == "string" and (err:match("^unknown model") or err:match("^ambiguous model")) then
				lib.fail("llm.unknown_model", err)
			end
			error(err, 0)
		end
		return descriptor
	end

	return {
		provider_failure = provider_failure,
		install_provider = install_provider,
		uninstall_provider = uninstall_provider,
		normalize_entry = normalize_entry,
		unresolved_model = function()
			if type(state.model) ~= "string" or state.model == "" then
				return nil
			end
			local m, why = resolve_ref(ctx, state.model)
			return not m and why or nil
		end,
	}
end
