-- mcp: Model Context Protocol servers (built in, on by default, does nothing until a server is
-- configured; `mare.plugin("mcp", { enabled = false })` turns it off). The protocol and the
-- transports (stdio, streamable HTTP, OAuth) are the core primitive `mare.mcp` (official Rust
-- SDK rmcp); this plugin does configuration, exposure tiers, tool registration, prompts,
-- resources, elicitation and `/mcp`.
--
--   mare.plugin("mcp", { servers = {
--     github = { command = "npx", args = { "-y", "@modelcontextprotocol/server-github" },
--                env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }, tier = "direct" },
--     docs = { url = "https://example.com/mcp", headers = { ["x-team"] = "a" } },
--     linear = { url = "https://mcp.linear.app/mcp", oauth = true, -- `/mcp login linear`
--                tools = { delete_issue = "hidden" } }, -- per-tool tier
--   } })
--
-- Configuration sources, later ones override earlier ones: `servers`
-- above, `~/.config/mare/mcp.json`, the file named by `$MARE_MCP_CONFIG`, then the project's
-- `.mcp.json` and `.mare/mcp.json` (`{"mcpServers": {...}}`). A project entry without
-- `command`/`url`/`type` only overrides enabled / tier / tools / enabled_tools / disabled_tools /
-- description of the server with the same name. Project and `$MARE_MCP_CONFIG` servers start only
-- once approved (keyed by name + a hash of their configuration, `<state>/mcp-approvals.json`;
-- headless: `--approve-mcps`), and project stdio servers only in a trusted project. `/mcp enable`
-- / `disable` are remembered per project in `<state>/mcp-disabled.json`, never written back to a
-- config file.
--
-- Per-server keys: command, args, env, env_file, cwd, env_include, env_exclude | url, headers,
-- env_http_headers, bearer_token, bearer_token_env_var, oauth = true | { scopes, client_id,
-- client_secret, client_name, callback_port, auth_server_metadata_url }, tls = { ca_bundle };
-- plus type, enabled, required, tier, tools = { [tool] = tier }, enabled_tools, disabled_tools
-- (`*` patterns), description, startup_timeout_ms (default 60 s, at most 300 s),
-- call_timeout_ms. `required = true`: a request waits for this server, with no
-- time cap, until it is connected or failed; a failure denies the request with the reason.
-- Strings expand `${VAR}`, `${env:VAR}` and `${VAR:-default}` (names A-Z 0-9 _).
--
-- Exposure tiers (default `deferred`):
--   direct — connected in the background at startup, tools declared like built-ins; a request
--              never waits for it: while it is still connecting its tools join from the next turn
--
--   deferred — not declared and not connected at startup; the model finds tools with
--              `tool_search` (invoked natively on `tool_update = "addition_only"` models or via
--              `call_tool` otherwise); uncached servers connect on first search, cached on first use
--   codemode — only callable from a codemode script; needs a codemode plugin (otherwise deferred)
--   hidden — not available
-- Tool declarations change only between requests; the system prompt
-- gets one line per server. Tool names are `mcp__<server>__<tool>`, with a short hash suffix when
-- longer than 64 characters or colliding; two servers whose
-- names sanitize alike are an error, never a silent override.
--
-- Events (emit): mcp/ready {server, tools}, mcp/error {server, error},
-- mcp/codemode {server, tools}, mcp/changed {server}. Listens: mcp/cli {action, server, lines,
-- code} (`mare mcp …`), doctor.
--
-- Service `mcp`: list_servers() -> { data = { { name, state, source, tier, transport, target,
-- tools, error?, missing? }, … }, errors = { config error, … } }, sorted like `/mcp`. A read-only
-- snapshot: it never connects a server (state is what `/mcp` and `mare doctor` show).

local TIERS = { direct = true, deferred = true, codemode = true, hidden = true }
local SOURCE_RANK = { project = 1, user = 2, env = 3, config = 4 }
local MAX_NAME = 64

local function glob_match(pat, s)
	local p = "^" .. pat:gsub("[%^%$%(%)%%%.%[%]%+%-%?]", "%%%0"):gsub("%*", ".*") .. "$"
	return s:match(p) ~= nil
end

local function sanitize(s)
	return (tostring(s):gsub("[^%w_%-]", "_"))
end

return {
	name = "mcp",
	inject = { "tools", "bus", "task" },
	provide = { "mcp" },
	config_schema = {
		-- name → { command, args?, env?, cwd? } | { url, headers?, bearer_token?, oauth? } plus
		-- the keys listed at the top of this file
		servers = { type = "map", default = {}, desc = "MCP servers by name." },
		tier = { type = "string", default = "deferred", desc = "Default exposure tier for servers without their own." },
		startup_timeout_ms = {
			type = "integer",
			default = 60000,
			desc = "Initialize plus the first listings (at most 300000).",
		},
		call_timeout_ms = {
			type = "integer",
			default = 120000,
			desc = "One tools/call (progress notifications reset it).",
		},
		text_limit = {
			type = "integer",
			default = 20 * 1024,
			desc = "Longer text results are cut in the middle.",
		},
		files = {
			type = "boolean",
			default = true,
			desc = "Read mcp.json / .mcp.json / .mare/mcp.json / $MARE_MCP_CONFIG.",
		},
	},
	apply = function(ctx, cfg)
		-- the optional services, read on each call
		local svc = require("mare_lib").followers(ctx, { "agent", "llm", "approvals", "commands" })
		local servers = {} -- name → entry (see new_entry)
		local owned = {} -- tool name → server, for every tool this plugin registered
		local config_errors = {}
		local codemode = function()
			return ctx.tools:executor("codemode") ~= nil
		end
		local state_dir = (mare.paths and mare.paths.state or ".")
		local cache_dir = state_dir .. "/mcp-tools"
		local approvals_path = state_dir .. "/mcp-approvals.json"
		local disabled_path = state_dir .. "/mcp-disabled.json"
		local startup_cap = 300000

		local function read_json(path, project_safe)
			if not mare.fs.exists(path) then
				return nil
			end
			local ok, text, rerr
			if project_safe then
				ok, text, rerr = pcall(mare.fs.read, path, { max_bytes = 256 * 1024, no_follow = true })
			else
				ok, text, rerr = pcall(mare.fs.read, path)
			end
			if not ok then
				return nil, ("%s: %s"):format(path, tostring(text))
			end
			if type(text) ~= "string" then
				return nil, ("%s: %s"):format(path, tostring(rerr or "read failed"))
			end
			local dok, j, derr = pcall(mare.json.decode, text)
			if not dok then
				return nil, ("%s: invalid JSON (%s)"):format(path, tostring(j))
			end
			if j == nil then
				return nil, ("%s: invalid JSON (%s)"):format(path, tostring(derr or "null"))
			end
			return j
		end
		local function write_json(path, v)
			local ok, w, err = pcall(mare.fs.write, path, mare.json.encode(v), { atomic = true, mode = 384 })
			if not ok then
				return nil, ("%s: %s"):format(path, tostring(w))
			end
			if not w then
				return nil, ("%s: %s"):format(path, tostring(err or "write failed"))
			end
			return true
		end

		local function agent_notify(text)
			pcall(function()
				local agent = svc.agent
				if agent then
					agent:notify(text)
				end
			end)
		end

		-- structural submodules (plugins/lua/mcp/), loaded in dependency order; the tool list cache,
		-- the readiness step and the system prompt section stay here between them, so
		-- every apply-time registration keeps its place
		local D = {
			ctx = ctx,
			svc = svc,
			cfg = cfg,
			servers = servers,
			owned = owned,
			config_errors = config_errors,
			codemode = codemode,
			approvals_path = approvals_path,
			disabled_path = disabled_path,
			startup_cap = startup_cap,
			read_json = read_json,
			write_json = write_json,
			agent_notify = agent_notify,
			TIERS = TIERS,
			SOURCE_RANK = SOURCE_RANK,
			MAX_NAME = MAX_NAME,
			glob_match = glob_match,
			sanitize = sanitize,
		}
		-- result shaping, elicitation, server prompts, the /mcp and `mare mcp` actions and the
		-- public catalog register nothing when loaded: each loads the first time its field on D is
		-- read (a server connects, a tool returns, /mcp or `mare mcp` runs) instead of at startup
		local lib = require("mare_lib")
		local function cold(loader)
			local _, proxy = lib.lazy_group({}, {}, loader)
			return proxy
		end
		D.results = cold(function()
			return require("mcp.results")(D)
		end)
		D.elicit = cold(function()
			return require("mcp.elicit")
		end)
		D.prompts = cold(function()
			return require("mcp.prompts")(D)
		end)
		D.actions = cold(function()
			return require("mcp.actions")(D)
		end)
		D.catalog = cold(function()
			return require("mcp.catalog")(D)
		end)
		D.approvals = require("mcp.approvals")(D)
		D.config = require("mcp.config")(D)
		D.exposure = require("mcp.exposure")(D)
		local enabled = D.approvals.enabled
		local order, blocked = D.config.order, D.config.blocked
		local eager = D.exposure.eager

		-- ---- tool list cache (non-direct servers: tool_search works without connecting) ---

		local function cache_path(name)
			return ("%s/%s.json"):format(cache_dir, sanitize(name))
		end
		local function load_cache(name)
			local ok, text = pcall(mare.fs.read, cache_path(name))
			if not ok or type(text) ~= "string" then
				return nil
			end
			local decoded, j = pcall(mare.json.decode_lossless, text)
			return decoded and type(j) == "table" and j.version == 2 and type(j.tools) == "table" and j.tools or nil
		end
		-- written outside tool calls (a write from inside one goes through the sandbox and the
		-- snapshot): queued here, flushed before the next request and at dispose
		local pending_cache = {}
		local function flush_cache()
			for name, tools in pairs(pending_cache) do
				pending_cache[name] = nil
				pcall(
					mare.fs.write,
					cache_path(name),
					mare.json.encode_strict({ version = 2, tools = tools }),
					{ atomic = true, mode = 384 }
				)
			end
		end
		ctx:effect(function()
			return flush_cache
		end, "cleanup")

		D.load_cache, D.pending_cache = load_cache, pending_cache
		D.connection = require("mcp.connection")(D)
		D.startup = require("mcp.startup")(D)
		local connect = D.connection.connect
		local tools_allowed = D.startup.tools_enabled_allows_server

		-- send-time wait: only `required` servers are waited for — until they connect
		-- or fail, no time cap — and a failed one stops the request in the ready step below. Other
		-- servers (`direct` included) never delay a prompt: one still connecting contributes its
		-- tools from the next turn on (a connect that finishes mid-turn is queued by in_turn) and
		-- /mcp shows it as "connecting"
		ctx:inject({ "llm" }, function(sub)
			-- send-time readiness, one step of llm's ordered chain: withdrawn with llm, registered
			-- again for a new provider. No time cap
			-- (approvals may need a human; connection I/O has its own timeout)
			sub.llm:readiness("mcp", {
				order = require("mare_lib").READY_ORDER.mcp,
				prepare = function(event)
					flush_cache()
					local cancel = event and event.cancel
					-- A required server whose connection dropped (stdio exit or expired HTTP session)
					-- would otherwise stay disconnected forever because `ready` denies before any lazy
					-- tool call can reconnect it. Attempt recovery before checking readiness.
					for _, n in ipairs(order) do
						local e = servers[n]
						if
							e.srv.required
							and enabled(e)
							and tools_allowed(n)
							and not blocked(e)
							and not e.connecting
							and not e.approval_pending
						then
							if e.client then
								local ok_st, st = pcall(e.client.status, e.client)
								if (not ok_st or st ~= "connected") and type(e.client.reconnect) == "function" then
									e.connecting = true
									local _reconn <close> = require("mare_lib").guard(function()
										e.connecting = false
									end)
									pcall(mare.ui.working, "Reconnecting MCP: " .. n)
									local ok_rc, rc_err = pcall(e.client.reconnect, e.client)
									if ok_rc then
										e.state, e.error, e.told = "connected", nil, nil
									else
										D.connection.fail(e, rc_err, true)
										-- Release our own connecting flag before disconnect so it
										-- does not wait 5 s on the flag held by this coroutine.
										e.connecting = false
										_reconn:disarm()
										D.connection.disconnect(e)
									end
								end
							elseif e.state == "disconnected" or e.state == "error" or e.state == "idle" then
								pcall(mare.ui.working, "Connecting MCP: " .. n)
								D.connection.connect(n, true, nil, nil, cancel)
							end
						end
					end
					local function pending()
						local names, awaiting_approval = {}, false
						for _, n in ipairs(order) do
							local e = servers[n]
							if e.srv.required and tools_allowed(n) then
								local approval = e.approval_pending and mare.ui.connected() and enabled(e)
								if (e.connecting and eager(e)) or approval then
									names[#names + 1] = n
									awaiting_approval = awaiting_approval or approval
								end
							end
						end
						return names, awaiting_approval
					end
					local names, awaiting_approval = pending()
					local shown = false
					while #names > 0 do
						shown = true
						local label = awaiting_approval and "Waiting for MCP approval: " or "Connecting MCP: "
						pcall(mare.ui.working, label .. table.concat(names, ", "))
						if not mare.task.sleep(25, cancel) then
							return
						end
						names, awaiting_approval = pending()
					end
					-- the turn is running (the ui plugin showed "Thinking" on agent/start): put that back
					-- after our own label, never clear it
					if shown then
						pcall(mare.ui.working, "Thinking")
					end
				end,
				-- recheck on every prompt, including after a failed or cancelled attempt: a denial
				-- stops the request before provider I/O
				ready = function()
					for _, n in ipairs(order) do
						local e = servers[n]
						if e.srv.required and enabled(e) and tools_allowed(n) then
							local reason = mare.mcp.redact(tostring(e.error or e.state))
							local ready = e.client and not e.connecting and e.state == "connected"
							if ready then
								local ok, status = pcall(function()
									return e.client:status()
								end)
								ready = ok and status == "connected"
								if not ready then
									reason = ok and tostring(status) or mare.mcp.redact(tostring(status))
								end
							end
							if not ready then
								return {
									decision = "deny",
									reason = ("required MCP server %s is not available: %s"):format(n, reason),
								}
							end
						end
					end
				end,
			})
		end)

		D.discovery = require("mcp.discovery")(D)
		require("mcp.resources")(D)

		-- ---- system prompt: one line per server -----------------------

		local function description_of(e)
			if type(e.srv.description) == "string" and e.srv.description ~= "" then
				return e.srv.description
			end
			local ins = type(e.info) == "table" and e.info.instructions
			if type(ins) == "string" then
				local first = ins:match("^%s*([^\n]+)")
				if first then
					return first
				end
			end
			return nil
		end
		-- registered while the optional `context` service is up (a child fiber on the cordis tree)
		ctx:inject({ "context" }, function(ctx)
			ctx.context:section({
				name = "mcp",
				order = 9500,
				interpolate = false, -- server names and descriptions: third-party text, verbatim
				text = function()
					local lines = {}
					for _, n in ipairs(order) do
						local e = servers[n]
						if not blocked(e) then
							local effective_tier = D.exposure.resolve_tier(e.tier)
							local how = effective_tier == "direct" and "tools already loaded"
								or effective_tier == "codemode" and "from codemode scripts"
								or "find its tools with tool_search"
							local d = description_of(e)
							lines[#lines + 1] = ("- %s%s (%s)"):format(n, d and (": " .. d) or "", how)
						end
					end
					if #lines == 0 then
						return nil
					end
					return "MCP servers:\n" .. table.concat(lines, "\n")
				end,
			})
		end)

		-- forwarders into the actions module, which loads on the first call
		local function forward(name)
			return function(...)
				return D.actions[name](...)
			end
		end
		local actions = setmetatable({}, {
			__index = function(_, k)
				return D.actions.actions[k]
			end,
		})
		local find, state_of, transport_of = forward("find"), forward("state_of"), forward("transport_of")
		local sorted_entries, summary_line, zero_tools_hint, tools_lines, tools_text =
			forward("sorted_entries"),
			forward("summary_line"),
			forward("zero_tools_hint"),
			forward("tools_lines"),
			forward("tools_text")

		-- ---- /mcp ---------------------------------------------------------------------

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("mcp", {
				description = "MCP servers (/mcp [filter], /mcp reconnect|enable|disable|login|logout|tools <server>)",
				-- the panel and the subcommands load on first use
				run = function(args)
					return require("mcp.command")({
						ctx = ctx,
						servers = servers,
						actions = actions,
						config_errors = config_errors,
						state_of = state_of,
						transport_of = transport_of,
						sorted_entries = sorted_entries,
						summary_line = summary_line,
						zero_tools_hint = zero_tools_hint,
						tools_lines = tools_lines,
						tools_text = tools_text,
					}, args)
				end,
			})
		end)

		-- ---- `mare mcp list | list-tools | enable | disable | login` ---------------------

		ctx:on("mcp/cli", function(req)
			local out = req.lines
			local function say(s)
				out[#out + 1] = s
			end
			local ok, err = pcall(function()
				if req.action == "list" then
					for _, err in ipairs(config_errors) do
						say("config error: " .. err)
						req.code = 1
					end
					for _, e in ipairs(sorted_entries()) do
						if not blocked(e) and not e.client then
							connect(e.name, true)
						end
						local is_enabled, enabled_err = enabled(e)
						local _, approved_err = D.approvals.approved(e)
						local st = state_of(e)
						if enabled_err or approved_err then
							req.code = 1
						elseif e.srv.required and is_enabled and st ~= "connected" and st ~= "disabled" then
							req.code = 1
						end
						say(summary_line(e))
					end
					if #order == 0 then
						say("No MCP servers configured")
					end
				elseif req.action == "list-tools" then
					local e = find(req.server)
					if not blocked(e) then
						connect(e.name, true)
					end
					if not e.client and e.error then
						say(("%s: %s"):format(e.name, e.error))
						req.code = 1
					end
					say(tools_text(e))
				elseif req.action == "enable" or req.action == "disable" then
					find(req.server)
					actions[req.action](req.server)
					say(("%s %sd for %s"):format(req.server, req.action, mare.cwd))
				elseif req.action == "login" then
					actions.login(req.server, function(url)
						say("Open this address to log in: " .. url)
						io.stderr:write("Open this address to log in: " .. url .. "\n")
					end, req.tty and req.ask or nil)
					say("Logged in to " .. req.server)
				else
					error("unknown action " .. tostring(req.action))
				end
			end)
			if not ok then
				say("error: " .. tostring(err))
				req.code = 1
			end
		end)

		local mcp_service = setmetatable({}, {
			__index = function(_, k)
				return D.actions.service[k]
			end,
		})
		ctx:on("tools/executors-changed", function()
			for _, entry in pairs(servers) do
				for _, tool in ipairs(entry.tools or {}) do
					if D.exposure.requested_tier(entry, tool.name) == "codemode" then
						local name = entry.names[tool.name]
						if name then
							D.connection.unregister(entry, name)
						end
					end
				end
			end
		end)
		require("mare_lib").service(ctx, "mcp", {
			catalog = function(_, _, tier)
				return D.catalog.catalog(tier)
			end,
			--- list_servers() -> { data = { … }, errors = { … } } (see the top of this file)
			list_servers = function()
				-- no server configured: the answer the actions module would give, without loading
				-- it (the status bar asks on every draw)
				if #D.config.order == 0 then
					return { data = mare.json.array({}), errors = mare.json.array({ table.unpack(config_errors) }) }
				end
				return mcp_service.list_servers()
			end,
			list_tools = function(_, _, opts)
				return mcp_service.list_tools(opts)
			end,
			connect = function(_, _, server)
				return mcp_service.connect(server)
			end,
			reconnect = function(_, _, server)
				return mcp_service.reconnect(server)
			end,
			list_resources = function(_, _, opts)
				return mcp_service.list_resources(opts)
			end,
			read_resource = function(_, _, server, uri)
				return mcp_service.read_resource(server, uri)
			end,
			list_prompts = function(_, _, opts)
				return mcp_service.list_prompts(opts)
			end,
			get_prompt = function(_, _, server, name, arguments)
				return mcp_service.get_prompt(server, name, arguments)
			end,
		})

		-- `mare doctor`: one line per configured server
		ctx:on("kernel/doctor", function(checks)
			for _, err in ipairs(config_errors) do
				table.insert(checks, { name = "mcp config", ok = false, detail = err })
			end
			for _, n in ipairs(order) do
				local e = servers[n]
				local st = state_of(e)
				table.insert(checks, {
					name = "mcp " .. n,
					ok = e.error == nil or st == "connected",
					detail = e.error and st ~= "connected" and mare.mcp.redact(tostring(e.error))
						or ("%s, %s, %s"):format(e.tier, st, e.client and (#e.tools .. " tools") or "not connected"),
				})
			end
		end)
	end,
}
