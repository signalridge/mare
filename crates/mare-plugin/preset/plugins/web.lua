-- Generic web capabilities. Backends own retrieval; this service owns selection and tools.
return {
	name = "web",
	provide = { "web" },
	inject = { "tools", "bus" },
	config_schema = {
		web_search = {
			type = "enum",
			choices = { "disabled", "cached", "indexed", "live" },
			default = "cached",
			desc = "Web search mode.",
		},
		search = { type = "boolean", default = true, desc = "Offer the web search tool." },
		fetch = { type = "boolean", default = true, desc = "Offer the web fetch tool." },
		search_max_queries = { type = "integer", default = 10, desc = "Most queries in one search call." },
		search_max_results = { type = "integer", default = 10, desc = "Most results per query." },
		search_summary_max_chars = { type = "integer", default = 800, desc = "Characters kept of a result summary." },
		search_timeout_ms = { type = "integer", default = 30000, desc = "Search timeout." },
		fetch_timeout_ms = { type = "integer", default = 30000, desc = "Fetch timeout." },
		fetch_max_output_chars = { type = "integer", default = 200000, desc = "Most characters a fetch returns." },
		native_max_uses = {
			type = "integer",
			default = 5,
			desc = "Most uses of a provider's native search per request.",
		},
		search_provider = {
			type = "string",
			desc = "Search provider name (overridden by MARE_WEB_SEARCH_PROVIDER).",
		},
		fetch_provider = { type = "string", desc = "Fetch provider name (overridden by MARE_WEB_FETCH_PROVIDER)." },
		allowed_domains = {
			type = "list",
			items = { type = "string" },
			desc = "Native search: only these domains (exclusive with blocked_domains).",
		},
		blocked_domains = {
			type = "list",
			items = { type = "string" },
			desc = "Native search: never these domains (exclusive with allowed_domains).",
		},
		context_size = {
			type = "enum",
			choices = { "low", "medium", "high" },
			desc = "Native search context size.",
		},
		location = { type = "map", desc = "Approximate user location passed to native search." },
		native_version = { type = "string", desc = "Version of the provider-native search tool." },
	},
	apply = function(ctx, cfg)
		cfg.search_provider = mare.env("MARE_WEB_SEARCH_PROVIDER") or cfg.search_provider
		cfg.fetch_provider = mare.env("MARE_WEB_FETCH_PROVIDER") or cfg.fetch_provider
		assert(
			cfg.web_search == "disabled"
				or cfg.web_search == "cached"
				or cfg.web_search == "indexed"
				or cfg.web_search == "live",
			"web_search must be disabled, cached, indexed or live"
		)
		for _, key in ipairs({
			"search_max_queries",
			"search_max_results",
			"search_summary_max_chars",
			"search_timeout_ms",
			"fetch_timeout_ms",
			"fetch_max_output_chars",
			"native_max_uses",
		}) do
			assert(
				type(cfg[key]) == "number" and cfg[key] > 0 and cfg[key] % 1 == 0 and cfg[key] < 2147483648,
				key .. " must be a positive bounded integer"
			)
		end
		assert(type(cfg.search) == "boolean" and type(cfg.fetch) == "boolean", "search/fetch must be booleans")
		assert(
			not (cfg.allowed_domains and cfg.blocked_domains),
			"allowed_domains and blocked_domains are mutually exclusive"
		)
		if cfg.context_size then
			assert(
				cfg.context_size == "low" or cfg.context_size == "medium" or cfg.context_size == "high",
				"invalid context_size"
			)
		end
		-- the optional llm and context services, read on each call
		local svc = require("mare_lib").followers(ctx, { "llm", "context" })
		local service, has_selected = require("web.registry")(ctx, cfg, svc)
		local web = require("mare_lib").service(ctx, "web", service)
		require("web.tools")(ctx, cfg, svc, web, has_selected)
		local function valid_mode(mode)
			return mode == "disabled" or mode == "cached" or mode == "indexed" or mode == "live"
		end
		local function invalidate_web()
			ctx.tools:invalidate()
			if svc.context then
				svc.context:invalidate()
			end
		end
		local function selected_id(res)
			for _, p in ipairs(res.data) do
				if p.selected then
					return p.provider_id
				end
			end
			return nil
		end
		ctx:on("kernel/doctor", function(checks)
			for _, kind in ipairs({ "search", "fetch" }) do
				local rows = web:list_providers(kind).data
				checks[#checks + 1] =
					{ name = "web " .. kind .. " backends", ok = true, detail = mare.json.encode(rows) }
			end
		end)
		ctx:on("option/changed", function(ev)
			if ev.name == "web_search" then
				if valid_mode(ev.value) then
					cfg.web_search = ev.value
					invalidate_web()
				else
					mare.log("warn", "mare.o.web_search: must be disabled, cached, indexed or live")
				end
			elseif ev.name == "web_search_provider" or ev.name == "search_provider" then
				if ev.value == nil or (type(ev.value) == "string" and ev.value ~= "") then
					cfg.search_provider = ev.value
					invalidate_web()
				else
					mare.log("warn", "mare.o." .. ev.name .. ": must be a non-empty string or nil")
				end
			elseif ev.name == "web_fetch_provider" or ev.name == "fetch_provider" then
				if ev.value == nil or (type(ev.value) == "string" and ev.value ~= "") then
					cfg.fetch_provider = ev.value
					invalidate_web()
				else
					mare.log("warn", "mare.o." .. ev.name .. ": must be a non-empty string or nil")
				end
			end
		end)
		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("web", {
				description = "List or switch search and fetch backends (/web [mode|search|fetch <val>])",
				run = function(args)
					local sub, val = (args or ""):match("^%s*(%S+)%s*(.-)%s*$")
					if sub == "mode" then
						assert(valid_mode(val), "usage: /web mode disabled|cached|indexed|live")
						cfg.web_search = val
						invalidate_web()
						return "web_search mode: " .. val
					elseif sub == "search" or sub == "fetch" then
						assert(val ~= "", "usage: /web " .. sub .. " <id|auto>")
						cfg[sub .. "_provider"] = val ~= "auto" and val or nil
						invalidate_web()
						local res = web:list_providers(sub)
						local id = selected_id(res)
						return sub .. " selection: " .. (id or ("unavailable (" .. tostring(res.reason) .. ")"))
					elseif sub and sub ~= "" then
						error("usage: /web [mode <disabled|cached|indexed|live> | search <id|auto> | fetch <id|auto>]")
					end
					local lines = { "mode: " .. cfg.web_search }
					for _, kind in ipairs({ "search", "fetch" }) do
						local res = web:list_providers(kind)
						local id = selected_id(res)
						lines[#lines + 1] = kind .. " active: " .. (id or tostring(res.reason or "unavailable"))
						for _, p in ipairs(res.data) do
							lines[#lines + 1] = kind
								.. ": "
								.. p.provider_id
								.. (p.available and " (available)" or " (unavailable)")
						end
					end
					return table.concat(lines, "\n")
				end,
			})
		end)
	end,
}
