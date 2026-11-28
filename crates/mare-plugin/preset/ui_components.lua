-- Default UI components (Yazi-style) and UI functions (Neovim vim.ui-style).
-- Components live here; icons, file types, the logo, segments and the gauge are in ui_shell.lua, loaded right after.
-- Installed by the core before init.lua runs, so users can override or wrap any of them:
--
--   function mare.ui.ToolCall:header(call, state)... end -- override
--   local old = mare.ui.UserMessage.render -- wrap
--   function mare.ui.UserMessage:render(text) return ui.Lines({ old(self, text), ui.Gap() }) end
--   mare.ui.status("hi", { align = "right", order = 500, render = function() return ui.Span("hi") end })
--   mare.ui.renderer("tool:bash", { compact = fn(call, result), expanded = fn(call, result) })
--   mare.ui.select = my_fuzzy_select -- like dressing.nvim
--   mare.ui.style("MareToolName", { fg = "#7aa2f7", bold = true }) -- like nvim_set_hl
--   mare.keymap.set("editor", "<C-o>", "transcript") -- like vim.keymap.set
--
-- Components are called at message start/finish and on invalidate — never per streamed token.

local mare, core = ...
local ui_core = core and core.ui
local cfg_core = core and core.config
local ui = mare.ui

-- Config Lua can yield; only registrations made by its own coroutine belong to its reload.
local function config_state()
	return cfg_core and cfg_core.state or nil
end

local function in_config()
	local st = config_state()
	return st ~= nil and st.in_config == true and st.config_coroutine == coroutine.running()
end

-- user style overrides survive theme (re)application: themes set defaults, then replay these
local style_overrides = {}
local raw_style = ui.style
function ui.style(name, spec)
	if spec ~= nil then
		style_overrides[name] = spec
	end
	return raw_style(name, spec)
end
--- theme_style(name, spec): set a theme default; unlike style() it is not recorded as an
--- explicit override, so it never wins over the user's mare.ui.style calls
ui.theme_style = raw_style
--- style_overrides() -> { name = spec }: explicit mare.ui.style calls, replayed over themes
function ui.style_overrides()
	local out = {}
	for k, v in pairs(style_overrides) do
		out[k] = v
	end
	return out
end
--- reset_style(name, spec): drop the explicit override of name and restore spec as its default
function ui.reset_style(name, spec)
	style_overrides[name] = nil
	raw_style(name, spec)
end

---------------------------------------------------------------------------
-- renderer registry: "tool:<name>", "message:user", "message:assistant", "message:reasoning"
---------------------------------------------------------------------------
-- kind -> stack of renderers
local renderer_stacks = {}
if ui_core then
	ui_core.renderer_stack_len = function(kind)
		local st = renderer_stacks[kind]
		return st and #st or 0
	end
end
--- get_renderer(kind) -> the topmost renderer registered for kind, or nil
function ui.get_renderer(kind)
	local st = renderer_stacks[kind]
	return st and st[#st] or nil
end
local function stage_registration(commit, remove)
	local st = in_config() and config_state() or nil
	if st and st.config_components then
		table.insert(st.config_components, remove)
	end
	if st and st.stage then
		table.insert(st.stage, { commit = commit })
	else
		commit()
	end
	return remove
end
local raw_comp_reg = ui.completion.register
function ui.completion.register(spec)
	local release
	local cancelled = false
	local function remove()
		cancelled = true
		if release then
			local fn = release
			release = nil
			fn()
		end
	end
	return stage_registration(function()
		if not cancelled then
			release = raw_comp_reg(spec)
		end
	end, remove)
end
--- renderer(kind, { compact = fn, expanded = fn }) -> remove()
function ui.renderer(kind, r)
	local st, cancelled
	local function commit()
		if cancelled then
			return
		end
		st = renderer_stacks[kind] or {}
		renderer_stacks[kind] = st
		table.insert(st, r)
	end
	local function remove()
		cancelled = true
		if not st then
			return
		end
		local i = table.find(st, r)
		if i then
			table.remove(st, i)
		end
	end
	return stage_registration(commit, remove)
end
--- get_entry_renderer(kind) -> { compact = fn?, expanded = fn? } | nil
function ui.get_entry_renderer(kind)
	return type(kind) == "string" and ui.get_renderer("entry:" .. kind) or nil
end
--- entry_renderer(kind, fn | { compact?, expanded?, render? }) -> remove()
function ui.entry_renderer(kind, r)
	assert(type(kind) == "string" and kind ~= "", "entry kind must be a non-empty string")
	local norm
	if type(r) == "function" then
		norm = { compact = r, expanded = r }
	elseif type(r) == "table" then
		local compact = r.compact or r.render
		local expanded = r.expanded or r.render or compact
		assert(type(compact) == "function" or type(expanded) == "function", "entry renderer requires a render function")
		norm = { compact = compact, expanded = expanded }
	else
		error("entry renderer must be a function or table", 2)
	end
	return ui.renderer("entry:" .. kind, norm)
end

local MD_SURFACES = {
	user = true,
	assistant = true,
	["assistant-reasoning"] = true,
	custom = true,
	summary = true,
	changelog = true,
}
local markdown_transforms = {}
--- markdown_transform(fn, opts?) -> remove()
function ui.markdown_transform(fn, opts)
	assert(type(fn) == "function", "markdown transformer must be a function")
	local types
	local list = opts and (opts.message_types or opts.messageTypes)
	if list ~= nil then
		assert(type(list) == "table", "markdown_transform message_types must be an array")
		types = {}
		for _, k in ipairs(list) do
			assert(MD_SURFACES[k], "unknown markdown message_type: " .. tostring(k))
			types[k] = true
		end
	else
		types = { user = true, assistant = true, ["assistant-reasoning"] = true }
	end
	local rec = { fn = fn, types = types }
	local cancelled, committed = false, false
	local function commit()
		if not cancelled then
			committed = true
			table.insert(markdown_transforms, rec)
		end
	end
	local function remove()
		cancelled = true
		if not committed then
			return
		end
		local i = table.find(markdown_transforms, rec)
		if i then
			table.remove(markdown_transforms, i)
		end
	end
	return stage_registration(commit, remove)
end
function ui.has_markdown_transforms(message_type)
	local mt = message_type or "assistant"
	for _, rec in ipairs(markdown_transforms) do
		if rec.types[mt] then
			return true
		end
	end
	return false
end
function ui.transform_markdown(text, context)
	local cur = text == nil and "" or tostring(text)
	if #markdown_transforms == 0 then
		return cur
	end
	local c = type(context) == "table" and context or {}
	local mt = c.message_type or "assistant"
	assert(MD_SURFACES[mt], "unknown markdown message_type: " .. tostring(mt))
	local arg = {
		message_type = mt,
		is_streaming = c.is_streaming == true,
		available_width = c.available_width or (ui.width and ui.width()) or 80,
		message_id = c.message_id,
		created_at_ms = c.created_at_ms,
	}
	for _, rec in ipairs(markdown_transforms) do
		if rec.types[mt] then
			local ok, out = pcall(rec.fn, cur, arg)
			if ok and type(out) == "string" then
				cur = out
			elseif not ok then
				mare.log("warn", "markdown transformer failed: " .. tostring(out))
			end
		end
	end
	return cur
end

---------------------------------------------------------------------------
-- messages
---------------------------------------------------------------------------
ui.UserMessage = {}
function ui.UserMessage:render(text)
	local r = ui.get_renderer("message:user")
	if r and r.compact then
		return r.compact(text)
	end
	-- a full-width bar in the selection color marking the user's turn with a background, the ❯
	-- prompt kept; every source line wraps with a 2-cell indent
	local ascii = ui.icon_set and ui.icon_set() == "ascii"
	local items = {}
	for line in (tostring(text) .. "\n"):gmatch("(.-)\n") do
		local lead = #items == 0 and ui.Span(ascii and "> " or "❯ "):hl("MareUserPrompt")
			or ui.Span("  "):hl("MareUserMessage")
		items[#items + 1] = ui.Line({ lead, ui.Span(line):hl("MareUserMessage") })
			:wrapped({ ui.Span("  "):hl("MareUserMessage") })
			:filled("MareSelection")
	end
	return #items == 1 and items[1] or ui.Lines(items)
end

--- The assistant's streaming text is rendered by the core (Markdown, highlighting) with
--- `marker()` as its gutter. Override `render(text)` to replace the finished message.
ui.AssistantMessage = { render = nil }
function ui.AssistantMessage:marker()
	return (ui.icon_set and ui.icon_set() == "ascii") and "* " or "● "
end

ui.Reasoning = {}
--- collapsed (scrollback) form, shown once reasoning ends
function ui.Reasoning:render(text, secs)
	local r = ui.get_renderer("message:reasoning")
	if r and r.compact then
		return r.compact(text, secs)
	end
	local mark = (ui.icon_set and ui.icon_set() == "ascii") and "o " or "◇ "
	return ui.Line({
		ui.Span(mark):hl("MareReasoning"),
		ui.Span(("Thought for %.1fs"):format(secs)):hl("MareReasoning"),
	})
end
--- expanded form (Ctrl+T)
function ui.Reasoning:expanded(text, secs)
	local r = ui.get_renderer("message:reasoning")
	if r and r.expanded then
		return r.expanded(text, secs)
	end
	local title = (ui.icon_set and ui.icon_set() == "ascii") and "o Thinking" or "◇ Thinking"
	return ui.Lines({ ui.Line({ ui.Span(title):hl("MareReasoning") }), ui.Indent("  ", ui.Markdown(text)) })
end

--- The dim line closing each turn. info = { secs, interrupted, failed, output_tokens, clock };
--- return nil to show nothing.
ui.TurnEnd = {}
function ui.TurnEnd.duration(secs)
	secs = math.max(0, math.floor(secs + 0.5))
	if secs < 60 then
		return secs .. "s"
	end
	return ("%dm %02ds"):format(secs // 60, secs % 60)
end
function ui.TurnEnd.tokens(n)
	if n >= 1e6 then
		return ("%.1fM"):format(n / 1e6)
	end
	if n >= 1000 then
		return ("%.1fk"):format(n / 1000)
	end
	return tostring(math.floor(n))
end
function ui.TurnEnd:render(info)
	local d = ui.TurnEnd.duration(info.secs or 0)
	local text
	if info.interrupted then
		text = "interrupted after " .. d
	elseif info.failed then
		text = "stopped after " .. d
	else
		local parts = { "worked " .. d }
		if (info.output_tokens or 0) > 0 then
			parts[#parts + 1] = ui.TurnEnd.tokens(info.output_tokens) .. " tokens"
		end
		parts[#parts + 1] = info.clock
		local sep = (ui.icon_set and ui.icon_set() == "ascii") and " - " or " · "
		text = table.concat(parts, sep)
	end
	return ui.Line({ ui.Span(text):hl("MareDim") })
end

ui.Notice = {}
function ui.Notice:render(text, level)
	local hl = ({ error = "MareError", warning = "MareWarning", success = "MareSuccess" })[level] or "MareMuted"
	local mark = (ui.icon_set and ui.icon_set() == "ascii") and "* " or "● "
	return ui.Paragraph(ui.Line({ ui.Span(mark):hl(hl), ui.Span(text):hl(hl) }), { ui.Span("  ") })
end

---------------------------------------------------------------------------
-- tool calls: "● Name(args)" header + "⎿ result" body
---------------------------------------------------------------------------
ui.ToolCall = { preview_lines = 2 }

--- header(summary, state) — state: "running" | "ok" | "error"
--- tools whose summary argument is a path (colored by file type)
ui.ToolCall.file_tools = { read = true, edit = true, write = true, update = true, create = true, ls = true }

--- tense of a tool's verb: present while running, past when done —
--- "Reading a.rs" → "Read a.rs". Keyed by the lower-cased tool name of the summary; a tool not
--- listed keeps its "Name(args)" form. Add or replace entries to cover other tools.
ui.ToolCall.tenses = {
	read = { "Reading", "Read" },
	write = { "Writing", "Wrote" },
	edit = { "Editing", "Edited" },
	patch = { "Patching", "Patched" },
	list = { "Listing", "Listed" },
	ls = { "Listing", "Listed" },
	grep = { "Searching", "Searched" },
	search = { "Searching", "Searched" },
	find = { "Finding", "Found" },
	glob = { "Finding", "Found" },
	fetch = { "Fetching", "Fetched" },
	web_fetch = { "Fetching", "Fetched" },
}

--- running(label) -> the head of a running call: a spinner, or (`still`, set from
--- `ui.working` / `ui.tool_call.spinner`) a steady pending dot
ui.ToolCall.still = false
ui.ToolCall.spinner = nil
function ui.ToolCall:running(label)
	local ascii = ui.icon_set and ui.icon_set() == "ascii"
	if self.still then
		local spans = { ui.Span(ascii and "* " or "● "):hl("MareToolPending") }
		for _, s in ipairs(label) do
			spans[#spans + 1] = s
		end
		return ui.Line(spans)
	end
	if type(self.spinner) == "table" and type(self.spinner.frames) == "table" and #self.spinner.frames > 0 then
		local d = self.spinner
		local ascii = ui.icon_set and ui.icon_set() == "ascii"
		if ascii and type(d.ascii) == "table" and type(d.ascii.frames) == "table" then
			d = d.ascii
		end
		local frames = {}
		for _, f in ipairs(d.frames) do
			frames[#frames + 1] = ui.Line({ ui.Span(f .. " "):hl("MareSpinner") })
		end
		local still = ui.Line({ ui.Span((d.still or d.frames[1]) .. " "):hl("MareSpinner") })
		return ui.Row({ ui.Frames(frames, d.ms or 120, still), ui.Line(label) })
	end
	return ui.Spinner(label)
end

--- verb(name, state) -> the tool's verb in the tense for `state`, or nil when it has none
function ui.ToolCall:verb(name, state)
	local t = name and self.tenses and self.tenses[string.lower(name)]
	return t and (state == "running" and t[1] or t[2]) or nil
end

function ui.ToolCall:header(summary, state, source)
	local name, rest = string.match(summary, "^([%w_%-%.:]+)(%(.*)$")
	local verb = self:verb(name, state)
	-- a tool with a verb reads as a sentence: "Reading a.rs" (no parentheses)
	local args = verb and string.match(rest, "^%((.*)%)$")
	local label
	if args then
		label = { ui.Span(verb):hl("MareToolName"), ui.Span(args == "" and "" or " " .. args):hl("MareToolArgs") }
	else
		label = name and { ui.Span(name):hl("MareToolName"), ui.Span(rest):hl("MareToolArgs") }
			or { ui.Span(summary):hl("MareToolName") }
	end
	-- file tools: the path in the file-type color
	local path = name and ui.ToolCall.file_tools[string.lower(name)] and string.match(rest, "^%(([^%s()]+)%)$")
	if path and args then
		label = { ui.Span(verb .. " "):hl("MareToolName"), ui.Span(path):hl(ui.filetype_style(path)) }
	elseif path then
		label = {
			ui.Span(name):hl("MareToolName"),
			ui.Span("("):hl("MareToolArgs"),
			ui.Span(path):hl(ui.filetype_style(path)),
			ui.Span(")"):hl("MareToolArgs"),
		}
	end
	if type(source) == "string" and source ~= "" then
		label[#label + 1] = ui.Span(" via " .. source):hl("MareDim")
	end
	if state == "running" then
		return self:running(label)
	end
	local mark = (ui.icon_set and ui.icon_set() == "ascii") and "* " or "● "
	local spans = { ui.Span(mark):hl(state == "error" and "MareToolError" or "MareToolOk") }
	for _, s in ipairs(label) do
		spans[#spans + 1] = s
	end
	return ui.Paragraph(ui.Line(spans), { ui.Span("  ") })
end

--- body(node) — hang a result under the header
function ui.ToolCall:body(node)
	local gutter = (ui.icon_set and ui.icon_set() == "ascii") and "  `- " or "  ⎿  "
	return ui.Indent({ ui.Span(gutter):hl("MareToolGutter") }, node, { ui.Span("     ") })
end

--- default result text when a tool has no renderer
function ui.ToolCall:result_text(text, is_error, expanded)
	text = string.gsub(text or "", "%s+$", "")
	if text == "" then
		text = "(no output)"
	end
	local node = ui.Paragraph(ui.Line({ ui.Span(text):hl(is_error and "MareError" or "MareToolResult") }))
	if expanded then
		return node
	end
	return ui.Clip(node, self.preview_lines or 2)
end

--- render(call, result, expanded, ctx) → full node. ctx = { summary, tool (definition) }
--- rows a compact diff keeps before "… N more lines" (nil: diffs always show in full, today)
ui.ToolCall.diff_fold = nil

local function is_diff_node(node)
	return type(node) == "table" and (node.type == "diff" or node.type == "unified")
end

local function diff_rendered_rows(node)
	local unified = node.unified or node.diff
	if type(unified) ~= "string" and node.type == "diff" and mare.text and mare.text.diff then
		unified = mare.text.diff(node.old or "", node.new or "", { path = node.path })
	end
	if type(unified) ~= "string" then
		return nil
	end
	local rows, hunks, in_hunk = 1, 0, false -- row 1 is the `+A −D` stats line
	for _, line in ipairs(mare.text.split_lines(unified)) do
		if line:match("^@@ %-%d+,?%d* %+%d+,?%d* @@") then
			if hunks > 0 then
				rows = rows + 1 -- separator between hunks
			end
			hunks = hunks + 1
			in_hunk = true
		elseif line:sub(1, 5) == "diff " or line:sub(1, 6) == "index " then
			in_hunk = false
		elseif in_hunk and line:sub(1, 1) ~= "\\" then
			rows = rows + 1
		end
	end
	return rows
end

--- has_diff(node) -> the node is, or holds, a diff (edit / write / apply_patch results)
function ui.ToolCall:has_diff(node)
	if is_diff_node(node) then
		return true
	end
	if type(node) ~= "table" then
		return false
	end
	for _, c in ipairs(node.items or node) do
		if is_diff_node(c) then
			return true
		end
	end
	return false
end

function ui.ToolCall:render(call, result, expanded, info)
	local state = result == nil and "running" or (result.is_error and "error" or "ok")
	local head = self:header(info.summary, state, info.source)
	if result == nil then
		if info.live then
			return ui.Lines({
				head,
				self:body(
					ui.Clip(
						ui.Paragraph(ui.Line({ ui.Span(info.live):hl("MareToolResult") })),
						self.preview_lines or 2,
						true
					)
				),
			})
		end
		return head
	end
	local r = ui.get_renderer("tool:" .. call.name)
	local node
	if r then
		local f = expanded and (r.expanded or r.compact) or r.compact
		if f then
			node = f(call, result)
		end
	end
	if node == nil and info.tool and info.tool.render then
		local ok, n = pcall(info.tool.render, call.arguments or {}, result, expanded)
		if ok then
			node = n
		end
	end
	-- a long diff folds like command output (`diff_fold` rows, then "… N more lines"; nil keeps
	-- today's full diff). The expanded view (ctrl+o, the Ctrl+T transcript) always shows it all.
	-- Multi-item `ui.Lines` (e.g. multi-file apply_patch) clips each diff individually so later
	-- file headers and status lines are not hidden behind the first file.
	if node ~= nil and not expanded and self.diff_fold and self:has_diff(node) then
		local fold_lines = self.diff_fold == true and 8 or self.diff_fold
		if type(fold_lines) == "number" and fold_lines > 0 then
			if node.type == "lines" and type(node.items) == "table" and #node.items > 1 then
				local diffs_left = 0
				for _, c in ipairs(node.items) do
					if is_diff_node(c) then
						diffs_left = diffs_left + 1
					end
				end
				local remaining = fold_lines
				local folded = {}
				for i, c in ipairs(node.items) do
					if is_diff_node(c) then
						-- Share `fold_lines` sequentially across diffs while reserving 1 row
						-- (`+A −D` stats) per later diff so total diff rows stay within budget
						-- and uneven small patches that fit never emit a fold hint.
						local allow = math.max(1, remaining - (diffs_left - 1))
						local rows = diff_rendered_rows(c)
						local used = rows and math.min(rows, allow) or allow
						remaining = math.max(0, remaining - used)
						diffs_left = diffs_left - 1
						folded[i] = ui.Clip(c, allow, false)
					else
						folded[i] = c
					end
				end
				node = ui.Lines(folded)
			else
				node = ui.Clip(node, fold_lines, false)
			end
		end
	end
	if node == nil then
		node = self:result_text(info.text, result.is_error, expanded)
	elseif result.is_error then
		node = ui.Lines({ self:result_text(info.text, true, expanded), node })
	end
	return ui.Lines({ head, self:body(node) })
end

---------------------------------------------------------------------------
-- status bar: single status-segment registration surface
---------------------------------------------------------------------------
local status_entries = {}
local status_seq = 0
local status_batch_depth = 0
local status_dirty = {}
local status_flush_gen = 0

local function render_status_entry(entry, flush_gen)
	local width = ui.width and ui.width() or 80
	local ok, node = pcall(entry.render, width, flush_gen)
	if ok then
		ui.set_status(entry.id, entry.align, node, entry.order, entry.keep)
	elseif mare.log then
		pcall(mare.log, "warn", "status " .. tostring(entry.id) .. ": " .. tostring(node))
	end
end

local function flush_status_entries(list)
	table.sort(list, function(a, b)
		if a.align ~= b.align then
			return a.align < b.align
		end
		if a.order ~= b.order then
			return a.order < b.order
		end
		return a.seq < b.seq
	end)
	status_flush_gen = status_flush_gen + 1
	local gen = status_flush_gen
	for _, entry in ipairs(list) do
		render_status_entry(entry, gen)
	end
end

if ui_core then
	ui_core.status_batch_begin = function()
		status_batch_depth = status_batch_depth + 1
	end
	ui_core.status_batch_end = function()
		if status_batch_depth <= 0 then
			return
		end
		status_batch_depth = status_batch_depth - 1
		if status_batch_depth > 0 then
			return
		end
		local list = {}
		for id in pairs(status_dirty) do
			local entry = status_entries[id]
			if entry then
				list[#list + 1] = entry
			end
		end
		status_dirty = {}
		if #list > 0 then
			flush_status_entries(list)
		end
	end
end

--- status(id, { align?, order?, keep?, render }) -> release
function ui.status(id, spec)
	assert(type(id) == "string" and id ~= "", "ui.status: id must be a non-empty string")
	assert(type(spec) == "table", "ui.status: spec must be a table")
	assert(
		spec.align == nil or spec.align == "left" or spec.align == "right",
		'ui.status: align must be "left" or "right"'
	)
	assert(spec.order == nil or type(spec.order) == "number", "ui.status: order must be a number")
	assert(spec.keep == nil or type(spec.keep) == "boolean", "ui.status: keep must be a boolean")
	assert(type(spec.render) == "function", "ui.status: render must be a function")

	local align = spec.align or "left"
	local order = spec.order or 1000
	local keep = spec.keep == true
	local cancelled = false
	local entry

	local function commit()
		if cancelled then
			return
		end
		local prev = status_entries[id]
		local seq = prev and prev.seq
		if not seq or prev.align ~= align or prev.order ~= order then
			status_seq = status_seq + 1
			seq = status_seq
		end
		entry = {
			id = id,
			align = align,
			order = order,
			keep = keep,
			render = spec.render,
			seq = seq,
		}
		status_entries[id] = entry
		if status_batch_depth > 0 then
			status_dirty[id] = true
		else
			render_status_entry(entry)
		end
	end

	local function remove()
		if cancelled then
			return
		end
		cancelled = true
		status_dirty[id] = nil
		if entry and status_entries[id] == entry then
			status_entries[id] = nil
			ui.set_status(id, entry.align, nil)
		end
	end

	return stage_registration(commit, remove)
end

--- status_redraw(id?): re-render one registered status segment or all of them.
function ui.status_redraw(id)
	if status_batch_depth > 0 then
		if id ~= nil then
			if status_entries[id] then
				status_dirty[id] = true
			end
		else
			for k in pairs(status_entries) do
				status_dirty[k] = true
			end
		end
		return
	end
	if id ~= nil then
		local entry = status_entries[id]
		if entry then
			render_status_entry(entry)
		end
		return
	end
	local list = {}
	for _, entry in pairs(status_entries) do
		list[#list + 1] = entry
	end
	flush_status_entries(list)
end

---------------------------------------------------------------------------
-- interface-shell pieces the core draws for its overlays: the key
-- hint line, tab titles, the select panel's frame and the confirm dialog. Lua describes, the core
-- draws: each method runs only when its input changes (an overlay opens, the selection or a tab
-- changes, a search starts) and the core caches the result; the list rows, filtering and virtual
-- scrolling stay in the core, so typing a filter never calls Lua. Override a method, or a style
-- field, like any other component:
--   function mare.ui.KeyHints:render(hints)... end
--   mare.ui.Confirm.title = "Sure?"
---------------------------------------------------------------------------

--- KeyHints: the status bar while an overlay is open (lazygit options bar).
--- render(hints) -> Line, hints = { {key=, desc=}, ... }
--- the working line: nil triggers the generic text fallback; by default the ui
--- plugin installs a renderer configured by `ui.working` (plugins/lua/ui/working.lua);
--- info = { label, detail, secs, elapsed, hints, color = "truecolor" | "256" | "16" | "none",
--- still, stopping, stop_hint } (still: reduce_motion, only still frames are drawn)
ui.Working = {}
function ui.Working:render(_info)
	return nil
end

ui.KeyHints = { lead = " ", sep = " │ ", key_style = "MareKeyHint", desc_style = "MareDim", sep_style = "MareSubtle" }
function ui.KeyHints:render(hints)
	local sep = (self.sep == " │ " and ui.icon_set and ui.icon_set() == "ascii") and " | " or self.sep
	local spans = { ui.Span(self.lead) }
	for i, h in ipairs(hints) do
		if i > 1 then
			spans[#spans + 1] = ui.Span(sep):hl(self.sep_style)
		end
		spans[#spans + 1] = ui.Span(h.key):hl(self.key_style)
		spans[#spans + 1] = ui.Span(" " .. h.desc):hl(self.desc_style)
	end
	return ui.Line(spans)
end

--- Tabs: the views of one panel as its border title (`Running │ All`, lazygit).
--- render(names, active (1-based), title?, width?) -> Line
ui.Tabs = {
	sep = " │ ",
	sep_style = "MareBorder",
	active_style = "MareTitle",
	inactive_style = "MareDim",
	title_style = "MareMuted",
}
function ui.Tabs:render(names, active, title, width)
	local ascii = ui.icon_set and ui.icon_set() == "ascii"
	local sep = (self.sep == " │ " and ascii) and " | " or self.sep
	local spans = {}
	local prefix = title and title ~= "" and (title .. "  ") or ""
	local available = width and math.max(1, width - 6) or math.huge
	local current = math.max(1, math.min(active or 1, #names))
	if width and mare.text.width(prefix) + mare.text.width(names[current] or "") > available then
		prefix = ""
	end
	if prefix ~= "" then
		spans[1] = ui.Span(prefix):hl(self.title_style)
	end
	if #names == 0 then
		return ui.Line(spans)
	end
	available = available - mare.text.width(prefix)
	local first, last = current, current
	local function required(a, b)
		local w = (a > 1 and 2 or 0) + (b < #names and 2 or 0)
		for i = a, b do
			w = w + mare.text.width(names[i]) + (i > a and mare.text.width(sep) or 0)
		end
		return w
	end
	while first > 1 or last < #names do
		if first > 1 and required(first - 1, last) <= available then
			first = first - 1
		elseif last < #names and required(first, last + 1) <= available then
			last = last + 1
		else
			break
		end
	end
	if first > 1 then
		spans[#spans + 1] = ui.Span(ascii and "< " or "‹ "):hl(self.sep_style)
	end
	for i = first, last do
		if i > first then
			spans[#spans + 1] = ui.Span(sep):hl(self.sep_style)
		end
		local room = math.max(0, available - (first > 1 and 2 or 0) - (last < #names and 2 or 0))
		local name = width and i == current and mare.text.trunc_width(names[i], room) or names[i]
		spans[#spans + 1] = ui.Span(name):hl(i == current and self.active_style or self.inactive_style)
	end
	if last < #names then
		spans[#spans + 1] = ui.Span(ascii and " >" or " ›"):hl(self.sep_style)
	end
	return ui.Line(spans)
end

--- Select: the frame of a list overlay (`mare.ui.select`, the `?` key list). The core fills in
--- the rows, the search text and the `3 of 42` count.
--- frame(s) -> { title = Line, border = style, search = Line (before the typed text),
---               cursor = Line (after it), empty = Line, footer_style = style,
---               preview_title = Line, preview_border = style }
--- s = { title, tabs = {names}, tab (1-based), searching, empty, width }
ui.Select = { search_prompt = "/ ", search_cursor = "▏", empty = "(no matches)", preview = "Preview" }
function ui.Select:frame(s)
	local title = #(s.tabs or {}) > 1 and ui.Tabs:render(s.tabs, s.tab, s.title, s.width)
		or ui.Line({ ui.Span(s.title or ""):hl("MareTitle") })
	local cursor = (self.search_cursor == "▏" and ui.icon_set and ui.icon_set() == "ascii") and "|"
		or self.search_cursor
	return {
		title = title,
		border = s.searching and "MareBorderSearch" or "MareBorderActive",
		search = ui.Line({ ui.Span(self.search_prompt):hl("MareBorderSearch") }),
		cursor = ui.Line({ ui.Span(cursor):hl("MareBorderSearch") }),
		empty = ui.Line({ ui.Span(s.empty and "(empty)" or self.empty):hl("MareDim") }),
		footer_style = "MareDim",
		preview_title = ui.Line({ ui.Span(self.preview):hl("MareMuted") }),
		preview_border = "MareBorder",
	}
end

--- Confirm: the centered yes / no dialog (Yazi confirm; permission approvals use it too).
--- render(c) -> node drawn `c.width` cells wide (normally a ui.Block);
--- c = { prompt, yes, no, selected (1 = yes, 2 = no), width }
ui.Confirm = { title = "Confirm", gap = "   " }
--- "yes" -> "[Y]es"
function ui.Confirm.label(s)
	local first = s:match("^" .. utf8.charpattern) or ""
	return first == "" and "" or ("[" .. first:upper() .. "]" .. s:sub(#first + 1))
end
function ui.Confirm:render(c)
	local function btn(label, on)
		local sp = ui.Span(" " .. self.label(label) .. " ")
		return on and sp:hl("MareSelection"):bold():reverse() or sp:hl("MareMuted")
	end
	local yes, no = btn(c.yes or "Yes", c.selected == 1), btn(c.no or "No", c.selected == 2)
	local inner = math.max(1, (c.width or 40) - 4)
	local used = mare.text.width(yes.text) + mare.text.width(self.gap) + mare.text.width(no.text)
	local pad = string.rep(" ", math.max(0, (inner - used) // 2))
	return ui.Block(
		ui.Lines({
			ui.Gap(1),
			ui.Paragraph(ui.Line({ ui.Span(c.prompt or "") })),
			ui.Gap(1),
			ui.Line({ ui.Span(pad), yes, ui.Span(self.gap), no }),
		}),
		{ title = { ui.Span(self.title):hl("MareTitle") }, border_style = "MareBorderActive" }
	)
end

---------------------------------------------------------------------------
-- editor decoration
---------------------------------------------------------------------------
ui.Editor = {
	prompt = "❯ ", -- (accent); mare.o.prompt overrides
	placeholder = "Ask mare anything… (@ files, / commands, ? keys)",
	border = "MareBorder",
}
--- push the current decoration to the core (call after changing fields)
function ui.Editor:apply()
	ui.editor_config({
		prompt = self.prompt,
		placeholder = self.placeholder,
		busy_placeholder = self.busy_placeholder,
		border = self.border,
	})
end

--- EditorChrome: per-frame input box styling/decoration.
--- render(s) -> { prompt?, prompt_style?, placeholder?, placeholder_style?, border? }
ui.EditorChrome = { prompt_style = "accent", placeholder_style = "dim" }
function ui.EditorChrome:render(s)
	return {
		prompt = s.prompt,
		prompt_style = self.prompt_style,
		placeholder = s.placeholder,
		placeholder_style = self.placeholder_style,
		border = s.border,
	}
end

--- NoticeCard: rounded notification card in the transcript / live area.
--- render(p) -> Node, p = { text, level, icon, title, style, width, is_first, max_rows }
ui.NoticeCard = {}
function ui.NoticeCard:render(p)
	local text = mare.text.sanitize(tostring(p.text or "")):gsub("\t", "    ")
	local border_w = (ui.icon_set and ui.icon_set() == "ascii") and 1 or mare.text.width("│")
	local wrapped = ui.render_text(ui.Paragraph(text), math.max(1, (p.width or 40) - (border_w * 2 + 2)))
	local lines = {}
	for line in (wrapped .. "\n"):gmatch("(.-)\n") do
		lines[#lines + 1] = line
	end
	local max_rows = math.max(1, p.max_rows or 5)
	if #lines > max_rows then
		local omitted = #lines - max_rows + 1
		while #lines >= max_rows do
			table.remove(lines)
		end
		lines[#lines + 1] = ("↓ %d more lines"):format(omitted)
	end
	local rows = {}
	for _, line in ipairs(lines) do
		rows[#rows + 1] = ui.Line(line)
	end
	return ui.Block(ui.Lines(rows), {
		title = { ui.Span((p.icon or "") .. " " .. p.title):hl(p.style) },
		border_style = p.style,
	})
end

--- CompletionPopup: completion menu popup.
--- render(p) -> Node, p = { title, items, selected, total, width, height, commands, footer }
ui.CompletionPopup = { border = "MareBorder" }
function ui.CompletionPopup:render(p)
	local rows = {}
	for _, it in ipairs(p.items or {}) do
		local spans = {}
		if it.icon then
			spans[#spans + 1] = ui.Span(it.icon .. " "):hl(it.style)
		end
		spans[#spans + 1] = ui.Span(it.label or ""):hl(it.style)
		if it.hint then
			spans[#spans + 1] = ui.Span(" " .. it.hint):hl("MareHint")
		end
		if it.detail then
			spans[#spans + 1] = ui.Span("  " .. it.detail):hl("MareMuted")
		end
		rows[#rows + 1] = ui.Line(spans)
	end
	return ui.Block(ui.List(rows, { selected = p.selected, focused = true, height = p.height }), {
		title = { ui.Span(p.title):hl("MareTitle") },
		footer = p.footer,
		border_style = self.border,
	})
end

--- WhichKey: prefix continuation popup.
--- render(p) -> Node, p = { prefix, bindings = { {key, action}, ... }, width }
ui.WhichKey = {
	border = "MareBorderActive",
	title_style = "MareTitle",
	key_style = "MareKeyHint",
	action_style = "MareMuted",
}
function ui.WhichKey:render(p)
	local rows = {}
	local bindings = p.bindings or {}
	local key_width, action_width = 0, 0
	for _, b in ipairs(bindings) do
		key_width = math.max(key_width, mare.text.width(b.key or ""))
		action_width = math.max(action_width, mare.text.width(b.action or ""))
	end
	local cell = key_width + 2 + action_width
	local available = math.max(1, (p.width or 40) - 4)
	local columns = math.max(1, math.min(#bindings, (available + 2) // (cell + 2)))
	for first = 1, #bindings, columns do
		local spans = {}
		for i = first, math.min(first + columns - 1, #bindings) do
			local b = bindings[i]
			local key, action = b.key or "", b.action or ""
			spans[#spans + 1] = ui.Span(key .. string.rep(" ", key_width - mare.text.width(key) + 2)):hl(self.key_style)
			spans[#spans + 1] = ui.Span(action):hl(self.action_style)
			if i < math.min(first + columns - 1, #bindings) then
				spans[#spans + 1] = ui.Span(string.rep(" ", action_width - mare.text.width(action) + 2))
			end
		end
		rows[#rows + 1] = ui.Line(spans)
	end
	return ui.Block(ui.Lines(rows), {
		title = { ui.Span("+" .. (p.prefix or "")):hl(self.title_style) },
		footer = p.footer,
		border_style = self.border,
	})
end

--- HelpModal: keymap help modal frame.
ui.HelpModal = {}
function ui.HelpModal:frame(s)
	return ui.Select:frame(s)
end

--- InfoModal: read-only overlay modal frame.
ui.InfoModal = { border = "MareBorderActive", title_style = "MareTitle", footer_style = "MareDim" }
function ui.InfoModal:frame(s)
	return {
		title = ui.Line({ ui.Span(s.title or ""):hl(self.title_style) }),
		border = self.border,
		footer_style = self.footer_style,
	}
end

--- InputModal: single-line / prompt input modal frame.
ui.InputModal = { border = "MareBorderActive", title_style = "MareTitle" }
function ui.InputModal:frame(s)
	return {
		title = ui.Line({ ui.Span(s.title or ""):hl(self.title_style) }),
		border = self.border,
	}
end

--- SelectRow: single item row inside Select/Help lists.
--- render(item) -> Node|nil, item = { label, detail?, header, selected, filter, index }
ui.SelectRow = { header_style = "MareDim", detail_style = "MareMuted" }
function ui.SelectRow:render(item)
	if item.header then
		return ui.Line({ ui.Span(item.label or ""):hl(self.header_style) })
	end
	local spans = { ui.Span(item.label or "") }
	if item.detail and item.detail ~= "" then
		spans[#spans + 1] = ui.Span("  ")
		spans[#spans + 1] = ui.Span(item.detail):hl(self.detail_style)
	end
	return ui.Line(spans)
end

-- Invalidate Rust component caches whenever a user/plugin mutates a cached UI component table.
if ui_core and ui_core.component_changed then
	local bump = ui_core.component_changed
	local function watch_component(backing)
		return setmetatable({}, {
			__index = backing,
			__newindex = function(_, k, v)
				backing[k] = v
				bump()
			end,
			__pairs = function()
				return pairs(backing)
			end,
			__len = function()
				return #backing
			end,
		})
	end
	for _, name in ipairs({
		"Working",
		"KeyHints",
		"Select",
		"Confirm",
		"EditorChrome",
		"NoticeCard",
		"CompletionPopup",
		"WhichKey",
		"HelpModal",
		"InfoModal",
		"InputModal",
		"SelectRow",
	}) do
		if type(rawget(ui, name)) == "table" then
			rawset(ui, name, watch_component(rawget(ui, name)))
		end
	end
end

---------------------------------------------------------------------------
-- replaceable UI functions (vim.ui-style). select/input/notify are core functions on this
-- table; confirm is built on select. Replace any of them and every caller uses yours.
---------------------------------------------------------------------------
--- confirm(prompt, default?, opts?) -> boolean
function ui.confirm(prompt, default, opts)
	-- centered dialog with [Y]es [N]o (Yazi confirm); y / n answer directly
	if type(prompt) == "table" then
		opts = prompt
		prompt = opts.prompt or opts.title or ""
		default = opts.default
	end
	local i, err = ui.select({
		title = prompt,
		kind = "confirm",
		items = { "Yes", "No" },
		selected = default == false and 2 or 1,
		cancel = opts and opts.cancel or nil,
	})
	if i == nil then
		return nil, err or "cancelled"
	end
	return i == 1
end

-- "waiting for the user" around select/input/confirm, whoever implements them: the functions
-- live behind a metatable so replacements (init.lua, RPC forwarding) are wrapped too. The host
-- installs core.ui.wait_hook, which emits the public `ui/wait-start` / `ui/wait-end` events
-- ({kind, title, id}); nested calls (confirm → select) in one coroutine report only the
-- outermost one.
do
	local WAIT = { select = true, input = true, confirm = true, editor = true, questions = true, approve = true }
	local INTERACTION = { select = true, input = true, confirm = true, editor = true, questions = true }
	local OVERRIDABLE = { select = true, input = true, confirm = true, notify = true }
	local MANAGED = {}
	for k in pairs(WAIT) do
		MANAGED[k] = true
	end
	for k in pairs(OVERRIDABLE) do
		MANAGED[k] = true
	end
	local impl, cache = {}, {}
	local override_stacks = { select = {}, input = {}, confirm = {}, notify = {} }
	local wrapped = setmetatable({}, { __mode = "k" }) -- wrapper -> implementation
	-- nesting is per coroutine: two plugins asking at once are two waits, each ending its own
	-- (a shared counter folded them into one, and `ui/wait-end` closed the wrong one)
	local depth = setmetatable({}, { __mode = "k" }) -- coroutine -> nesting depth
	local in_base = setmetatable({}, { __mode = "k" }) -- coroutine -> { [kind] = depth }
	local next_id = 0
	local function shallow_copy(t)
		local out = {}
		for k, v in pairs(t) do
			out[k] = v
		end
		return out
	end
	local function normalize_opts(kind, a1, a2, a3)
		if kind == "confirm" then
			if type(a1) == "table" then
				local opts = shallow_copy(a1)
				if opts.prompt == nil and opts.title ~= nil then
					opts.prompt = opts.title
				end
				if opts.title == nil and opts.prompt ~= nil then
					opts.title = opts.prompt
				end
				return opts
			end
			local opts = type(a3) == "table" and shallow_copy(a3) or {}
			opts.prompt = a1
			if opts.title == nil then
				opts.title = a1
			end
			if a2 ~= nil then
				opts.default = a2
			end
			return opts
		elseif kind == "notify" then
			if type(a1) == "table" then
				return shallow_copy(a1)
			end
			return { text = a1, level = a2 }
		end
		return type(a1) == "table" and a1 or {}
	end
	local function call_raw_base(kind, base_fn, ...)
		if not base_fn then
			return nil, "unavailable"
		end
		local co = coroutine.running()
		local t = in_base[co]
		if not t then
			t = {}
			in_base[co] = t
		end
		t[kind] = (t[kind] or 0) + 1
		local _guard <close> = setmetatable({}, {
			__close = function()
				local n = t[kind] - 1
				t[kind] = n > 0 and n or nil
			end,
		})
		return base_fn(...)
	end
	local function call_base(kind, base_fn, cur_opts)
		if kind == "notify" then
			if type(cur_opts) == "table" then
				return call_raw_base(kind, base_fn, cur_opts.text, cur_opts.level)
			end
			return call_raw_base(kind, base_fn, cur_opts)
		end
		return call_raw_base(kind, base_fn, cur_opts)
	end
	local function dispatch_override(kind, base_fn, ...)
		local co = coroutine.running()
		local st = override_stacks[kind]
		if not st or #st == 0 or (in_base[co] and (in_base[co][kind] or 0) > 0) then
			return call_raw_base(kind, base_fn, ...)
		end
		-- Snapshot active entries so mid-call release (including self-release) steps cleanly.
		local chain = {}
		for i = 1, #st do
			chain[i] = st[i]
		end
		local cur_opts = normalize_opts(kind, ...)
		local function invoke(idx, opts_in)
			while idx >= 1 and chain[idx].released do
				idx = idx - 1
			end
			if idx < 1 then
				return call_base(kind, base_fn, opts_in)
			end
			local entry = chain[idx]
			local before_prompt = kind == "confirm" and type(opts_in) == "table" and opts_in.prompt or nil
			local before_title = kind == "confirm" and type(opts_in) == "table" and opts_in.title or nil
			local function next_fn(...)
				local next_opts = select("#", ...) == 0 and opts_in or normalize_opts(kind, ...)
				if kind == "confirm" and type(next_opts) == "table" then
					if
						next_opts.prompt == before_prompt
						and next_opts.title ~= before_title
						and next_opts.title ~= nil
					then
						next_opts.prompt = next_opts.title
					elseif
						next_opts.title == before_title
						and next_opts.prompt ~= before_prompt
						and next_opts.prompt ~= nil
					then
						next_opts.title = next_opts.prompt
					end
				end
				return invoke(idx - 1, next_opts)
			end
			return entry.fn(opts_in, next_fn)
		end
		return invoke(#chain, cur_opts)
	end
	--- override(name, fn(opts, next)) -> release(): wrap or replace select/input/confirm/notify.
	function ui.override(name, fn)
		assert(
			type(name) == "string" and OVERRIDABLE[name],
			"ui.override: expected one of select, input, confirm, notify"
		)
		assert(type(fn) == "function", "ui.override: fn must be a function")
		local entry = { fn = fn, released = false }
		local committed = false
		local function commit()
			if not entry.released then
				committed = true
				table.insert(override_stacks[name], entry)
			end
		end
		local function release()
			if entry.released then
				return
			end
			entry.released = true
			if not committed then
				return
			end
			local st = override_stacks[name]
			local i = table.find(st, entry)
			if i then
				table.remove(st, i)
			end
		end
		return stage_registration(commit, release)
	end
	local function hook(phase, info, interaction)
		local h = ui_core and ui_core.wait_hook
		if h then
			pcall(h, phase, info, interaction)
		end
	end
	local function wrap(kind, f, underlying)
		local w = function(...)
			local a = ...
			local title = type(a) == "table" and (a.wait_title or a.title or a.prompt) or a
			-- callers can name what they wait for (`wait_kind = "approval"`, "sudo", …)
			local k = type(a) == "table" and a.wait_kind or kind
			local co = coroutine.running()
			local d = (depth[co] or 0) + 1
			depth[co] = d
			local info, int_req, outcome
			local closed = false
			local _guard <close> = setmetatable({}, {
				__close = function()
					if closed then
						return
					end
					closed = true
					depth[co] = d > 1 and d - 1 or nil
					if info then
						-- Synchronously release the host wait counter so UiWaits::until_idle()
						-- never stalls; the async `ui/wait-end` event is best-effort dispatched
						-- in a per-call spawned task (preserving caller context/authority,
						-- without a global queue). If the caller context tears down first,
						-- `ui/wait-end` may still truncate, while the host count is always freed.
						local close_fn = ui_core and ui_core.wait_close
						if close_fn then
							pcall(close_fn)
						end
						local phase = close_fn and "end_emit" or "end"
						local int_res = int_req
							and {
								interaction_id = int_req.interaction_id,
								kind = int_req.kind,
								title = int_req.title,
								session_id = int_req.session_id,
								outcome = outcome or "cancelled",
							}
						if mare.task and mare.task.spawn then
							local done_tx, done_rx
							if coroutine.isyieldable() then
								done_tx, done_rx = mare.task.channel()
							end
							mare.task.spawn(function()
								hook(phase, info, int_res)
								if done_tx then
									done_tx:send(true)
								end
							end)
							if done_rx then
								pcall(done_rx.recv, done_rx)
							end
						else
							hook(phase, info, int_res)
						end
					end
				end,
			})
			if d == 1 then
				next_id = next_id + 1
				-- `wait_id` pairs the completion with its start (status keeps a stack of open waits)
				local title_str = title ~= nil and tostring(title) or nil
				info = { kind = k, title = title_str, wait_id = next_id }
				if INTERACTION[kind] then
					int_req = { interaction_id = "int-" .. next_id, kind = kind, title = title_str }
				end
				hook("start", info, int_req)
			end
			if int_req then
				local res = table.pack(f(...))
				if res[1] ~= nil then
					if kind == "questions" and type(res[1]) == "table" and res[1].dismissed == true then
						outcome = "cancelled"
					else
						outcome = "answered"
					end
				elseif res[2] == "unavailable" then
					outcome = "unavailable"
				else
					outcome = "cancelled"
				end
				return table.unpack(res, 1, res.n)
			end
			return f(...)
		end
		wrapped[w] = underlying or f
		return w
	end
	for k in pairs(MANAGED) do
		impl[k] = rawget(ui, k)
		rawset(ui, k, nil)
	end
	do
		local orig_print = rawget(ui, "print")
		if orig_print then
			ui.print = function(node)
				if mare.task and mare.task.spawn and ui.render_text then
					local ok, text = pcall(ui.render_text, node)
					if ok and type(text) == "string" then
						mare.task.spawn(function()
							hook("print", text)
						end)
					end
				end
				return orig_print(node)
			end
		end
	end
	setmetatable(ui, {
		__index = function(_, k)
			if MANAGED[k] and impl[k] then
				if not cache[k] then
					local cur_base = impl[k]
					local target = OVERRIDABLE[k]
							and function(...)
								return dispatch_override(k, cur_base, ...)
							end
						or cur_base
					if WAIT[k] then
						cache[k] = wrap(k, target, cur_base)
					else
						wrapped[target] = cur_base
						cache[k] = target
					end
				end
				return cache[k]
			end
		end,
		__newindex = function(t, k, v)
			if MANAGED[k] then
				-- restoring a value read earlier hands back our own wrapper: unwrap it
				impl[k] = wrapped[v] or v
				cache[k] = nil
			else
				rawset(t, k, v)
			end
		end,
		__pairs = function(t)
			local extra = impl.notify and rawget(t, "notify") == nil and { notify = t.notify } or {}
			local phase = 0
			return function(_, prev)
				if phase == 0 then
					local k, v = next(t, prev)
					if k ~= nil then
						return k, v
					end
					phase = 1
					prev = nil
				end
				return next(extra, prev)
			end
		end,
	})
end
