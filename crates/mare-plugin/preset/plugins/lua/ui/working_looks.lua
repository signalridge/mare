-- ui.working_looks: the working line's elements and named presets as data, configured via `mare.o.ui.working`.
--
-- Elements:
--   preset  combined (default) | mare | spinner | codex | cc | kimi | shimmer | pulse | dots | wave | foam | sail | sea | <custom>
--   glyph   swell (default) | ember | braille | moon | phase | wave | sail | bullet | pulse | none
--   label   accent | shimmer | glint | foam | dots
--   elapsed auto | seconds | off
--   tokens  true | false
--   interrupt_hint true | false
--   sep     dot | bullet
--   tools   still | spin
local M = {}

M.defaults = {
	preset = "combined",
	glyph = "swell",
	label = "glint",
	elapsed = "auto",
	tokens = true,
	interrupt_hint = true,
	tips = true,
	sep = "dot",
	tools = "still",
	text_throttle_ms = 250,
}

M.presets = {
	combined = { glyph = "swell", label = "glint", elapsed = "auto", sep = "dot", tools = "still" },
	mare = { glyph = "swell", label = "foam", elapsed = "auto", sep = "dot" },
	spinner = { glyph = "braille", label = "accent", elapsed = "seconds", sep = "dot" },
	codex = { glyph = "bullet", label = "shimmer", tokens = false, sep = "bullet" },
	cc = { glyph = "ember", label = "glint", interrupt_hint = false, tools = "still" },
	kimi = { glyph = "moon", label = "accent", elapsed = "off", tokens = false, interrupt_hint = false },
	shimmer = { glyph = "bullet", label = "shimmer" },
	pulse = { glyph = "pulse", label = "accent" },
	dots = { glyph = "none", label = "dots" },
	wave = { glyph = "wave", label = "accent", tools = "still" },
	foam = { glyph = "none", label = "foam", tools = "still" },
	sail = { glyph = "sail", label = "accent", tools = "still" },
	sea = { glyph = "wave", label = "foam", tools = "still" },
}

--- glyph animations as data: `frames` `ms` apart, `still` under reduce_motion,
--- `ascii` for icons = "ascii"; the cells shade accent → accent2. Block elements
--- (▁▂▃▅) and ▲▴ are in every common monospace font; under ambiwidth = "double" they take two
--- cells like the other ambiguous-width glyphs.
M.frames = {
	braille = {
		ms = 80,
		frames = { "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏" },
		still = "•",
		flat = true,
		ascii = { ms = 80, frames = { "|", "/", "-", "\\" }, still = "*", flat = true },
	},
	ember = {
		ms = 120,
		frames = { "·", "∙", "•", "●", "◉", "●", "•", "∙" },
		still = "●",
		flat = true,
		ascii = { ms = 120, frames = { ".", "o", "O", "@", "O", "o" }, still = "O", flat = true },
	},
	moon = {
		ms = 120,
		frames = { "🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘" },
		still = "🌕",
		flat = true,
		ascii = { ms = 120, frames = { ".", "o", "O", "@", "O", "o" }, still = "@", flat = true },
	},
	phase = {
		ms = 120,
		frames = { "◐", "◓", "◑", "◒" },
		still = "◐",
		flat = true,
		ascii = { ms = 120, frames = { "|", "/", "-", "\\" }, still = "|", flat = true },
	},
	-- a swell rolling right to left (▁▂▃▅▃▂ repeated, a 4-cell window)
	swell = {
		ms = 120,
		frames = { "▁▂▃▅", "▂▃▅▃", "▃▅▃▂", "▅▃▂▁", "▃▂▁▂", "▂▁▂▃" },
		still = "▂▃▅▃",
		ascii = { ms = 120, frames = { "_.-^", ".-^-", "-^-.", "^-._", "-._.", "._.-" }, still = ".-^-" },
	},
	-- a sail bobbing on the waterline (big / small = up / down) while the water moves
	sail = {
		ms = 240,
		frames = { "~▲-", "-▲~", "~▴-", "-▴~" },
		still = "~▲~",
		ascii = { ms = 240, frames = { "~A-", "-A~", "~^-", "-^~" }, still = "~A~" },
	},
}
M.frames.wave = M.frames.swell

--- label motions as data: a sweep is a band of `peak` over `base` (`half` cells wide, crossing
--- in `run` seconds, `pad` cells before and after, then `rest` seconds still); `shape` = "cos"
--- or "crest" (mare's: steep front, exponential wash behind)
M.sweeps = {
	shimmer = {
		base = "MareMuted",
		peak = "MareSpinner",
		shape = "cos",
		half = 5,
		run = 2.0,
		pad = 10,
		rest = 0,
		tick = 80,
	},
	glint = {
		base = "MareSpinner",
		peak = "MareSpinnerGlint",
		shape = "cos",
		half = 2,
		run = 1.6,
		pad = 4,
		rest = 0,
		tick = 80,
	},
	foam = {
		base = "MareSpinner",
		peak = "MareSpinnerGlint",
		shape = "crest",
		half = 3,
		run = 1.8,
		pad = 3,
		rest = 0.9,
		tick = 90,
	},
}

return M
