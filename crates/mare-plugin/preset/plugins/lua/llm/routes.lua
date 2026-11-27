-- llm.routes: per-model routes and the wire registry (a route is a property of the model, a
-- provider only supplies the gateway, auth and session header). Part of the `llm` plugin
-- (plugins/llm.lua), loaded at the start of its apply.
--
-- A provider with several routes declares `routes = { <route> = true, … }` (the configured ones),
-- `route` (its default) and optionally `model_routes = { ["<glob>"] = route }` and
-- `models_dev_routes = true`. A model's route, first that applies:
--   1. `mare.model{ route = }` (the user, per model);
--   2. the provider's `model_routes` glob, the longest pattern winning;
--   3. the built-in catalogue entry's `route` (measured values only);
--   4. models.dev's protocol for the model (responses / messages → anthropic /
--      chat-completions → chat), only when the provider turns it on and only onto a configured
--      route — the chat route is never picked here unless configured;
--   5. the provider's default route.
-- A provider without `routes` always uses its `route`.
return function(D)
	local providers, llm = D.providers, D.llm

	local R = {}

	-- models.dev protocol keys → route names
	local PROTOCOL = { responses = "responses", messages = "anthropic", ["chat-completions"] = "chat" }
	R.PROTOCOL = PROTOCOL

	local wires = {}

	--- Select the gateway route and identify the layer that chose it.
	local function select_route(model)
		local p = model and providers[model.provider]
		if not p then
			return nil
		end
		if type(p.routes) ~= "table" then
			return p.route, "provider"
		end
		local user = (mare.config.models or {})[model.ref]
		if type(user) == "table" and type(user.route) == "string" then
			return user.route, "mare.model"
		end
		local matches = D.matching_model_globs(p.model_routes, model)
		for i = #matches, 1, -1 do
			local m = matches[i]
			if type(m.value) == "string" then
				return m.value, ('model_routes "%s"'):format(m.pat)
			end
		end
		if type(model.route) == "string" then
			return model.route, "built-in catalogue"
		end
		local via = p.models_dev_routes and PROTOCOL[model.protocol or ""]
		if via and p.routes[via] then
			return via, ("models.dev (%s)"):format(model.protocol)
		end
		return p.route, "provider default"
	end

	-- A request retains the selected route data and wire even if a plugin changes the registry
	-- while the request is in flight. Configuration may contain nested tables.
	local function snapshot(value, seen)
		if type(value) ~= "table" then
			return value
		end
		seen = seen or {}
		if seen[value] then
			return seen[value]
		end
		local out = {}
		seen[value] = out
		for k, v in pairs(value) do
			out[snapshot(k, seen)] = snapshot(v, seen)
		end
		return out
	end

	--- Resolve the gateway route and the wire's parameter route together. The latter comes from
	--- the registered wire, so a gateway may give its route any name.
	function R.route_of(model)
		local route, source = select_route(model)
		local p = model and providers[model.provider]
		if not p or type(p.routes) ~= "table" then
			return route, source, route
		end
		local rc = route and p.routes[route]
		if type(rc) ~= "table" or type(rc.wire) ~= "string" then
			return route,
				source,
				nil,
				("provider %s has no configured wire for route %s"):format(p.name, tostring(route))
		end
		local wire = wires[rc.wire]
		if not wire or type(wire.describe.route) ~= "string" then
			return route,
				source,
				nil,
				("provider %s route %s has no available wire route for %s"):format(p.name, route, rc.wire)
		end
		local styles = wire.describe.auth
		local auth = rc.auth or (type(styles) == "table" and styles[1]) or "bearer"
		if type(styles) == "table" and #styles > 0 then
			local supported = false
			for _, style in ipairs(styles) do
				if style == auth then
					supported = true
					break
				end
			end
			if not supported then
				return route,
					source,
					nil,
					("provider %s route %s auth %s is unsupported by wire %s"):format(
						p.name,
						route,
						tostring(auth),
						rc.wire
					)
			end
		end
		return route,
			source,
			wire.describe.route,
			nil,
			auth,
			{ config = snapshot(rc), describe = snapshot(wire.describe), stream = wire.stream }
	end

	--- route_of(model?) -> gateway route, source, wire parameter route, error, auth, snapshot
	function llm.route_of(self, caller, model)
		model = model or self:current(caller)
		return R.route_of(model)
	end

	-- wires: name → { spec, stream = fn(endpoint, req, sink), plugin, describe }. A provider plugin that
	-- speaks a wire format registers it; a provider routing a model over that format looks it up
	-- per request, so a disabled or failed wire plugin only affects the models on its route.
	--- register_wire(spec) or register_wire(name, spec). The spec's route, path and auth
	--- describe gateway routing; encode and decoder are required.
	function llm.register_wire(_, caller, name, spec)
		if type(name) == "table" then
			spec, name = name, name.name
		elseif type(spec) ~= "table" then
			spec = nil
		end
		local validate = require("llm.validate")
		local caller_plugin = validate.plugin_caller(caller, D.ctx, require("mare_lib").caller_name(caller))
		local ok, key, reason = validate.wire(name, spec)
		if not ok then
			if caller_plugin then
				validate.report_problem(caller_plugin, key, reason)
				mare.log("warn", reason)
				return nil, reason
			end
			error(reason, 2)
		end
		validate.clear_problem(caller_plugin, key)
		local wire_stream = function(endpoint, req, sink)
			return D.send(spec, endpoint, req, sink)
		end
		local description = {
			route = spec.route,
			path = spec.path,
			auth = spec.auth,
			hosted_tools = spec.hosted_tools and snapshot(spec.hosted_tools) or nil,
			provider_fields = spec.provider_fields and snapshot(spec.provider_fields) or nil,
			model_fields = spec.model_fields and snapshot(spec.model_fields) or nil,
		}
		local release_hint
		if type(spec.estimate_hints) == "table" and mare.text and mare.text.register_estimate_hint then
			local hint_ns = caller_plugin or "host"
			release_hint = mare.text.register_estimate_hint("wire:" .. hint_ns .. ":" .. name, spec.estimate_hints)
		end
		local w = {
			name = name,
			spec = spec,
			stream = wire_stream,
			plugin = require("mare_lib").caller_name(caller),
			describe = description,
		}
		wires[name] = w
		if D.directory and D.directory.on_wire_added then
			D.directory.on_wire_added(name, w)
		end
		-- the wire goes with whoever registered it (the caller's fiber)
		if caller and caller.effect then
			caller:effect(function()
				return function()
					if release_hint then
						release_hint()
					end
					if wires[name] == w then
						wires[name] = nil
						if D.directory and D.directory.on_wire_removed then
							D.directory.on_wire_removed(name, w)
						end
					end
				end
			end, "llm wire " .. name)
		end
	end

	function R.get_wire(name)
		return wires[name]
	end

	--- Collect all registered wire specs referenced by a provider declaration (`wire`, `routes`, or `route`).
	function R.wire_specs_for_provider(p)
		if type(p) ~= "table" then
			return nil
		end
		local seen, specs = {}, {}
		local function add_wire(name)
			if type(name) == "string" and not seen[name] then
				local w = wires[name]
				if w and w.spec then
					seen[name] = true
					specs[#specs + 1] = w.spec
				end
			end
		end
		add_wire(p.wire)
		if type(p.routes) == "table" then
			for _, rc in pairs(p.routes) do
				if type(rc) == "table" then
					add_wire(rc.wire)
				end
			end
		end
		if #specs == 0 and type(p.route) == "string" then
			local validate = require("llm.validate")
			local info = validate.WIRE_APIS[p.route]
			if info then
				add_wire(info.wire)
			end
			if #specs == 0 then
				for _, w in pairs(wires) do
					if w.describe and w.describe.route == p.route then
						add_wire(w.name)
					end
				end
			end
		end
		return #specs > 0 and specs or nil
	end

	--- wire(name) -> stream, plugin | nil
	function llm.wire(_, _, name)
		local w = wires[name]
		return w and w.stream, w and w.plugin
	end

	--- describe_wire(name) -> the registered wire's describe table (a copy) | nil
	function llm.describe_wire(_, _, name)
		local w = wires[name]
		if not w then
			return nil
		end
		return snapshot(w.describe)
	end

	--- wire_owner(name) -> the plugin that registered wire `name`, else the first plugin whose
	--- definition declares it (`wires = { name }`, also while that plugin is off or failed) | nil
	function llm.wire_owner(_, _, name)
		local w = wires[name]
		if w then
			return w.plugin
		end
		return (mare.plugins and mare.plugins.declaring("wires", name) or {})[1]
	end

	--- known_routes() -> { route = true }: the routes something provides — a registered wire's
	--- `describe.route` and a single-route provider's `route` (llm/params checks init.lua
	--- rules against these)
	function R.known_routes()
		local out = {}
		for _, w in pairs(wires) do
			if type(w.describe.route) == "string" then
				out[w.describe.route] = true
			end
		end
		for _, p in pairs(providers) do
			if type(p.routes) ~= "table" and type(p.route) == "string" then
				out[p.route] = true
			end
		end
		return out
	end

	return R
end
