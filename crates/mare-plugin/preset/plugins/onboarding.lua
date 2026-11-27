-- First run: whenever no
-- provider has a usable credential — whether or not init.lua exists — an interactive session
-- walks through picking a provider, logging in (subscription OAuth or a masked API key) and
-- choosing a default model, and says that a key can also come from an environment variable or
-- a command. The result goes to state.json (the model) and auth.json (credentials); init.lua is
-- only written by `mare init`. `/login` and `/model` do the same later.

return {
	name = "onboarding",
	-- `task` gives ctx:spawn. The guided login is offered on ui/ready, once per launch: a reload
	-- does not offer it again (/login does it later)
	inject = { "llm", "task" },
	config_schema = {
		enabled = {
			type = "boolean",
			default = true,
			desc = "Offer the guided first-run login when no credentials exist.",
		},
		plugins = { type = "boolean", default = true, desc = "Offer optional plugins on first open." },
	},
	apply = function(ctx, cfg)
		local function needed()
			if not cfg.enabled or mare.mode ~= "tui" or not mare.ui.connected() then
				return false
			end
			-- an init.lua is no reason to skip it: a user without any key would only see an error
			return not ctx.llm:has_credentials()
		end

		--- run() -> model ref | nil: the guided first-run flow (also callable from a command)
		local function run()
			local list = {}
			for _, p in ipairs(ctx.llm:providers()) do
				if p.login then
					list[#list + 1] = p
				end
			end
			if #list == 0 then
				return nil
			end
			local items = {}
			for i, p in ipairs(list) do
				items[i] = { label = p.name, detail = p.login_label }
			end
			items[#items + 1] = { label = "Use an environment variable or a command", detail = "no login stored" }
			local i = mare.ui.select({ title = "Welcome to mare — pick a provider to log in", items = items })
			if i == #items then
				local envs = {}
				for _, l in ipairs(ctx.llm:status_lines()) do
					if not l.logged_in and not l.warning and not l.unavailable then
						envs[#envs + 1] = "  " .. l.text
					end
				end
				local U = mare.ui
				local lines = {
					U.Line({ U.Span("Provide a key without logging in:"):hl("MareTitle") }),
					U.Line({ U.Span("  • set the provider's variable (OPENAI_API_KEY, ANTHROPIC_API_KEY, …), or") }),
					U.Line({
						U.Span(
							'  • in init.lua: mare.provider("openai", { api_key = { cmd = { "pass", "show", "openai" } } })'
						),
					}),
					U.Line({ U.Span("Then restart mare. Status per provider:"):hl("MareMuted") }),
				}
				for _, e in ipairs(envs) do
					lines[#lines + 1] = U.Line({ U.Span(e):hl("MareMuted") })
				end
				U.print(U.Lines(lines))
				return nil
			end
			local p = i and list[i]
			if not p then
				return nil
			end
			local ok, err = pcall(ctx.llm.login, ctx.llm, p.name)
			if not ok then
				mare.ui.notify("Login failed: " .. tostring(err) .. " — try /login later", "error")
				return nil
			end
			local models = {}
			for _, m in ipairs(ctx.llm:models()) do
				if m.provider == p.name then
					models[#models + 1] = m
				end
			end
			local ref
			if #models == 1 then
				ref = models[1].ref
			elseif #models > 1 then
				local mi = {}
				for k, m in ipairs(models) do
					mi[k] = { label = m.id, detail = m.name }
				end
				local j = mare.ui.select({ title = "Default model", items = mi })
				ref = j and models[j].ref
			end
			if ref then
				ctx.llm:set_model(ref)
				mare.ui.notify("Ready: " .. ref .. " — /model and /login change it later", "success")
			end
			return ref
		end

		ctx:on("ui/ready", function()
			if mare.mode ~= "tui" or not mare.ui.connected() then
				return
			end
			ctx:spawn(function()
				if needed() then
					run()
				end
				local args = mare.args or {}
				if cfg.plugins == false or args.continue or args.resume ~= nil then
					return
				end
				local plugins = ctx:get("plugins")
				if not plugins or not plugins.choose_plugins then
					return
				end
				-- The plugin chooser checks the saved answer before showing Discover.
				local result = plugins:choose_plugins()
				if result and #result.enabled > 0 then
					if result.synced then
						-- the last step, the same entry as a bare `/reload` (commands/reload.lua): the
						-- full reload loads the newly installed packages (a kernel reload does not
						-- rescan them) and restarts this plugin too; the state key is written already
						local ok, err = pcall(mare.reload)
						if not ok then
							mare.ui.notify(
								"New plugins not loaded yet (" .. tostring(err) .. ") — run /reload when ready",
								"warning"
							)
						end
					else
						mare.ui.notify(
							"Plugins selection saved — new plugins not loaded yet; run /plugins sync or /reload later",
							"warning"
						)
					end
				end
			end)
		end)
	end,
}
