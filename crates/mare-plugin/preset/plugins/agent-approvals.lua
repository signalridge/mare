-- agent-approvals: how a sub-agent's approval request looks in the parent —
-- the dialog titled with the sub-agent, its run and task, the text of the rule it offers to
-- remember, and the line under the editor listing the requests waiting their turn.
--
-- Presentation only: the host keeps
-- the queue (one dialog at a time, in arrival order), the rule snapshot and its one-time token,
-- the elevation and the `approval/resolved` report. It asks this plugin through the bail event
-- `agent/approval-dialog {call, info, verdict, rule?}` and accepts only
-- `{choice = "allow"|"allow_session"|"allow_amend"|"deny"|"abort", reason?}` (no choice =
-- dismissed); with this plugin off, failing or answering anything else the request is
-- `unavailable` — denied, never approved. A plugin listening ahead of it (`prepend`) replaces the
-- dialog; `rule` is a copy of what the host stores, whatever the answer carries.
return {
	name = "agent-approvals",
	-- in the minimal plugin set
	tags = { "minimal" },
	-- the parent's dialog and waiting line for sub-agent approvals
	-- ui is followed, nothing is required
	apply = function(ctx)
		local dialog = require("agent-approvals.dialog")
		local line = require("agent-approvals.line")

		ctx:on("agent/approval-dialog", function(ev)
			return dialog.ask(ev)
		end)

		-- the waiting line follows the optional ui service (a child fiber on the cordis tree, where
		-- ui may start after this plugin; widget and handlers go with the service)
		ctx:inject({ "ui" }, function(ctx)
			local waiting = {}
			ctx.ui:widget("agent-approvals", {
				slot = "below-editor",
				-- after any other widget there (where the line has always been)
				order = 1000,
				render = function()
					return line.render(waiting)
				end,
			})
			ctx:on("agent/approval-queue", function(ev)
				waiting = {}
				for i, o in ipairs(type(ev) == "table" and ev.pending or {}) do
					waiting[i] = line.label_of(o)
				end
				ctx.ui:invalidate("agent-approvals")
			end)
			-- a widget's styles are resolved when it is drawn (`invalidate` draws at once): redraw
			-- once the theme is re-applied (a new terminal background, `mare.o.flavor` /
			-- `mare.o.theme`), which `theme/changed` announces, so a visible line never keeps the old
			-- colors. The event orders it, not the listeners' registration order
			ctx:on("theme/changed", function()
				if waiting[1] then
					ctx.ui:invalidate("agent-approvals")
				end
			end)
		end)
	end,
}
