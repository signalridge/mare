-- ui: default Lua-side UI. Turns agent events into transcript blocks using the overridable
-- components in mare.ui (UserMessage, AssistantMessage, Reasoning, ToolCall, Notice, Status,
-- Editor), provides the `ui` service (widgets, status segments, keys — cleaned up on dispose)
-- and routes editor input. The core decides *how* to draw; this plugin decides *what*.
--
-- Components are looked up at call time, so user overrides in init.lua always apply. They run
-- at message start/finish and on invalidate; streamed text is drawn by the core.

return {
	name = "ui",
	-- in the minimal plugin set
	tags = { "minimal" },
	provide = { "ui" },
	-- `task` gives ctx:spawn and `bus` the mare events; every other service is read
	-- with ctx:get (optional)
	inject = { "task", "bus" },
	config_schema = {
		preview_lines = {
			type = "integer",
			default = 2,
			desc = "Compact view (Ctrl+O toggles): lines of command / output shown per tool call.",
		},
		-- a compact edit / write / apply_patch diff keeps this many rows, then "… N more lines"
		-- (default 8; false = show full diff). Ctrl+O / Ctrl+T show all.
		diff_fold_lines = {
			type = { "integer", "boolean" },
			default = 8,
			desc = "Rows a compact diff keeps before folding the rest (false = show full diff).",
		},
		group_lines = {
			type = "integer",
			default = 4,
			desc = 'Merged lines an exploration group keeps; older ones become "N earlier".',
		},
		tool_update_interval_ms = { type = "integer", default = 100, desc = "Refresh of live tool output." },
		double_esc_ms = { type = "integer", default = 600, desc = "Esc Esc window (the /undo picker when idle)." },
		widget_area_ratio = {
			type = "number",
			default = 0.5,
			desc = "Share of the terminal height the widget area may use, minus the input box and status bar.",
		},
		widget_reserved_rows = { type = "integer", default = 4, desc = "Rows kept free of widgets." },
	},
	apply = function(ctx, cfg)
		local U = mare.ui
		U.ToolCall.preview_lines = cfg.preview_lines
		U.ToolCall.diff_fold = cfg.diff_fold_lines
		-- structural submodules (plugins/lua/ui/), loaded here in dependency order
		local D = { ctx = ctx, cfg = cfg, U = U }
		D.service = require("ui.service")(D)

		if not U.connected() then
			return
		end
		-- prompt symbol: ❯ in accent by default, mare.o.prompt overrides
		local function apply_prompt()
			local p = (mare.o or mare.config or {}).prompt
			if type(p) == "string" and p ~= "" then
				U.Editor.prompt = p:sub(-1) == " " and p or (p .. " ")
			end
			U.Editor:apply()
		end
		apply_prompt()
		ctx:on("option/changed", function(ev)
			if ev.name == "prompt" then
				apply_prompt()
			end
		end)

		-- Transcript rendering (`ui.classify`, `ui.tool_nodes`, `ui.transcript`) loads lazily on
		-- the first turn, session replay, or notice event so fresh startup renders the first frame
		-- without loading unused transcript modules.
		local lib = require("mare_lib")
		D.lazy_dispatch = true
		local ensure_transcript = lib.lazy(function()
			D.classify = require("ui.classify")
			D.tool_nodes = require("ui.tool_nodes")(D)
			D.transcript = require("ui.transcript")(D)
			return D.transcript
		end)
		for _, field in ipairs({ "classify", "tool_nodes", "transcript" }) do
			D[field] = setmetatable({}, {
				__index = function(_, k)
					ensure_transcript()
					return D[field][k]
				end,
			})
		end
		for _, ev_name in ipairs({
			"turn/started",
			"step/started",
			"session/recorded",
			"item/completed",
			"item/delta",
			"ui/presenter-changed",
			"pipeline/tool-dispatching",
			"turn/completed",
			"step/completed",
			"subagent/started",
			"subagent/completed",
			"agent/child-approval",
			"agent/approval-queue",
			"pipeline/approval-decided",
			"host/notice",
			"kernel/error",
			"kernel/stalled",
			"session/loaded",
			"session/new",
		}) do
			ctx:on(ev_name, function(ev)
				return ensure_transcript().dispatch(ev_name, ev)
			end)
		end
		require("ui.input")(D)
	end,
}
