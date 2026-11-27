-- ui.handler_errors: where a plugin's handler error is shown. One that
-- happens during a turn goes under that turn's prompt as a short line, every turn
-- shows "UserPromptSubmit hook error / Failed with non-blocking status code: …" under the
-- prompt each time): "⎿ plugin `x` hook `agent/start` failed: <first line>". The same handler
-- failing again within one turn (a stream listener, once per chunk) adds no second line. Outside
-- a turn (startup, a background task or timer) it is the one-line notice. No traceback on screen:
-- that stays in /plugins, `mare doctor` and the log. Constructed once by the ui plugin.
return function(deps)
	local U, push_done = deps.U, deps.push_done
	local M = {}
	local in_turn = false
	local shown = {} -- "plugin\0what" -> true, for the current turn

	function M.turn_start()
		in_turn, shown = true, {}
	end

	function M.turn_end()
		in_turn = false
	end

	function M.on_error(ev)
		ev = type(ev) == "table" and ev or { message = tostring(ev) }
		local event = tostring(ev.event or "")
		-- the kernel reports a listener's error after the emit that failed: an `agent/end` hook
		-- belongs to the turn that just ended
		local turn = in_turn or event:find("^agent/") ~= nil
		local plugin = ev.plugin and tostring(ev.plugin) or "?"
		local what = ev.what and tostring(ev.what) or ("hook `" .. event .. "`")
		local err = ev.error and tostring(ev.error) or deps.clean_error(ev.message)
		if not turn then
			U.notify("Run mare doctor — " .. deps.clean_error(ev.message) .. " — see /plugins", "error")
			return
		end
		local key = plugin .. "\0" .. what
		if shown[key] then
			return
		end
		shown[key] = true
		push_done("info", {
			node = ui.Paragraph(
				ui.Line({
					ui.Span("  ⎿  "):hl("MareToolGutter"),
					ui.Span(("plugin `%s` %s failed: %s"):format(plugin, what, err)):hl("MareError"),
				}),
				{ ui.Span("     ") }
			),
		})
	end

	return M
end
