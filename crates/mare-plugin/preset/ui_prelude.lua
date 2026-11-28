-- Component tree constructors (Yazi-style naming). Pure tables; the core converts them.
local ui = {}

local Span = {}
Span.__index = Span
local function style_of(self)
	local st = rawget(self, "style")
	if type(st) ~= "table" then
		-- a named style string becomes the base ({hl = name}) for further tweaks
		st = type(st) == "string" and { hl = st } or {}
		rawset(self, "style", st)
	end
	return st
end
--- hl(name): use a named style (MareXxx), like a Neovim highlight group
function Span:hl(name)
	rawset(self, "style", name)
	return self
end
function Span:fg(c)
	style_of(self).fg = c
	return self
end
function Span:bg(c)
	style_of(self).bg = c
	return self
end
function Span:bold()
	style_of(self).bold = true
	return self
end
function Span:dim()
	style_of(self).dim = true
	return self
end
function Span:italic()
	style_of(self).italic = true
	return self
end
function Span:underline()
	style_of(self).underline = true
	return self
end
function Span:strike()
	style_of(self).strike = true
	return self
end
function Span:reverse()
	style_of(self).reverse = true
	return self
end
--- link(url): an OSC 8 hyperlink (http/https only) where the terminal supports them
function Span:link(url)
	style_of(self).link = url
	return self
end
function Span:with_style(s)
	rawset(self, "style", s)
	return self
end

--- ui.Span(text, style?)  style: color slot string or {fg,bg,bold,dim,italic,...}
function ui.Span(text, style)
	return setmetatable({ type = "span", text = tostring(text or ""), style = style }, Span)
end

local Line = {}
Line.__index = Line
for _, k in ipairs({ "fg", "bg", "bold", "dim", "italic", "underline", "strike", "reverse", "with_style", "hl" }) do
	Line[k] = Span[k]
end
function Line:wrapped(indent)
	rawset(self, "wrap", true)
	rawset(self, "indent", indent)
	return self
end
function Line:filled(c)
	rawset(self, "fill", c)
	return self
end
function Line:push(x)
	table.insert(self.spans, x)
	return self
end

--- ui.Line { span|string, ... }
function ui.Line(spans)
	if type(spans) == "string" then
		spans = { spans }
	end
	return setmetatable({ type = "line", spans = spans or {} }, Line)
end

--- ui.Paragraph(text|Line, indent?, opts?) — word-wrapped (or hard-wrapped with `{ hard_wrap = true }`)
function ui.Paragraph(x, indent, opts)
	local l = type(x) == "table" and x or ui.Line({ x })
	if type(indent) == "table" and indent.hard_wrap ~= nil then
		opts, indent = indent, nil
	end
	return {
		type = "paragraph",
		spans = l.spans,
		style = l.style,
		indent = indent,
		hard_wrap = type(opts) == "table" and opts.hard_wrap == true or nil,
	}
end

--- ui.Lines { node, ... }
function ui.Lines(items)
	return { type = "lines", items = items or {} }
end
ui.List = ui.Lines

function ui.Markdown(text)
	return { type = "markdown", text = text or "" }
end

--- ui.image{ path = "…" | bytes = "…", max_rows = 16, max_cols = 80 }: an image —
--- the terminal's protocol (Kitty / iTerm2 / sixel) or a one-line placeholder
function ui.image(opts)
	opts = opts or {}
	return {
		type = "image",
		image_id = opts.image_id or ui.image_id(),
		path = opts.path,
		bytes = opts.bytes,
		resource = opts.resource,
		fallback = opts.fallback,
		max_rows = opts.max_rows,
		max_cols = opts.max_cols,
	}
end
--- Render cached fenced-block nodes in the original Markdown context. Source offsets are
--- returned by markdown_blocks(), and all blocks without an entry keep their source card.
function ui.PresentedMarkdown(text, blocks)
	return { type = "presented_markdown", text = text, blocks = blocks or {} }
end

function ui.Code(code, lang)
	return { type = "code", code = code or "", lang = lang or "" }
end
--- ui.Diff{old=, new=, path=} or ui.Diff{unified=}
function ui.Diff(t)
	t.type = "diff"
	return t
end
--- ui.Indent(first_prefix, child, rest_prefix?)
function ui.Indent(first, child, rest)
	return { type = "indent", first = first, child = child, rest = rest }
end
function ui.Gap(n)
	return { type = "gap", n = n or 1 }
end
function ui.Spinner(label)
	return { type = "spinner", label = label }
end
--- ui.Frames(frames, ms, still?) — the core shows frames[i] by wall-clock time, `ms` apart;
--- `still` (default the first) under reduce_motion
function ui.Frames(frames, ms, still)
	return { type = "frames", frames = frames, ms = ms, still = still }
end
--- ui.Row(items) — nodes side by side on one row (the first line of each)
function ui.Row(items)
	return { type = "row", items = items }
end
--- ui.Clip(child, max_lines, tail?) — shows "… N more lines"
function ui.Clip(child, max, tail)
	return { type = "clip", child = child, max = max, tail = tail }
end

--- ui.Block(child, {title=string|spans, footer=, border_style="MareBorder", border=}) — a
--- bordered panel with the title in the top border
function ui.Block(child, opts)
	opts = opts or {}
	return {
		type = "block",
		child = child,
		title = opts.title,
		footer = opts.footer,
		border_style = opts.border_style,
		border = opts.border,
	}
end
--- ui.List(items, {selected=1-based, focused=true, height=}) — virtually scrolled list
function ui.List(items, opts)
	opts = opts or {}
	return { type = "list", items = items, selected = opts.selected, focused = opts.focused, height = opts.height }
end
--- ui.Split(left, right, {ratio=0.4, stack_below=84}) — list + preview panes, stacked when narrow
function ui.Split(left, right, opts)
	opts = opts or {}
	return { type = "split", left = left, right = right, ratio = opts.ratio, stack_below = opts.stack_below }
end

--- Checkbox list for todos / plans: items = { {text=, done=bool, active=bool} }
function ui.Checklist(items)
	local out = {}
	for _, it in ipairs(items) do
		local box = it.done and ui.Span("☒ "):hl("MareSuccess")
			or ui.Span("☐ "):hl(it.active and "MareAccent" or "MareMuted")
		local text = ui.Span(it.text or "")
		if it.done then
			text:hl("MareMuted"):strike()
		elseif it.active then
			text:bold()
		end
		out[#out + 1] = ui.Line({ box, text })
	end
	return ui.Lines(out)
end

return ui
