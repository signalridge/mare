-- context.report: what /context and context:report() show; part of the `context` plugin
-- (plugins/context.lua), loaded on the first report (the command is registered by its apply).
return function(D)
	local ctx, context = D.ctx, D.context

	-- /context: where the context's tokens go, by source,
	-- with the compaction summary on its own row, the space left in the window and a warning
	-- when the context no longer fits; `/context all` adds the per-item detail (instruction
	-- files, skills, tool definitions, the largest messages). The numbers are compact's
	-- measure({by_source}) — the same estimator as the compaction trigger.
	local function svc(name)
		return D.svc[name]
	end
	local function tk(x)
		return mare.text.tokens(x)
	end
	local function fmt_k(n)
		if n >= 1e6 then
			return ("%.1fM"):format(n / 1e6)
		end
		return n >= 1000 and ("%.1fk"):format(n / 1000) or tostring(n)
	end
	local function compute_report(args)
		local all = args == "all"
		local K, A, L = svc("compact"), svc("agent"), svc("llm")
		local okm, model = pcall(function()
			return L and L:current()
		end)
		model = okm and model or nil
		local window = model and model.context_window or nil
		local msgs = A and A:list_messages().data or {}
		local b
		if K then
			b = K:measure({ by_source = true })
		else
			-- no compaction plugin: the same pieces, estimated here
			b = { system = 0, instructions = 0, skills = 0, tools = 0, messages = 0, tool_results = 0, images = 0 }
			for _, f in ipairs(context.files(context, ctx)) do
				b.instructions = b.instructions + tk(f.content or "")
			end
			for _, m in ipairs(msgs) do
				local key = m.role == "tool_result" and "tool_results" or "messages"
				b[key] = b[key] + tk(m.content or {})
			end
			b.total = b.instructions + b.messages + b.tool_results
		end
		-- the compaction summary (and a provider's opaque compacted items) out of "messages"
		local summary = 0
		for _, m in ipairs(msgs) do
			if m.compaction then
				summary = summary
					+ tk(
						type(m.remote) == "table" and type(m.remote.items) == "table" and m.remote.items
							or m.content
							or {}
					)
			end
		end
		summary = math.min(summary, b.messages or 0)
		b.summary = summary
		b.conversation = (b.messages or 0) - summary
		b.window = window
		b.free = window and math.max(0, window - b.total) or nil
		b.over = window and b.total > window and (b.total - window) or nil
		local rows = {
			{ "System prompt", b.system },
			{ "Instructions (AGENTS.md)", b.instructions },
			{ "Skills", b.skills },
			{ "Tool definitions", b.tools },
			{ "Compaction summary", b.summary },
			{ "Messages", b.conversation },
			{ "Tool results", b.tool_results },
			{ "Images (estimated)", b.images },
		}
		local detail
		if all then
			local files = {}
			for _, f in ipairs(context.files(context, ctx)) do
				files[#files + 1] = {
					ui.display_path(f.path, 40) .. (f.omitted and " (over budget)" or ""),
					tk(f.content or ""),
				}
			end
			local skills = {}
			for _, sk in ipairs(context.skills(context, ctx)) do
				skills[#skills + 1] = { sk.name, tk((sk.name or "") .. ": " .. (sk.description or "")) }
			end
			local tools
			local T = svc("tools")
			if T then
				tools = {}
				local okd, defs = pcall(T.definitions, T, model)
				for _, d in ipairs(okd and defs or {}) do
					tools[#tools + 1] = { d.name or "?", tk(mare.json.encode(d)) }
				end
				table.sort(tools, function(x, y)
					return x[2] > y[2]
				end)
			end
			local big = {}
			for i, m in ipairs(msgs) do
				local preview = ""
				for _, c in ipairs(m.content or {}) do
					if c.type == "text" and c.text then
						preview = c.text
						break
					elseif c.type == "tool_call" then
						preview = "call " .. tostring(c.name)
						break
					end
				end
				local who = m.compaction and "summary" or m.role
				big[#big + 1] = {
					("#%d %s: %s"):format(i, who, (string.gsub(preview, "%s+", " "))),
					tk(
						type(m.remote) == "table" and type(m.remote.items) == "table" and m.remote.items
							or m.content
							or {}
					),
				}
			end
			table.sort(big, function(x, y)
				return x[2] > y[2]
			end)
			local largest = {}
			for i = 1, math.min(10, #big) do
				largest[#largest + 1] = big[i]
			end
			detail = {
				files = files,
				skills = skills,
				tools = tools,
				largest = largest,
			}
		end
		return { breakdown = b, window = window, rows = rows, detail = detail }
	end

	local function build_ui_lines(report)
		local b, window, rows, detail = report.breakdown, report.window, report.rows, report.detail
		local lines = {}
		local function out(label, n, style, pct_of)
			local pct = (pct_of or 0) > 0 and math.floor(n * 100 / pct_of + 0.5) or nil
			lines[#lines + 1] = ui.Line({
				ui.Span(("%-26s"):format(label)):hl(style or "MareMuted"),
				ui.Span(("%8s"):format(fmt_k(n))):hl(style or "MareText"),
				ui.Span(pct and ("  %3d%%"):format(pct) or ""):hl("MareDim"),
			})
		end
		local base = window or b.total
		for _, r in ipairs(rows) do
			out(r[1], r[2] or 0, nil, base)
		end
		lines[#lines + 1] = ui.Line({ ui.Span(string.rep("─", 40)):hl("MareSubtle") })
		out("Total", b.total, "MareHeading", window)
		if window then
			out("Free space", b.free, b.free < window * 0.1 and "MareWarning" or "MareSuccess", window)
			lines[#lines + 1] = ui.Line({
				ui.Span(ui.Gauge(math.min(1, b.total / window), 40))
					:hl(b.over and "MareError" or (b.total / window >= 0.85 and "MareWarning" or "MareAccent")),
			})
		end
		if b.over then
			local warn = ("Over the context window by %s tokens: the next request will fail or be cut — /compact now"):format(
				fmt_k(b.over)
			)
			lines[#lines + 1] = ui.Gap()
			lines[#lines + 1] = ui.Paragraph(ui.Line({ ui.Span("⚠ " .. warn):hl("MareError") }))
		end
		if detail then
			local function section_head(t)
				lines[#lines + 1] = ui.Gap()
				lines[#lines + 1] = ui.Line({ ui.Span(t):hl("MareHeading") })
			end
			local function item(label, n)
				lines[#lines + 1] = ui.Line({
					ui.Span("  " .. mare.text.trunc_width(label, 40)):hl("MareMuted"),
					ui.Span(("  %s"):format(fmt_k(n))):hl("MareText"),
				})
			end
			section_head("Instruction files")
			for _, f in ipairs(detail.files) do
				item(f[1], f[2])
			end
			section_head("Skills")
			for _, sk in ipairs(detail.skills) do
				item(sk[1], sk[2])
			end
			if detail.tools then
				section_head("Tool definitions")
				for _, d in ipairs(detail.tools) do
					item(d[1], d[2])
				end
			end
			section_head("Largest messages")
			for _, m in ipairs(detail.largest) do
				item(m[1], m[2])
			end
		else
			lines[#lines + 1] = ui.Gap()
			lines[#lines + 1] = ui.Line({ ui.Span("/context all: per file, skill, tool and message"):hl("MareHint") })
		end
		return ui.Lines(lines)
	end

	local function format_text_report(report)
		local b, window, rows, detail = report.breakdown, report.window, report.rows, report.detail
		local text_lines = {}
		local function out(label, n, pct_of)
			local pct = (pct_of or 0) > 0 and math.floor(n * 100 / pct_of + 0.5) or nil
			text_lines[#text_lines + 1] = ("%-26s %8d%s"):format(label, n, pct and ("  %3d%%"):format(pct) or "")
		end
		local base = window or b.total
		for _, r in ipairs(rows) do
			out(r[1], r[2] or 0, base)
		end
		out("Total", b.total, window)
		if window then
			out("Free space", b.free, window)
			text_lines[#text_lines + 1] = ("%-26s %8d"):format("Window", window)
		end
		if b.over then
			local warn = ("Over the context window by %s tokens: the next request will fail or be cut — /compact now"):format(
				fmt_k(b.over)
			)
			text_lines[#text_lines + 1] = "! " .. warn
		end
		if detail then
			local function section_head(t)
				text_lines[#text_lines + 1] = ""
				text_lines[#text_lines + 1] = t
			end
			local function item(label, n)
				text_lines[#text_lines + 1] = ("  %-40s %8d"):format(label, n)
			end
			section_head("Instruction files")
			for _, f in ipairs(detail.files) do
				item(f[1], f[2])
			end
			section_head("Skills")
			for _, sk in ipairs(detail.skills) do
				item(sk[1], sk[2])
			end
			if detail.tools then
				section_head("Tool definitions")
				for _, d in ipairs(detail.tools) do
					item(d[1], d[2])
				end
			end
			section_head("Largest messages")
			for _, m in ipairs(detail.largest) do
				item(m[1], m[2])
			end
		end
		return table.concat(text_lines, "\n")
	end

	local function context_report(args)
		local report = compute_report(args)
		local b, window = report.breakdown, report.window
		ctx.bus:emit("context/report", b)
		if mare.ui.connected() then
			ui.overlay(
				("Context · %s%s"):format(
					fmt_k(b.total),
					window and (" of %s (%d%%)"):format(fmt_k(window), math.floor(b.total * 100 / window + 0.5)) or ""
				),
				build_ui_lines(report)
			)
		elseif mare.mode == "print" or mare.mode == "tui" then
			-- Never print() plain text when stdout carries a protocol stream (rpc/acp/json/child).
			print(format_text_report(report))
		end
		return b
	end
	return { context_report = context_report }
end
