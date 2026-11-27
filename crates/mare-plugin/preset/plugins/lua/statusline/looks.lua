-- statusline.looks: the layouts `mare.plugin("statusline", { layout = … })` can pick. Each is
-- data: which segments go left and right, the order they give way in as the terminal narrows
-- (`drop`: an id drops it, `id:short` shortens it first), and the look of each element. Copy one
-- into `looks` to change it; `layout_opts` overrides single elements of the chosen look.
--
-- Element values and their source (2026-11-12 survey):
--   sep "dot" " · " in MareSubtle
--            "space" two spaces
--   mode "word" bold coloured word, nothing for the safest preset
--            "capsule" mare's coloured capsule · "dim" a dim word · "off"
--   model "text" default text colour · "dim"
--            · "muted" (mare's classic bar)
--   effort "reasoning" `model thinking: high` · "suffix" `model high`
--            · "off"
--   jobs "bracket" `[2 jobs running]` in the accent · "icon" mare's `⚙ 2`
--   context "kimi" `context: 27% (27k/100k)` · "left" `73% context left`
--            · "gauge" mare's `▰▰▱▱▱▱ 27k/100k 27%` (short: `▰▰▱▱▱▱ 27%`)
--   context_color true: muted → info ≥ 60 % → warning ≥ 80 % → error ≥ 90 %
--   hints true: hint row on the left, keys from the keymap
--   flash seconds a change stays visible on the left (`effort → high`, `MCP ready`); 0 = off
--   cwd_segments directory components kept (`…/a/b`)
local M = {}

M.codex = {
	left = { "hints" },
	right = { "flags", "mcp", "model", "cwd", "branch", "context" },
	drop = { "cwd", "branch", "hints:short", "model:short", "hints" },
	sep = "dot",
	mode = "off",
	model = "dim",
	effort = "suffix",
	jobs = "icon",
	context = "left",
	context_color = false,
	hints = true,
	flash = 0,
	cwd_segments = 3,
}

M.kimi = {
	left = { "mode", "flags", "mcp", "model", "jobs", "cwd", "branch", "pr" },
	right = { "context" },
	drop = { "pr", "branch", "cwd", "context:short", "model:short" },
	sep = "space",
	mode = "word",
	model = "text",
	effort = "reasoning",
	jobs = "bracket",
	context = "kimi",
	context_color = false,
	hints = false,
	flash = 0,
	cwd_segments = 3,
}

M.classic = {
	left = { "mode", "flags", "mcp", "workflow", "jobs" },
	right = { "model", "context", "cwd", "branch", "pr", "changes", "quota", "cost", "peer_allow" },
	drop = {},
	sep = "dot",
	mode = "capsule",
	model = "muted",
	effort = "suffix",
	jobs = "icon",
	context = "gauge",
	context_color = true,
	hints = false,
	flash = 0,
	cwd_mode = "display_path",
	cwd_segments = 2,
}

-- the combined preset: hint row on the left, status on the right's collapse, mode capsule
-- first on the left, the context gauge, numbers only while they fit, model in the text colour
-- with `thinking: high`, `[2 jobs running]` badges, a short `…/a/b` directory, plus mare's
-- threshold colours and transient flashes
M.combined = {
	left = { "mode", "hints", "flash" },
	right = {
		"flags",
		"mcp",
		"jobs",
		"model",
		"cwd",
		"branch",
		"pr",
		"changes",
		"quota",
		"cost",
		"peer_allow",
		"context",
	},
	drop = {
		"pr",
		"hints:short",
		"changes",
		"quota",
		"cost",
		"cwd",
		"branch",
		"context:short",
		"mcp:short",
		"hints",
		"model:short",
	},
	sep = "dot",
	mode = "capsule",
	model = "text",
	effort = "reasoning",
	jobs = "bracket",
	context = "gauge",
	context_color = true,
	hints = true,
	flash = 3,
	cwd_segments = 2,
}

--- the look named by `cfg.layout` (a name of M or of cfg.looks, or a table), with
--- `cfg.layout_opts` laid over it; defaults to `"combined"`.
function M.resolve(cfg)
	local l = cfg.layout or "combined"
	local base = type(l) == "table" and l or (cfg.looks and cfg.looks[l]) or M[l]
	if type(base) ~= "table" then
		base = M.combined
	end
	local out = {}
	for k, v in pairs(base) do
		out[k] = v
	end
	for k, v in pairs(type(cfg.layout_opts) == "table" and cfg.layout_opts or {}) do
		out[k] = v
	end
	return out
end

return M
