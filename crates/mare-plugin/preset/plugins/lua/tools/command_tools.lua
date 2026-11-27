-- tools.command_tools: user-defined command tools (mare.tool.command) and tools.disable;
-- part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local tools = D.registry.tools
	local function clean_output(...)
		return D.bash_output.clean_output(...)
	end

	---------------------------------------------------------------------------
	-- user-defined command tools: mare.tool.command { name, command = {...}, params }
	---------------------------------------------------------------------------
	function tools.command(self, caller, def)
		local props, required, defaults = {}, {}, {}
		for pname, p in pairs(def.params or {}) do
			props[pname] = { type = p.type or "string", description = p.description }
			if p.required then
				required[#required + 1] = pname
			end
			if p.default ~= nil then
				defaults[pname] = p.default
			end
		end
		table.sort(required)
		local argv_template = {}
		for i, item in ipairs(def.command) do
			argv_template[i] = item
		end
		self.register(self, caller, def.name, {
			snippet = def.snippet or def.description,
			-- safe default: a command may have side effects unless it declares readonly = true
			side_effect = def.side_effect or (def.readonly == true and "read" or "external"),
			idempotent = def.idempotent,
			command_tool = true,
			command = {
				argv = argv_template,
				defaults = next(defaults) ~= nil and defaults or nil,
			},
			description = def.description,
			sensitive_inputs = def.sensitive_inputs,
			parameters = { type = "object", properties = props, required = mare.json.array(required) },
			timeout_ms = def.timeout_ms,
			summary = function(a)
				local parts = {}
				for _, pn in ipairs(required) do
					parts[#parts + 1] = tostring(a[pn])
				end
				return ("%s(%s)"):format(def.name, table.concat(parts, ", "))
			end,
			execute = function(a, t)
				local expanded, err = require("mare_lib").expand_argv_template(argv_template, defaults, a)
				assert(expanded, err)
				local cmd = table.remove(expanded, 1)
				local pol = tools.env_policy(tools, ctx)
				local r = mare.proc.run({
					cmd = cmd,
					args = expanded,
					stderr = "merge",
					cancel = t.cancel,
					env = tools.process_env(tools, ctx),
					env_include = pol.env_include,
					env_exclude = pol.env_exclude,
					timeout_ms = def.timeout_ms or cfg.bash_timeout_ms,
				})
				local out = clean_output(r.stdout or "")
				if out == "" then
					out = (r.code == 1) and "No results." or "(no output)"
				end
				return {
					content = out,
					is_error = r.code ~= 0 and r.code ~= 1,
					max_line_chars = cfg.max_line_chars,
					details = { output = out },
				}
			end,
			render = function(_, res, expanded)
				local text = string.gsub((res.details and res.details.output) or "", "%s+$", "")
				local n = select(2, string.gsub(text, "\n", "")) + (text ~= "" and 1 or 0)
				if not expanded then
					return ui.Line({ ui.Span(("%d line%s"):format(n, n == 1 and "" or "s")):hl("MareMuted") })
				end
				return ui.Paragraph(ui.Line({ ui.Span(text):hl("MareMuted") }))
			end,
		})
	end

	for _, def in ipairs(mare.config.tool_commands or {}) do
		local ok, err = pcall(tools.command, tools, ctx, def)
		if not ok then
			mare.log("warn", "tool.command " .. tostring(def.name) .. ": " .. tostring(err))
		end
	end

	-- mare.tools.disable { ... } and CLI --exclude-tools: hidden from requests and refused at execution
	local function glob_match(pat, s)
		local p = "^" .. pat:gsub("[%^%$%(%)%%%.%[%]%+%-%?]", "%%%0"):gsub("%*", ".*") .. "$"
		return s:match(p) ~= nil
	end
	local disabled, disabled_patterns = {}, {}
	local excluded_exact, excluded_patterns = {}, {}
	local function add_entry(exact, patterns, pat)
		if type(pat) ~= "string" or pat == "" then
			return
		end
		if pat:find("*", 1, true) then
			patterns[pat] = true
		else
			exact[pat] = true
		end
	end
	for _, n in ipairs(mare.config.tools_disabled or {}) do
		add_entry(disabled, disabled_patterns, n)
	end
	for _, n in ipairs(mare.config.tools_excluded or {}) do
		add_entry(excluded_exact, excluded_patterns, n)
	end
	function tools.disable(_, _, name)
		add_entry(disabled, disabled_patterns, name)
		tools.invalidate(tools, ctx)
	end
	function tools.enable(_, _, name)
		if type(name) == "string" then
			disabled[name] = nil
			disabled_patterns[name] = nil
			tools.invalidate(tools, ctx)
		end
	end
	tools.is_disabled = function(_, _, name)
		if type(name) ~= "string" then
			return false
		end
		if disabled[name] == true or excluded_exact[name] == true then
			return true
		end
		for pat in pairs(disabled_patterns) do
			if glob_match(pat, name) then
				return true
			end
		end
		for pat in pairs(excluded_patterns) do
			if glob_match(pat, name) then
				return true
			end
		end
		return false
	end
end
