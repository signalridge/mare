-- Structured increments share the ordinary frozen tool/policy/approval pipeline.
return function(D)
	local E, P = D.escalation, D.P
	local lib = require("mare_lib")
	local function clean(inc)
		for _, kind in ipairs({ "read", "write", "network" }) do
			if type(inc[kind]) == "table" then
				local seen, values = {}, mare.json.array()
				for _, value in ipairs(inc[kind]) do
					if not seen[value] then
						values[#values + 1], seen[value] = value, true
					end
				end
				inc[kind] = values
			end
		end
		return inc
	end
	local function normalize(value, cwd)
		assert(type(value) == "table", "additional_permissions must be an object")
		local out = {}
		for kind, paths in pairs(value) do
			assert(kind == "read" or kind == "write" or kind == "network", "unknown permission dimension")
			if kind == "network" and type(paths) == "boolean" then
				out.network = paths or nil
			else
				assert(lib.is_dense_list(paths), "permission lists must be dense")
				out[kind] = mare.json.array()
				for _, path in ipairs(paths) do
					assert(type(path) == "string" and path:match("%S"), "permission entries must be nonempty strings")
					local resolved = path
					if kind ~= "network" then
						if cwd and lib.is_project_relative(path) then
							path = cwd .. "/" .. path
						end
						resolved = mare.fs.resolve(path)
						-- Unlike legacy write:<file>, a structured grant never grows to its parent.
						resolved = mare.fs.realpath(resolved) or resolved
						if kind == "read" then
							resolved =
								mare.proc.sandbox.policy({ level = "read-only", readable = { resolved } }).readable[1]
						end
						if kind == "write" then
							local stat = mare.fs.stat(resolved)
							assert(
								stat and stat.kind == "dir",
								"write permissions require an existing physical directory"
							)
						end
					else
						assert(not path:find("[/\\%s]"), "network entries must be host patterns")
					end
					out[kind][#out[kind] + 1] = resolved
				end
			end
		end
		return clean(out)
	end
	local function valid_store(store)
		local out = D.copy(store)
		out.read, out.write = {}, {}
		for _, path in ipairs(store.read or {}) do
			if mare.proc.sandbox.within(path, { path }, { stable_roots = true }) then
				out.read[#out.read + 1] = path
			end
		end
		for _, path in ipairs(store.write or {}) do
			if
				pcall(mare.proc.sandbox.policy, {
					level = "workspace-write",
					write = { path },
					exclude_tmpdir_env_var = true,
					exclude_slash_tmp = true,
				})
			then
				out.write[#out.write + 1] = path
			end
		end
		return clean(out)
	end
	local function stores()
		return clean(E.inc_union(valid_store(P.approved.session), valid_store(P.approved.turn)))
	end
	-- Only rights outside the current host baseline need a new grant. Do not turn
	-- already available baseline access into a capability that can outlive that baseline.
	local function additional(base, inc)
		local out = {}
		if base == false then
			return out
		end
		for _, kind in ipairs({ "read", "write" }) do
			for _, path in ipairs(inc[kind] or {}) do
				local target = kind == "write" and (path .. "/.mare-probe") or path
				if mare.proc.sandbox.check(kind, target, base) ~= true then
					out[kind] = out[kind] or {}
					table.insert(out[kind], path)
				end
			end
		end
		if inc.network == true and base.network ~= true then
			out.network = true
		elseif type(inc.network) == "table" and base.network ~= true then
			local net = {}
			for _, host in ipairs(inc.network) do
				if mare.proc.sandbox.check("network", host, base) ~= true then
					table.insert(net, host)
				end
			end
			if #net > 0 then
				out.network = net
			end
		end
		return out
	end
	local function vote(call, info)
		local a = call.arguments or {}
		if a.additional_permissions == nil and not info.permission_request then
			return nil
		end
		local binding = D.lib.policy_binding(call, info)
		local function refuse(reason)
			D.record("result", {
				tool = call.name,
				decision = "deny",
				source = "policy",
				reason = reason,
				requested_permissions = a.additional_permissions,
			})
			return { decision = "deny", reason = reason }
		end
		local ok, inc = pcall(normalize, a.additional_permissions, call.cwd or mare.cwd)
		if not ok then
			return refuse(tostring(inc))
		end
		if E.inc_empty(inc) then
			return refuse("at least one permission is required")
		end
		if type(a.justification) ~= "string" or not a.justification:match("%S") then
			return refuse("additional_permissions requires a justification")
		end
		if a.scope ~= nil and a.scope ~= "turn" and a.scope ~= "session" then
			return refuse("permission scope must be turn or session")
		end
		local base = D.presets.base_spec()
		if base ~= false then
			for _, path in ipairs(inc.read or {}) do
				if mare.proc.sandbox.within(path, base.deny_read or {}) then
					return refuse("requested read is forbidden by the baseline deny_read policy")
				end
			end
		end
		local increment = additional(base, inc)
		local missing = E.inc_remainder(increment, a.scope == "session" and valid_store(P.approved.session) or stores())
		local reason = missing and ("requests " .. E.inc_text(missing) .. " · because: " .. a.justification)
			or info.unconfined and "not confined by the local sandbox"
			or nil
		if info.permission_request or a.scope then
			reason = (reason and (reason .. " · ") or "") .. "maximum grant scope: " .. (a.scope or "turn")
		end
		D.record(missing and "request" or "auto", {
			tool = call.name,
			permissions = clean(D.copy(missing or increment)),
			requested_permissions = inc,
			scope = a.scope or (info.permission_request and "turn" or nil),
			justification = a.justification,
			basis = not missing and "additional rights already allowed" or nil,
		})
		return {
			decision = (missing or info.unconfined) and "ask" or "allow",
			category = info.permission_request and "request_permissions" or "sandbox",
			reason = reason,
			sandbox = E.apply_inc(base, E.inc_union(stores(), increment)),
			request = missing,
			permissions = inc,
			binding = binding,
			increment = increment,
			permission_request = info.permission_request,
			scope = a.scope or (info.permission_request and "turn" or nil),
			key = call.name .. "@" .. tostring(info and info.tool_version or "") .. "|" .. tostring(
				call.cwd or mare.cwd
			) .. "|" .. mare.json.encode(a),
		}
	end
	-- A callback supplies constraints, not extra authority. Intersect every approval vote.
	local function resolve(v, constraints, scope)
		assert(scope == nil or scope == "turn" or scope == "session", "permission scope must be turn or session")
		if constraints ~= nil then
			assert(type(constraints) == "table", "permission constraints must be a list")
			assert(lib.is_dense_list(constraints), "permission constraints must be dense")
		end
		local granted = D.copy(v.request or v.increment or v.permissions or {})
		for _, constraint in ipairs(constraints or {}) do
			granted = E.inc_narrow(granted, normalize(constraint))
		end
		if v.permissions and v.request then
			granted = E.inc_union(
				granted,
				E.inc_narrow(
					v.increment or v.permissions,
					v.scope == "session" and valid_store(P.approved.session) or stores()
				)
			)
		end
		local effective_scope = scope
		if v.permissions and v.scope then
			effective_scope = (v.scope == "session" and (scope == "session" or not v.request)) and "session" or "turn"
		end
		granted = clean(granted)
		return granted, E.apply_inc(D.presets.base_spec(), E.inc_union(stores(), granted)), effective_scope
	end
	-- registered while the optional tools service is up (a follower fiber; withdrawn with it)
	D.ctx:inject({ "tools" }, function(sub)
		sub.tools:register("request_permissions", {
			description = "Request additional read/write paths or network hosts, with a justification and turn/session scope. The user may grant only part. Returned permissions are newly approved or reused additional rights; access already in the baseline is omitted. Denied requests grant nothing. Subsequent calls can use these scopes without asking again.",
			side_effect = "read",
			idempotent = false,
			permission_request = true,
			parameters = {
				type = "object",
				additionalProperties = false,
				required = { "additional_permissions", "justification" },
				properties = {
					additional_permissions = {
						type = "object",
						description = "Extra read/write paths or network hosts to request beyond the current sandbox.",
						additionalProperties = false,
						properties = {
							read = {
								type = "array",
								description = "Additional paths to allow reading.",
								items = { type = "string" },
							},
							write = {
								type = "array",
								description = "Existing physical directories to make writable. File or missing-directory roots are refused; request a containing directory only when that access is intended.",
								items = { type = "string" },
							},
							network = {
								description = "Set true for general network access, or list specific hostnames to allow.",
								oneOf = {
									{ type = "boolean" },
									{ type = "array", items = { type = "string" } },
								},
							},
						},
					},
					justification = {
						type = "string",
						description = "Short explanation for the user stating why this access is needed.",
					},
					scope = {
						type = "string",
						description = "Duration of the grant: current turn (default) or entire session.",
						enum = { "turn", "session" },
						default = "turn",
					},
				},
			},
			execute = function(_, opts)
				return {
					content = mare.json.encode({
						permissions = opts.permissions or {},
						scope = opts.permission_scope or "turn",
					}),
				}
			end,
		})
	end)
	return { normalize = normalize, clean = clean, vote = vote, resolve = resolve, stores = stores }
end
