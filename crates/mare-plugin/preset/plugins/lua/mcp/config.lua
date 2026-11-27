-- mcp.config: configuration sources merged, `${VAR}` expansion, tiers, resolve and blocked;
-- part of the `mcp` plugin (plugins/mcp.lua), loaded at the start of its apply.
return function(D)
	local cfg = D.cfg
	local config_errors, codemode, read_json, TIERS = D.config_errors, D.codemode, D.read_json, D.TIERS
	local enabled, approved = D.approvals.enabled, D.approvals.approved
	local lib = require("mare_lib")

	-- `${VAR}`, `${env:VAR}`, `${VAR:-default}`; unknown variables are collected
	local function expand(s, missing)
		if type(s) ~= "string" then
			return s
		end
		return (
			s:gsub("%${([^}]*)}", function(inner)
				local body = inner:gsub("^env:", "")
				local name, default = body:match("^([A-Z0-9_]+):%-(.*)$")
				name = name or body:match("^([A-Z0-9_]+)$")
				if not name then
					return nil -- not a variable reference: keep as written
				end
				local v = mare.env.get(name)
				if v ~= nil and v ~= "" then
					return v
				end
				if default then
					return default
				end
				missing[name] = true
				return ""
			end)
		)
	end

	local function copy_tree(v)
		if type(v) == "table" then
			local out = {}
			for k, x in pairs(v) do
				out[k] = copy_tree(x)
			end
			return out
		end
		return v
	end

	local function expand_tree(v, missing)
		if type(v) == "string" then
			return expand(v, missing)
		elseif type(v) == "table" then
			local out = {}
			for k, x in pairs(v) do
				out[k] = expand_tree(x, missing)
			end
			return out
		end
		return v
	end

	-- KEY=VALUE lines (`export`, quotes and `#` comments allowed)
	local function parse_dotenv(text)
		local out = {}
		for raw in (text .. "\n"):gmatch("([^\n]*)\n") do
			local line = raw:gsub("\r$", "")
			local k, v = line:match("^%s*export%s+([%w_]+)%s*=%s*(.-)%s*$")
			if not k then
				k, v = line:match("^%s*([%w_]+)%s*=%s*(.-)%s*$")
			end
			if k then
				local q = v:sub(1, 1)
				if (q == '"' or q == "'") and v:sub(-1) == q and #v >= 2 then
					v = v:sub(2, -2)
				else
					v = v:gsub("%s+#.*$", "")
				end
				out[k] = v
			end
		end
		return out
	end
	-- ---- configuration -------------------------------------------------------------

	local SERVER_ALIASES = {
		enabledTools = "enabled_tools",
		disabledTools = "disabled_tools",
		exposure = "tier",
		toolExposure = "tools",
		bearerTokenEnvVar = "bearer_token_env_var",
		bearerToken = "bearer_token",
		envHttpHeaders = "env_http_headers",
		envFile = "env_file",
		envInclude = "env_include",
		envExclude = "env_exclude",
		startupTimeoutMs = "startup_timeout_ms",
		callTimeoutMs = "call_timeout_ms",
	}
	local OAUTH_ALIASES = {
		scope = "scopes",
		clientId = "client_id",
		clientSecret = "client_secret",
		clientName = "client_name",
		callbackPort = "callback_port",
		authServerMetadataUrl = "auth_server_metadata_url",
	}

	local function find_alias_errors(raw, file, name)
		local errs = {}
		local keys = {}
		for k in pairs(raw) do
			if type(k) == "string" and SERVER_ALIASES[k] then
				keys[#keys + 1] = k
			end
		end
		table.sort(keys)
		for _, k in ipairs(keys) do
			errs[#errs + 1] = ("%s: server %s: %q is not accepted in init.lua; use %q"):format(
				file,
				name,
				k,
				SERVER_ALIASES[k]
			)
		end
		if type(raw.oauth) == "table" then
			local okeys = {}
			for k in pairs(raw.oauth) do
				if type(k) == "string" and OAUTH_ALIASES[k] then
					okeys[#okeys + 1] = k
				end
			end
			table.sort(okeys)
			for _, k in ipairs(okeys) do
				errs[#errs + 1] = ("%s: server %s: oauth.%s is not accepted in init.lua; use oauth.%s"):format(
					file,
					name,
					k,
					OAUTH_ALIASES[k]
				)
			end
		end
		return errs
	end

	--- normalize one entry; camelCase aliases are only accepted for external JSON files
	local function normalize(raw, accept_external_aliases)
		local s = {}
		for k, v in pairs(raw) do
			s[k] = v
		end
		if accept_external_aliases then
			for from, to in pairs(SERVER_ALIASES) do
				if s[from] ~= nil and s[to] == nil then
					s[to] = s[from]
				end
				s[from] = nil
			end
			if type(s.oauth) == "table" then
				local oauth = {}
				for k, v in pairs(s.oauth) do
					oauth[k] = v
				end
				for from, to in pairs(OAUTH_ALIASES) do
					if oauth[from] ~= nil and oauth[to] == nil then
						oauth[to] = oauth[from]
					end
					oauth[from] = nil
				end
				s.oauth = oauth
			end
		end
		if type(s.tools) == "table" and #s.tools > 0 then
			s.tools = nil -- a list (some clients' allow-list) is not a tier map
		end
		return s
	end

	local OVERLAY = { "enabled", "tier", "tools", "enabled_tools", "disabled_tools", "description", "required" }
	local merged = {} -- name → { raw, source, file, overlays, rejected? }
	local order = {}
	local function add_source(map, source, file)
		if type(map) ~= "table" then
			return
		end
		local accept_external_aliases = source ~= "config"
		local names = {}
		for name in pairs(map) do
			names[#names + 1] = name
		end
		table.sort(names)
		for _, name in ipairs(names) do
			local raw = map[name]
			if type(name) ~= "string" or not name:match("^[%w_%-]+$") then
				config_errors[#config_errors + 1] = ("%s: invalid server name %q (letters, digits, _ and -)"):format(
					file,
					tostring(name)
				)
			elseif type(raw) ~= "table" then
				config_errors[#config_errors + 1] = ("%s: server %s is not an object"):format(file, name)
			else
				local rejected = nil
				if not accept_external_aliases then
					local alias_errs = find_alias_errors(raw, file, name)
					if #alias_errs > 0 then
						for _, err in ipairs(alias_errs) do
							config_errors[#config_errors + 1] = err
						end
						rejected = table.concat(alias_errs, "; ")
					end
				end
				raw = normalize(raw, accept_external_aliases)
				local prev = merged[name]
				if prev and not rejected and raw.command == nil and raw.url == nil and raw.type == nil then
					-- an override of enabled / tier / tools only
					local changed = false
					for _, k in ipairs(OVERLAY) do
						if raw[k] ~= nil then
							prev.raw[k] = raw[k]
							changed = true
						end
					end
					if changed then
						prev.source = source
						prev.file = file
						prev.overlays = prev.overlays or {}
						prev.overlays[#prev.overlays + 1] = { source = source, file = file }
					end
				else
					if not prev then
						order[#order + 1] = name
					end
					merged[name] = { raw = raw, source = source, file = file, rejected = rejected }
				end
			end
		end
	end
	local function servers_of(j)
		return type(j) == "table" and (j.mcpServers or j.mcp_servers or j.servers) or nil
	end
	local function add_file(path, source, project_safe)
		local j, err = read_json(path, project_safe)
		if err then
			config_errors[#config_errors + 1] = err
		elseif j then
			add_source(servers_of(j), source, path)
		end
	end
	add_source(cfg.servers or {}, "config", "init.lua")
	if cfg.files ~= false then
		add_file(mare.paths.config .. "/mcp.json", "user")
		local envfile = mare.env.get("MARE_MCP_CONFIG")
		if envfile and envfile ~= "" then
			add_file(envfile, "env")
		end
		local root_mcp = lib.project_file(".mcp.json")
		if root_mcp then
			add_file(root_mcp, "project", true)
		end
		local dot_mcp = lib.project_file(".mare/mcp.json")
		if dot_mcp then
			add_file(dot_mcp, "project", true)
		end
	end

	local function server_tier(name, srv)
		local t = srv.tier or cfg.tier or "deferred"
		if not TIERS[t] then
			mare.ui.notify(("mcp %s: unknown tier %q, using deferred"):format(name, tostring(t)), "warning")
			t = "deferred"
		end
		if t == "codemode" and not codemode() then
			mare.ui.notify(
				("mcp %s: no registered executor for codemode exposure; using deferred discovery"):format(name),
				"warning"
			)
		end
		return t
	end

	--- the effective, expanded spec of an entry; fills e.missing
	local function resolve(e)
		local raw = e.raw
		local missing = {}
		local defer_env = (e.source == "project" and (not mare.trusted or not approved(e)))
			or (e.source == "env" and not approved(e))
		local srv = defer_env and copy_tree(raw) or expand_tree(raw, missing)
		-- env_file (relative to the server's cwd or the project), then env over it.
		-- Project-supplied `env_file` is never resolved before trust and approval, and is
		-- confined strictly within `mare.project.root`.
		if type(srv.env_file) == "string" and srv.env_file ~= "" then
			local is_proj = e.source == "project"
			if defer_env then
				-- Defer reading env_file until trusted and approved.
			elseif is_proj then
				local root = mare.project and mare.project.root
				local raw_p = srv.env_file
				local escaped = raw_p:match("^/")
					or raw_p:match("^%a:[/\\]")
					or raw_p:sub(1, 1) == "\\"
					or raw_p == ".."
					or raw_p:match("^%.%.[/\\]")
					or raw_p:match("[/\\]%.%.$")
					or raw_p:match("[/\\]%.%.[/\\]")
				if type(root) ~= "string" or root == "" or escaped then
					e.problem = "env_file must be a project-relative path without parent traversal: " .. raw_p
				else
					local base = srv.cwd or root
					local rok_root, r_root = pcall(mare.fs.resolve_path, root)
					local rok_file, r_file = pcall(mare.fs.resolve_path, raw_p, base)
					local sep = mare.os == "windows" and "\\" or "/"
					local in_root = rok_root
						and rok_file
						and type(r_root) == "table"
						and type(r_file) == "table"
						and type(r_root.physical) == "string"
						and type(r_file.physical) == "string"
						and (
							r_file.physical:sub(1, #r_root.physical + 1) == r_root.physical .. "/"
							or (
								mare.os == "windows"
								and r_file.physical:sub(1, #r_root.physical + 1):lower()
									== (r_root.physical .. sep):lower()
							)
						)
					if not in_root then
						e.problem = "env_file outside project root: " .. raw_p
					else
						local rel_from_root = r_file.physical:sub(#r_root.physical + 2)
						local ok, text = pcall(mare.fs.read, rel_from_root, {
							max_bytes = 256 * 1024,
							no_follow = true,
							cwd = r_root.physical,
						})
						if ok and type(text) == "string" then
							local env = parse_dotenv(text)
							for k, v in pairs(type(srv.env) == "table" and srv.env or {}) do
								env[k] = v
							end
							srv.env = env
						else
							e.problem = "env_file not readable: " .. raw_p
						end
					end
				end
			else
				local p = srv.env_file
				if not p:match("^/") and not p:match("^%a:[/\\]") then
					p = (srv.cwd or mare.cwd) .. "/" .. p
				end
				local ok, text = pcall(mare.fs.read, p)
				if ok and type(text) == "string" then
					local env = parse_dotenv(text)
					for k, v in pairs(type(srv.env) == "table" and srv.env or {}) do
						env[k] = v
					end
					srv.env = env
				else
					e.problem = "env_file not readable: " .. p
				end
			end
		end
		if not defer_env and type(srv.bearer_token_env_var) == "string" then
			local v = mare.env.get(srv.bearer_token_env_var)
			if v and v ~= "" then
				srv.bearer_token = v
			else
				missing[srv.bearer_token_env_var] = true
			end
		end
		if not defer_env and type(srv.env_http_headers) == "table" then
			srv.headers = type(srv.headers) == "table" and srv.headers or {}
			for h, var in pairs(srv.env_http_headers) do
				local v = mare.env.get(tostring(var))
				if v and v ~= "" then
					srv.headers[h] = v
				else
					missing[tostring(var)] = true
				end
			end
		end
		if type(srv.cwd) == "string" and not srv.cwd:match("^/") and not srv.cwd:match("^%a:[/\\]") then
			srv.cwd = mare.cwd .. "/" .. srv.cwd
		end
		local list = {}
		for k in pairs(missing) do
			list[#list + 1] = k
		end
		table.sort(list)
		e.missing = list
		return srv
	end
	--- why an entry can't start now, or nil
	local function blocked(e)
		if e.rejected then
			return "error", e.rejected
		end
		local is_enabled, enabled_err = enabled(e)
		if enabled_err then
			return "error", enabled_err
		elseif not is_enabled then
			return "disabled", "disabled"
		elseif e.tier == "hidden" then
			return "disabled", "tier hidden"
		end
		local is_approved, approved_err = approved(e)
		if approved_err then
			return "error", approved_err
		elseif not is_approved then
			return "needs_approval",
				("%s server from %s — enable it with /mcp enable %s (headless: --approve-mcps)"):format(
					e.source,
					e.file,
					e.name
				)
		elseif e.source == "project" and e.srv.command and not mare.trusted then
			return "untrusted", "project stdio servers start only in a trusted project"
		end
		return nil
	end

	return {
		merged = merged,
		order = order,
		resolve = resolve,
		server_tier = server_tier,
		blocked = blocked,
	}
end
