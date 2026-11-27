-- context: system prompt assembly-style: a short preamble, one-line tool
-- snippets, rules, AGENTS.md / CLAUDE.md, skills as name + description only, cwd.
-- The prompt is built once per session/model and never changes mid-session (cache-friendly):
-- no dates or other per-turn data. It keeps the section registry (context.registry); every
-- section is registered by the plugin that owns it (tools: the tool list, permissions: the
-- sandbox, tools-apply-patch, mcp, user-config) and assembled in (order, tag) order.

--- frontmatter(text) -> fields, body, warning?: SKILL.md and prompt templates share one real YAML
--- parser: `>` / `|` blocks, quoted escapes, maps, comments.
--- Unknown keys (tier, surface, …) are kept as they are.
local function frontmatter(text)
	return mare.text.frontmatter(text or "")
end

--- `s` with every invalid UTF-8 sequence replaced by U+FFFD
local function lossy(s)
	if utf8.len(s) then
		return s
	end
	local out, i = {}, 1
	while i <= #s do
		local ok, bad = utf8.len(s, i)
		if ok then
			out[#out + 1] = s:sub(i)
			break
		end
		out[#out + 1] = s:sub(i, bad - 1) .. "\u{FFFD}"
		i = bad + 1
	end
	return table.concat(out)
end

--- a one-line form of a (possibly multi-line YAML) description for menus
local function one_line(s)
	return (tostring(s or ""):gsub("%s+", " "):gsub("^ ", ""):gsub(" $", ""))
end

-- Agent Skills spec limits
local MAX_NAME, MAX_DESCRIPTION, MAX_COMPATIBILITY = 64, 1024, 500

--- Validate skill name: warnings only, the skill still loads.
local function name_problems(name)
	local out = {}
	if #name > MAX_NAME then
		out[#out + 1] = ("name exceeds %d characters (%d)"):format(MAX_NAME, #name)
	end
	if not name:match("^[a-z0-9-]+$") then
		out[#out + 1] = "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
	end
	if name:match("^%-") or name:match("%-$") then
		out[#out + 1] = "name must not start or end with a hyphen"
	end
	if name:find("--", 1, true) then
		out[#out + 1] = "name must not contain consecutive hyphens"
	end
	return out
end

local function xml(s)
	return (
		string.gsub(
			s or "",
			"[&<>\"']",
			{ ["&"] = "&amp;", ["<"] = "&lt;", [">"] = "&gt;", ['"'] = "&quot;", ["'"] = "&apos;" }
		)
	)
end

return {
	name = "context",
	-- in the minimal plugin set
	tags = { "minimal" },
	inject = { "bus", "task" },
	provide = { "context" },
	config_schema = {
		-- per directory, the first existing name wins: AGENTS.override.md (local override),
		-- then AGENTS.md, then CLAUDE.md
		files = {
			type = "list",
			items = { type = "string" },
			default = { "AGENTS.override.md", "AGENTS.md", "CLAUDE.md" },
			desc = "Project instruction file names; per directory the first existing one wins.",
		},
		project_root_markers = {
			type = "list",
			items = { type = "string" },
			default = { ".git" },
			desc = "Markers of a project root; {} = only the cwd.",
		},
		project_doc_max_bytes = {
			type = "integer",
			default = 32 * 1024,
			desc = "Byte cap of the project documents.",
		},
		skill_dirs = {
			type = "list",
			items = { type = "string" },
			default = { "~/.config/mare/skills", "~/.agents/skills", ".mare/skills", ".agents/skills" },
			desc = "Directories skills are loaded from.",
		},
		-- "" = unset (a string default, so `mare.plugin("context", { … })` knows these keys)
		append = {
			type = "string",
			default = "",
			desc = "Extra text appended to the system prompt (with APPEND_SYSTEM.md).",
		},
		-- the whole system prompt, verbatim, like SYSTEM.md: no default section is sent, the
		-- sandbox and untrusted-content ones included
		system = {
			type = "string",
			default = "",
			desc = "The whole system prompt, verbatim, like SYSTEM.md; no default section is sent.",
		},
		persona = {
			type = "string",
			default = "",
			desc = "Replaces only the identity line; `{{model}}` etc. are interpolated.",
		},

		-- prompt templates: `<dir>/<name>.md` → `/name`; project ones
		-- (relative paths) load only in trusted projects
		-- (nil = `<config dir>/prompts` plus `.mare/prompts`)
		prompt_dirs = {
			type = "list",
			items = { type = "string" },
			desc = "Prompt template directories (`<dir>/<name>.md` becomes `/name`); nil = `<config dir>/prompts` plus `.mare/prompts`.",
		},
		rules = { type = "list", items = { type = "string" }, default = {}, desc = "Extra rule bullets." },
	},
	apply = function(ctx, cfg)
		local cache = nil

		local context = {}

		-- structural submodules (plugins/lua/context/), loaded here in dependency order; they
		-- share the file-level helpers above and the `context` service table
		local D = { ctx = ctx, cfg = cfg, context = context }
		-- the optional services, read on each call: D.svc.<name> is the
		-- service while it is up, nil otherwise
		D.svc = require("mare_lib").followers(ctx, { "tools", "agent", "compact", "llm" })
		D.tools = function()
			return D.svc.tools
		end
		D.invalidate = function()
			cache = nil
		end
		D.frontmatter, D.lossy, D.one_line, D.xml = frontmatter, lossy, one_line, xml
		D.name_problems, D.MAX_DESCRIPTION, D.MAX_COMPATIBILITY = name_problems, MAX_DESCRIPTION, MAX_COMPATIBILITY
		-- a project root marker at `d` (instructions and the skills freshness stamp walk up to the
		-- first); `.git` as Git's discovery reads it: a valid repository, or an invalid `.git`
		-- file where that discovery stops (a `.git` file is a root marker and nothing above it is
		-- searched)
		function D.root_marker(d, m)
			if m == ".git" then
				local gitdir, stop = mare.fs.git_dir(d)
				return gitdir ~= nil or stop ~= nil
			end
			return mare.fs.exists(d .. "/" .. m)
		end
		-- cwd first, then its ancestors through the first project root, if present.
		function D.ancestor_dirs()
			local dirs, root = {}, nil
			local d = mare.cwd
			while d and d ~= "" do
				dirs[#dirs + 1] = d
				for _, marker in ipairs(cfg.project_root_markers or {}) do
					if D.root_marker(d, marker) then
						root = d
						break
					end
				end
				if root then
					break
				end
				d = string.match(d, mare.os == "windows" and "^(.*)[/\\][^/\\]+$" or "^(.*)/[^/]+$")
			end
			return dirs, root
		end
		-- the section registry: every plugin registers its own sections
		local registry = require("context.registry")(D)
		D.registry = registry

		--- section{ name, order, text = string|fun(model, info): string?, when = fun(model): boolean,
		---          tag?, raw?, complete?, interpolate?, runtime?, required?, scope = nil|"user"|"agent",
		---          agent? } -> release (until called or the caller is disposed)
		function context.section(_, caller, def)
			return registry.section(caller, def)
		end
		--- suppress_runtime(opts?) -> release: hide the optional runtime-context sections
		--- (`runtime = true` and not `required = true`) in the matching layer until the release is
		--- called or the caller is disposed. opts: { scope = "global"|"user"|"agent", agent? }
		function context.suppress_runtime(_, caller, opts)
			return registry.suppress_runtime(caller, opts)
		end
		--- suppress(name): hide a section in every layer until the caller is disposed — sections
		--- registered later under that name stay hidden too.
		function context.suppress(_, caller, name)
			return registry.suppress(caller, name)
		end
		--- variable(name, fn(info) -> value?): a `{{name}}` for interpolated sections
		function context.variable(_, caller, name, fn)
			return registry.variable(caller, name, fn)
		end

		-- this plugin's sections: persona and suffix, rules, project context, skills, runtime
		-- facts, then a sub-agent's own layer (tools and sandbox belong to their plugins)
		-- instruction files (AGENTS.md reading, budget, subdirectory notes) and the /context
		-- report register nothing when loaded: each loads the first time it is needed (the first
		-- prompt build, tool result or /context run) instead of during startup
		local lib = require("mare_lib")
		local instructions_loaded = false
		local function cold(loader)
			local _, proxy = lib.lazy_group({}, {}, loader)
			return proxy
		end
		D.instructions = cold(function()
			instructions_loaded = true
			return require("context.instructions")(D)
		end)
		D.report = cold(function()
			return require("context.report")(D)
		end)
		require("context.persona")(D)
		require("context.rules")(D)
		-- AGENTS.md text is file text: verbatim, never interpolated
		context.section(context, ctx, {
			name = "project_context",
			order = 900,
			interpolate = false,
			text = function(...)
				return D.instructions.project_context_text(...)
			end,
		})
		-- skills: the section, listeners and cache teardown are registered here, in their
		-- place among this plugin's registrations; the scan and its cache (context.skills_scan)
		-- load the first time one of them runs (in the TUI: the background read at ui/ready,
		-- after the first frame)
		local function package_dirs(kind)
			local ok, rows = pcall(function()
				return mare.plugins and mare.plugins.dirs and mare.plugins.dirs(kind)
			end)
			local out = {}
			for _, r in ipairs(ok and rows or {}) do
				out[#out + 1] = r.dir
			end
			return out
		end
		-- installed packages' `skills/` and `prompts/`; the package set changes only on /reload,
		-- which applies this plugin again
		D.pkg_skill_dirs = package_dirs("skills")
		local skills_loaded = false
		local scan = cold(function()
			skills_loaded = true
			return require("context.skills_scan")(D)
		end)
		D.skills = {
			pkg_prompt_dirs = package_dirs("prompts"),
			load_skills = function(...)
				return scan.load_skills(...)
			end,
			freshness_stamp = function(...)
				return scan.freshness_stamp(...)
			end,
		}
		ctx:effect(function()
			return function()
				if skills_loaded then
					scan.deactivate()
				end
			end
		end, "skills cache")
		ctx:on("ui/ready", function()
			ctx:spawn(function()
				pcall(D.skills.load_skills)
			end)
		end)
		--- skill_problems() -> {{path, message, skipped?}}: what the last skill scan warned about
		--- or skipped, for /skills and `mare doctor`
		function context.skill_problems(_, _)
			return scan.skill_problems()
		end
		ctx:on("kernel/doctor", function(checks)
			scan.doctor_check(checks)
		end)
		-- descriptions are file text: verbatim, never interpolated
		context.section(context, ctx, {
			name = "skills",
			order = 950,
			interpolate = false,
			text = function()
				return scan.section_text()
			end,
		})
		require("context.runtime")(D)
		require("context.agent_layer")(D)
		local function read_context_files()
			return D.instructions.read_context_files()
		end
		local load_skills, freshness_stamp = D.skills.load_skills, D.skills.freshness_stamp

		function context.files(_, _)
			return read_context_files()
		end
		function context.skills(_, _)
			local skills = load_skills()
			return skills
		end
		function context.invalidate(_, _)
			cache = nil
		end
		--- "system prompt replaced by <source> — …" while SYSTEM.md or `system` replaces the
		--- default prompt (doctor, /status), else nil
		function context.replaced(_, _)
			return D.replaced()
		end

		-- the sandbox and untrusted-content sections vanish with a full replacement: say so once per
		-- session. The TUI hears it at app/ready (read on a task of its own: a yielding read inside
		-- the awaited dispatch stalls later turns); headless / RPC get it on stderr from the first
		-- prompt build, which every run waits for before its first request, so a short run can't
		-- exit without it. A sub-agent's VM stays quiet.
		local announced = mare.mode == "child"
		local function announce()
			if announced then
				return
			end
			announced = true
			local msg = D.replaced()
			if not msg then
				return
			end
			if mare.ui.connected() then
				mare.ui.notify(msg, "warning")
			else
				io.stderr:write("mare: warning: " .. msg .. "\n")
			end
		end
		ctx:on("app/ready", function()
			if mare.ui.connected() then
				ctx:spawn(announce)
			end
		end)

		-- a sub-agent's own name: its prompt is assembled for it unless a caller names another
		local own_agent = mare.mode == "child" and mare.args and mare.args.agent_label or nil
		local function run_system_hook(text, model)
			local wf = ctx.bus:waterfall("context/system", { text = text, model = model })
			return (type(wf) == "table" and type(wf.text) == "string" and wf.text) or text
		end
		--- Built once per session/model/agent; never changes mid-session (cache-friendly).
		function context.system_prompt(_, _, model, agent)
			agent = agent or own_agent
			local key = (model and model.ref or "") .. "|" .. (agent or "")
			if cache and cache.key == key then
				return cache.text
			end
			local text = registry.assemble(model, agent)
			if not mare.ui.connected() then
				announce()
			end
			text = run_system_hook(text, model)
			cache = { key = key, text = text }
			return text
		end

		--- preview(model?, agent?, opts?) -> mare.PromptPreview: the prompt with the candidate
		--- `opts.sections` (without the caller's own sections when `opts.replace`, without the
		--- runtime sections when `opts.suppress_runtime`) through the context/system post-hook, and
		--- each section's place in it; publishes nothing. A candidate that can't be built (an
		--- unknown {{var}}, two complete sections) raises here.
		function context.preview(_, caller, model, agent, opts)
			agent = agent or own_agent
			return registry.preview(caller, model, agent, opts, function(text)
				return run_system_hook(text, model)
			end)
		end

		ctx:on("workspace/changed", function()
			cache = nil
		end)
		-- before the first request of a prompt: rebuild only if an instruction file or skill
		-- changed, and build it now so a prompt that can't be built is "not sent" (context/
		-- readiness). Mid-prompt changes wait for the next prompt (built once per prompt).
		local ready_state
		local function readiness()
			ready_state = ready_state
				or require("context.readiness").new({
					stamp = freshness_stamp,
					invalidate = function()
						cache = nil
					end,
					build = function(model)
						context.system_prompt(context, ctx, model)
					end,
				})
			return ready_state
		end
		-- one step of llm's ordered readiness chain, from a
		-- follower of llm: withdrawn with it, registered again for a new provider
		ctx:inject({ "llm" }, function(sub)
			sub.llm:readiness("context", {
				order = require("mare_lib").READY_ORDER.context,
				prepare = function(args)
					readiness().prepare(args)
				end,
				ready = function()
					return readiness().ready()
				end,
			})
		end)
		ctx:on("session/changed", function()
			cache = nil
		end)
		-- the sandbox section lists what the preset allows: a preset switch rebuilds the prompt
		-- (once; the model must not keep asking for what it now has, or think it still has it)
		ctx:on("approval/changed", function()
			cache = nil
		end)
		ctx:on("permission/changed", function()
			cache = nil
		end)
		ctx:on("sandbox/changed", function()
			cache = nil
		end)
		-- plugins that own a section but apply before this one, or don't depend on it (tools,
		-- permissions), follow this service (`mare_lib.follow`, `ctx:inject({ "context" }, fn)`): their sections
		-- are registered here, during `provide`. Not an optional dependency the other way:
		-- `/reload context` must not reload them.
		require("context.slash")(D)
		-- /context: registered while the optional `commands` service is up (a child fiber on the
		-- cordis tree); the report itself loads on the first run
		local context_def = {
			description = "Show what the context is made of (/context all: per item)",
			args = "[all]",
			auto_execute = true,
			run = function(args)
				return D.report.context_report(args)
			end,
		}
		ctx:inject({ "commands" }, function(sub)
			sub.commands:register("context", context_def)
			-- registered again once every plugin applied, so this one stays on top of a /context
			-- another plugin registered after it
			sub:on("app/ready", function()
				local current = sub.commands:get("context")
				if
					not current
					or current.description ~= context_def.description
					or current.args ~= context_def.args
				then
					sub.commands:register("context", context_def)
				end
			end)
		end)
		function context.report(_, _, args)
			return D.report.context_report(args or "")
		end
		-- a mare.Service: every method is on `context` by now
		require("mare_lib").service(ctx, "context", context)

		-- subdirectory instruction files, registered here so
		-- each listener keeps its place among this plugin's session/changed listeners
		-- nothing was included yet while the instructions module is still unloaded
		ctx:on("session/changed", function(...)
			if instructions_loaded then
				return D.instructions.reset_included(...)
			end
		end)
		ctx:on("session/loaded", function(ev)
			if instructions_loaded and type(ev) == "table" and ev.navigated then
				return D.instructions.reset_included()
			end
		end)
		ctx:on("compact/completed", function(ev)
			if instructions_loaded and type(ev) == "table" and (ev.status == "completed" or ev.changed == true) then
				return D.instructions.reset_included()
			end
		end)
		-- tool/after is a bus waterfall: the around form passes `{ result, call, operation }` on to later listeners
		ctx:on("tool/after", function(data, ev)
			data.result = D.instructions.subdir_instructions(data.result, data.call)
			return ev.next(data)
		end)
	end,
}
