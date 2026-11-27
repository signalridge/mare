-- jobs.view: the running-jobs widget and the /jobs output view; part of the `jobs` plugin (plugins/jobs.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local status_text, elapsed = D.format.status_text, D.format.elapsed
	local service = D.registry.service

	-- ---- UI ------------------------------------------------------------------------------

	local function running_jobs()
		local out = {}
		for _, j in ipairs(D.registry.raw_list()) do
			if j.status == "running" and j.background then
				out[#out + 1] = j
			end
		end
		return out
	end

	local view_job -- (job) live output view (/jobs)
	-- the widget lives while the ui service is up (a child fiber on the cordis tree)
	if cfg.widget then
		ctx:inject({ "ui" }, function(c)
			local ui_svc = c.ui
			ui_svc:widget("jobs", {
				slot = "above-editor",
				order = -20,
				render = function(width)
					local list = running_jobs()
					if #list == 0 then
						return nil
					end
					local names = {}
					for _, j in ipairs(list) do
						names[#names + 1] = ("%s (%s)"):format(j.description or j.job_id, elapsed(j))
					end
					return ui.Line({
						ui.Span(("  ⏵ %d background job%s: "):format(#list, #list == 1 and "" or "s"))
							:hl("MareAccent"),
						ui.Span(mare.text.trunc_width(table.concat(names, " · "), math.max(10, width - 30)))
							:hl("MareMuted"),
						ui.Span("  /jobs"):hl("MareDim"),
					})
				end,
			})
			local ticking = false
			local function refresh()
				ui_svc:invalidate("jobs")
				-- elapsed times tick only while something runs in the background
				if not ticking and #running_jobs() > 0 then
					ticking = true
					ctx:spawn(function()
						while #running_jobs() > 0 do
							mare.task.sleep(1000)
							ui_svc:invalidate("jobs")
						end
						ticking = false
					end)
				end
			end
			c:on("jobs/changed", refresh)
		end)
	end

	--- live view of one job's output (non-consuming tail): the transcript view refreshes in
	--- place while it stays open, with the status, elapsed time and kill reason on top
	function view_job(j)
		local title = "Job " .. j.job_id
		local function node()
			local head = ("%s — %s · %s"):format(j.description or j.kind, status_text(j), elapsed(j))
			---@type mare.Node[]
			local lines = { ui.Line({ ui.Span(head):hl("MareAccent") }) }
			if j.kill_reason then
				lines[#lines + 1] = ui.Line({ ui.Span("stopped: " .. j.kill_reason):hl("MareWarning") })
			end
			local out = mare.text.sanitize(j.handle:tail(cfg.view_tail_bytes) or "")
			lines[#lines + 1] = ui.Paragraph(ui.Line({ ui.Span(out ~= "" and out or "(no output)"):hl("MareMuted") }))
			return ui.Lines(lines)
		end
		ui.transcript(title, node())
		ctx:spawn(function()
			local last
			while true do
				mare.task.sleep(cfg.view_refresh_ms)
				if not (ui.transcript_title and ui.transcript_title() == title) then
					return
				end
				-- redraw only on change: new output, a status change, or the elapsed time
				local key = table.concat({ status_text(j), elapsed(j), tostring(j.handle:status().bytes or 0) }, "|")
				if key ~= last then
					last = key
					ui.transcript(title, node())
				end
			end
		end)
	end

	-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
	ctx:inject({ "commands" }, function(ctx)
		ctx.commands:register("jobs", {
			description = "Background jobs: view output or stop one",
			run = function()
				local list = D.registry.raw_list()
				if #list == 0 then
					ui.notify("No background jobs")
					return
				end
				local items = {}
				for i, j in ipairs(list) do
					items[i] = {
						label = ("%s  %s"):format(j.description or j.kind, status_text(j)),
						detail = ("%s · %s%s"):format(
							j.job_id,
							elapsed(j),
							j.kill_reason and (" · stopped: " .. j.kill_reason) or ""
						),
					}
				end
				local i = ui.select({ title = "Background jobs", items = items })
				if not i then
					return
				end
				local j = list[i]
				local actions = { "View output" }
				if j.status == "running" then
					actions[#actions + 1] = "Stop"
				end
				local a = ui.select({ title = j.job_id, items = actions })
				if a == 1 then
					view_job(j)
				elseif a == 2 then
					-- stopped by the user: the model hears about it ([stopped: ...] / notification)
					service.cancel(service, ctx, j.job_id, "stopped by the user", { silent = false })
				end
			end,
		})
	end)
end
