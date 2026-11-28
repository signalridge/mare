-- Plugin management panel and update news.
--   /plugins Discover selects catalog plugins; Installed manages packages and plugins;
--                   Sources refreshes catalogs and shows failures. Progress shows live and
--                   Ctrl-C cancels an active operation. Plugins declared in init.lua are shown
--                   but remain managed there.
--   /news the NEWS.md sections of the running mare and the enabled plugins again
-- Package changes (sync, update, a plugin turned on or off, init.lua edited) apply on /reload:
-- new packages are installed and loaded, removed ones unloaded, packages' config() runs again.
-- Update news (LazyVim `news`): after mare or an enabled plugin changes version, its NEWS.md
-- section is printed into the scrollback once; seen versions in $XDG_STATE_HOME/mare/news.json;
-- `mare.o.news = false` turns it off. A fresh install records the versions without showing.
-- Change notices (lazy.nvim `change_detection`, notify only): when init.lua or plugins.json
-- changes on disk, a notice says /reload applies it (`config.watch_ms`, 0 turns it off).
-- Sources (`mare.o.plugins`, read by mare-plugin-manager): url_format = "https://github.com/%s.git" (short
-- names, any depth: "group/sub/repo"), allow_insecure = false (plain http:// and git://),
-- timeout = 120 (seconds per git operation), concurrency = 8, sources = {}. git never
-- prompts; checkouts are <data>/packages/<host>/<path>, so each row shows its source.
-- `mare doctor` gets: git and its version, the lock, plugin names defined twice.
-- Events (emit): plugins/completed {action, name?, status}, plugins/plugin {name, enabled}

return {
	name = "plugins",
	provide = { "plugins" },
	inject = { "ui", "bus", "task", "kernel" },
	config_schema = {
		watch_ms = { type = "integer", default = 2000, desc = "Poll interval of the package watcher." },
	},
	apply = function(ctx, cfg)
		local run_fn, panel_fn, choose_fn, news_mod
		local function get_run()
			if not run_fn then
				run_fn = require("plugins.extras_list").runner(ctx)
			end
			return run_fn
		end
		local function get_panel()
			if not panel_fn then
				panel_fn = require("plugins.panel").create(ctx, get_run())
			end
			return panel_fn
		end
		local function get_choose()
			if not choose_fn then
				choose_fn = require("plugins.choose").create(ctx, get_run())
			end
			return choose_fn
		end
		local function get_news()
			if not news_mod then
				news_mod = require("plugins.news")
			end
			return news_mod
		end

		local function pkg_action(action, name, opts)
			local res = mare.plugins[action](name, opts)
			if not res.ok then
				if res.cancelled then
					error(mare.error("cancelled", res.error or "cancelled"), 0)
				end
				error(
					mare.error(
						"plugins.failed",
						res.error or ("plugins." .. action .. " failed"),
						{ log = res.log or {} }
					),
					0
				)
			end
			local out_log = res.log or {}
			if opts and opts.reload then
				local ok, reloaded = pcall(mare.reload)
				if not ok then
					error(
						mare.error(
							"plugins.failed",
							require("mare_lib").error_message(reloaded) or "reload failed",
							{ log = out_log }
						),
						0
					)
				end
				return { log = out_log, reload = reloaded }
			end
			return { log = out_log }
		end

		local plugins = {}
		function plugins.check(_, _, names)
			return { data = mare.plugins.check(names) }
		end
		function plugins.list(_, _, kind)
			local dirs = mare.plugins.dirs(kind)
			local packages = mare.plugins.list_installed({ manifest = true })
			return { data = packages, dirs = dirs, diagnostics = mare.plugins.diagnostics() }
		end
		function plugins.install(_, _, name, opts)
			return pkg_action("install", name, opts)
		end
		function plugins.update(_, _, name, opts)
			return pkg_action("update", name, opts)
		end
		function plugins.uninstall(_, _, name, opts)
			return pkg_action("uninstall", name, opts)
		end
		--- news() -> titles shown: NEWS.md sections for mare and enabled plugins whose version
		--- changed since last seen; a fresh install only records the versions
		function plugins.news(_, _)
			return get_news().news()
		end
		function plugins.choose_plugins(_, _)
			return get_choose()()
		end
		-- a mare.Service
		require("mare_lib").service(ctx, "plugins", plugins)

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("plugins", {
				description = "Manage plugins or show the runtime tree",
				auto_execute = true,
				run = function(args)
					if args == "tree" or args == "list" then
						return require("plugins.tree")(ctx)
					end
					ctx:spawn(get_panel())
				end,
			})
			ctx.commands:register("news", {
				description = "Show the update notes of mare and the enabled plugins again",
				auto_execute = true,
				run = function()
					local news = get_news()
					local all = news.all_news()
					if #all == 0 then
						ui.notify("No update notes for this version")
						return
					end
					local lines = {}
					for _, n in ipairs(all) do
						for _, l in ipairs(news.lines(n.title, n.body)) do
							lines[#lines + 1] = l
						end
					end
					mare.ui.overlay("News", ui.Lines(lines))
				end,
			})
		end)

		ctx:on("app/ready", function()
			if mare.mode ~= "tui" or mare.o.news == false then
				return
			end
			ctx:spawn(function()
				plugins.news(plugins, ctx)
			end)
		end)

		-- mare doctor: git, the lock, plugin names defined twice
		ctx:on("kernel/doctor", function(checks)
			local ok, list = pcall(mare.plugins.doctor)
			for _, c in ipairs(ok and list or { { name = "packages", ok = false, detail = tostring(list) } }) do
				table.insert(checks, c)
			end
			-- the first-open offer still due (the same check the offer makes, plugins.choose)
			local config = mare.config.plugins and mare.config.plugins.onboarding
			if (not config or config.plugins ~= false) and require("plugins.choose").due() then
				table.insert(checks, { name = "plugins", ok = true, detail = "not chosen yet (/plugins → Discover)" })
			end
		end)

		require("plugins.watch").setup(ctx, cfg)
	end,
}
