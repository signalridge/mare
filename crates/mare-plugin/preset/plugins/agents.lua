-- agents: the /agents screen — this session's sub-agents and workflow
-- members (run → phase → member, then the background jobs) and, in a Peers tab, the other mare sessions on this machine. Enter on a
-- sub-agent opens its conversation read-only and live; Esc there comes back to the list and
-- Alt+←/→ step through the sub-agents; Ctrl+G goes to /resume (and /resume's Ctrl+G here).
-- Approvals stay in arrival order; a member waiting for one is marked `!` with its place.
--
-- Keys (named actions, so `mare.keymap.set` rebinds them and hints follow): `agents` on ← with
-- an empty input (with text ← moves the cursor; with nothing to list it declines),
-- `agents.back` / `agents.prev` / `agents.next` in the transcript layer, which decline in any
-- view this plugin didn't open.
return {
	name = "agents",
	inject = { "ui", "task" },
	-- /agents: sub-agents, workflow members, jobs and other sessions (← on an empty input)
	config_schema = {
		refresh_ms = {
			type = "integer",
			default = 1000,
			desc = "The open list refreshes in place this often (only when it changed).",
		},
		preview_min_width = {
			type = "integer",
			default = 84,
			desc = "The preview pane shows from this terminal width (side by side).",
		},
		split = {
			type = "object",
			desc = "The list's share of the wide screen (mare.ui.select).",
			fields = {
				ratio = { type = "number", default = 0.5, desc = "Share of the width the list takes." },
				min = { type = "integer", default = 80, desc = "Narrowest the list may get, in cells." },
			},
		},
	},
	apply = function(ctx, cfg)
		-- the optional services, read on each call
		local svc = require("mare_lib").followers(ctx, { "session", "jobs", "commands" })
		local store = require("agents.store")(ctx, svc)
		local peers = require("agents.peers")(svc)
		local list = require("agents.list")(ctx, cfg, store, peers, svc)

		ctx.ui:action("agents", function()
			if not list.any() then
				return false
			end
			list.open()
		end, {
			desc = "sub-agents and other sessions (/agents)",
			keys = { editor = { key = "<Left>", when = "editor_empty" } },
		})
		ctx.ui:action("agents.back", function()
			return list.back()
		end, {
			desc = "back to the agents list",
			keys = { transcript = { key = "<Esc>", priority = 10 } },
		})
		ctx.ui:action("agents.prev", function()
			return list.step(-1)
		end, {
			desc = "the previous sub-agent",
			keys = { transcript = "<M-Left>" },
		})
		ctx.ui:action("agents.next", function()
			return list.step(1)
		end, {
			desc = "the next sub-agent",
			keys = { transcript = "<M-Right>" },
		})

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("agents", {
				description = "Sub-agents, workflow members, jobs and other sessions",
				run = function()
					ctx:spawn(function()
						list.open()
					end)
				end,
			})
		end)
	end,
}
