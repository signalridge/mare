-- context.slash: prompt templates and the /<template>, /skill:<name> and /skills commands;
-- part of the `context` plugin (plugins/context.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, context = D.ctx, D.cfg, D.context
	local frontmatter, one_line = D.frontmatter, D.one_line
	local load_skills, pkg_prompt_dirs = D.skills.load_skills, D.skills.pkg_prompt_dirs
	local lib = require("mare_lib")

	-- prompt templates: `/name [args]` sends the template with
	-- $1…, $@ / $ARGUMENTS, ${1:-default}, ${@:-default}, ${@:N}, ${@:N:L} filled in
	local function expand_template(body, args)
		local argv = args and args ~= "" and mare.text.shell_split(args) or nil
		if not argv then
			argv = {}
			for w in (args or ""):gmatch("%S+") do
				argv[#argv + 1] = w
			end
		end
		local function slice(n, l)
			local out = {}
			for i = n, l and (n + l - 1) or #argv do
				if argv[i] then
					out[#out + 1] = argv[i]
				end
			end
			return table.concat(out, " ")
		end
		body = body:gsub("%${@:(%d+):(%d+)}", function(n, l)
			return slice(tonumber(n), tonumber(l))
		end)
		body = body:gsub("%${@:(%d+)}", function(n)
			return slice(tonumber(n))
		end)
		body = body:gsub("%${@:%-(.-)}", function(d)
			return #argv > 0 and slice(1) or d
		end)
		body = body:gsub("%${(%d+):%-(.-)}", function(i, d)
			local v = argv[tonumber(i)]
			return (v and v ~= "") and v or d
		end)
		body = body:gsub("%$ARGUMENTS", function()
			return slice(1)
		end)
		body = body:gsub("%$@", function()
			return slice(1)
		end)
		body = body:gsub("%$(%d)", function(i)
			return argv[tonumber(i)] or ""
		end)
		return body
	end
	context.expand_template = function(_, _, body, args)
		return expand_template(body, args)
	end
	local function load_templates()
		local out, seen = {}, {}
		-- later directories win on a name clash (the project overrides the user, the user
		-- overrides a package)
		local dirs = {}
		for _, dir in ipairs(pkg_prompt_dirs) do
			dirs[#dirs + 1] = { dir = dir, package = true }
		end
		for _, dir in ipairs(cfg.prompt_dirs or { mare.paths.config .. "/prompts", ".mare/prompts" }) do
			dirs[#dirs + 1] = { dir = dir }
		end
		for _, d in ipairs(dirs) do
			local dir = d.dir
			-- The resolver does not enforce trust; package paths remain available without project trust.
			local project = not d.package and lib.is_project_relative(dir)
			if not project or mare.trusted then
				local root = d.package and mare.fs.resolve(dir) or lib.resolve_project_dir(dir)
				for _, e in ipairs(root and mare.fs.list(root) or {}) do
					local name = e.kind == "file" and e.name:match("^(.+)%.md$")
					if name then
						local path = root .. "/" .. e.name
						if not seen[name] then
							out[#out + 1] = name
						end
						seen[name] = path
					end
				end
			end
		end
		local list = {}
		for _, name in ipairs(out) do
			list[#list + 1] = { name = name, path = seen[name] }
		end
		return list
	end
	function context.templates(_, _)
		return load_templates()
	end

	-- /skill:name [args] — force-load a skill: its instructions go in as a user message
	local function register_commands(commands)
		for _, tp in ipairs(load_templates()) do
			-- never shadow an existing command (built-in or another plugin's)
			if commands:get(tp.name) then
				mare.log("warn", ("prompt template /%s skipped: a command with that name exists"):format(tp.name))
				goto continue
			end
			local text = mare.fs.read(tp.path) or ""
			local fm, body, warn = frontmatter(text)
			if warn then
				mare.log("warn", ("prompt template %s: %s"):format(tp.path, warn))
			end
			local desc = type(fm.description) == "string" and fm.description ~= "" and fm.description
				or (body:match("^%s*([^\n]+)") or tp.name)
			local function run_prompt(text)
				local agent = D.svc.agent
				if not agent then
					return
				end
				ctx:spawn(function()
					local res = agent:prompt(text)
					if type(res) == "table" and res.error then
						ui.notify(res.error, "warning")
					end
				end)
			end
			commands:register(tp.name, {
				description = mare.text.trunc_width(one_line(desc), 60),
				-- `argument-hint`: shown dim after the name in the `/` menu
				args = type(fm["argument-hint"]) == "string" and fm["argument-hint"] or nil,
				run = function(args)
					run_prompt(expand_template(body, args))
				end,
			})
			::continue::
		end
		for _, sk in ipairs(load_skills()) do
			commands:register("skill:" .. sk.name, {
				description = mare.text.trunc_width(one_line(sk.description), 60),
				args = sk.argument_hint,
				run = function(args)
					local _, body = frontmatter(mare.fs.read(sk.path) or "")
					local dir = string.match(sk.path, "^(.*)/[^/]+$")
					local text = require("mare_lib").data_block(
						"skill",
						{ name = sk.name, location = sk.path },
						("References are relative to %s.\n\n%s"):format(dir, body)
					)
					if args and args ~= "" then
						text = text .. "\n\n" .. args
					end
					local agent = D.svc.agent
					if agent then
						ctx:spawn(function()
							local res = agent:prompt(text)
							if type(res) == "table" and res.error then
								ui.notify(res.error, "warning")
							end
						end)
					end
				end,
			})
		end
		-- /skills also lists what the scan warned about or skipped (warnings are
		-- shown there and in doctor, never as a startup interruption)
		local base = commands:get("skills")
		if base then
			commands:register("skills", {
				description = base.description,
				run = function()
					local probs = context.skill_problems(context, ctx)
					if #probs > 0 then
						local lines = { ("Skill warnings (%d):"):format(#probs) }
						for _, pr in ipairs(probs) do
							lines[#lines + 1] = ("%s %s: %s"):format(pr.skipped and "✗" or "!", pr.path, pr.message)
						end
						ui.notify(table.concat(lines, "\n"), "warning")
					end
					local list, items = {}, {}
					for _, d in ipairs(commands:list().data) do
						if string.sub(d.name, 1, 6) == "skill:" then
							list[#list + 1] = d
							items[#items + 1] = { label = "/" .. d.name, detail = d.description }
						end
					end
					if #list == 0 then
						ui.notify("Skills: nothing to show", "info")
						return
					end
					local i = ui.select({ title = "Skills (type to filter, Enter runs)", items = items })
					if i then
						local ok, err = pcall(commands.run, commands, "/" .. list[i].name)
						if
							not ok
							and err ~= nil
							and not (type(err) == "table" and err.code == "commands.command_unavailable")
						then
							ui.notify(tostring(err), "warning")
						end
					end
				end,
			})
		end
	end
	-- registered while the optional commands service is up, from a follower fiber (it may start
	-- after this plugin; reading the template files may yield there) — withdrawn with it
	ctx:inject({ "commands" }, function(sub)
		register_commands(sub.commands)
	end)
end
