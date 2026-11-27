-- context.persona: the whole-prompt replacement (`system`: SYSTEM.md or the `system` option, a
-- complete section), the identity at the top of the system prompt (`persona`, shown untagged),
-- the default `approach` and `deliverables` sections, and the user's suffix (`persona_suffix`,
-- tagged `<addendum>`: APPEND_SYSTEM.md plus `append`); part of the `context` plugin
-- (plugins/context.lua), loaded at the start of its apply.
-- File text is passed verbatim, never interpolated; only the `persona` option and
-- the built-in identity template are.

--- Identity preamble (a template: `{{model}}` is the model id)
local function preamble(model)
	if model and model.id then
		return "You are a coding agent powered by the {{model}} model, running in mare."
	end
	return "You are a coding agent running in mare."
end

--- an option's text; "" (the default, so `mare.plugin("context", { … })` knows the key) is unset
local function text_opt(v)
	return type(v) == "string" and v ~= "" and v or nil
end

-- Autonomy and persistence guidance for the agent.
local APPROACH = table.concat({
	"- Stay with the work until the task is handled end to end within the current turn whenever that"
		.. " is feasible. Do not stop at analysis or half-finished fixes; carry the work through"
		.. " implementation, verification, and a clear account of the outcome.",
	"- Unless the user asks for a plan, asks a question about the code, or is brainstorming approaches,"
		.. " assume they want you to make the change: do not stop at a proposal. If you hit a blocker, try"
		.. " to work through it yourself before handing the problem back.",
	"- Do what was asked — no less, no more, and nothing different. Leave out anything the ask does not"
		.. " call for.",
	"- When the standard way is blocked, do not quietly route around it, and do not shrink the"
		.. " deliverable on your own. Finish all the parts that are not blocked, and state plainly what"
		.. " remains; whether to accept a smaller result is the user's decision, not yours.",
	"- For routine, reversible choices that follow codebase conventions, pick the sensible option,"
		.. " keep moving, and note any material assumption in your final reply; ask the user only when"
		.. " an ambiguity would materially change the outcome and cannot be resolved from the codebase.",
	"- When you have evidence the user is wrong, say so and show the evidence. Defer once they have decided.",
}, "\n")

-- Frontend / artifact deliverables guidance.
local DELIVERABLES = table.concat({
	"- When you build something with a visual or interactive experience, think about its audience and"
		.. " make it feel rich and polished: build the complete set of features, states and details a user"
		.. " would naturally expect, not a minimal sketch.",
	"- Files, code and other artifacts you produce are the work itself. Short chat replies never mean a"
		.. " smaller deliverable.",
}, "\n")

-- what a replaced prompt no longer carries (doctor, /status)
local DROPPED = "no default section is sent (sandbox, untrusted-content rule, tool list,"
	.. " AGENTS.md, skills, cwd and environment, APPEND_SYSTEM.md)"

return function(D)
	local ctx, cfg, context = D.ctx, D.cfg, D.context
	local lib = require("mare_lib")

	--- `SYSTEM.md` and `APPEND_SYSTEM.md`: the user's in the config dir, the project's in
	--- `<resource root>/.mare/` — the project's only in a trusted project and taking
	--- precedence over the user's. -> text, path
	local function system_file(name)
		if mare.trusted then
			local path = lib.project_file(".mare/" .. name)
			if path then
				local s = mare.fs.read(path)
				if s then
					return s, path
				end
			end
		end
		local path = mare.paths.config .. "/" .. name
		return mare.fs.read(path), path
	end
	--- what replaces the whole default prompt and where it comes from: the `system` option, else
	--- SYSTEM.md -> text, source | nil
	local function replacement()
		local s = text_opt(cfg.system)
		if s then
			return s, "the context plugin's `system` option"
		end
		local text, path = system_file("SYSTEM.md")
		if text and text ~= "" then
			return text, path
		end
	end
	--- "system prompt replaced by <source> — …" while the default prompt is replaced, else nil
	function D.replaced()
		local _, source = replacement()
		return source and ("system prompt replaced by %s — %s"):format(source, DROPPED) or nil
	end
	-- a complete section is the whole prompt: every other
	-- section, the sandbox and the untrusted-content rule included, is then the user's
	-- responsibility. A sub-agent's `system =` (agent layer, same name) shadows it there.
	context.section(context, ctx, {
		name = "system",
		order = 0,
		raw = true,
		complete = true,
		interpolate = false,
		text = function()
			return (replacement())
		end,
	})
	-- the identity only: the `persona` option (strictly interpolated personaPrefix), else the
	-- default preamble
	context.section(context, ctx, {
		name = "persona",
		tag = "preamble",
		order = 0,
		raw = true,
		text = function(model)
			return text_opt(cfg.persona) or preamble(model)
		end,
	})
	context.section(context, ctx, { name = "approach", order = 50, interpolate = false, text = APPROACH })
	context.section(context, ctx, { name = "deliverables", order = 60, interpolate = false, text = DELIVERABLES })
	context.section(context, ctx, {
		name = "persona_suffix",
		tag = "addendum",
		order = 800,
		interpolate = false,
		text = function()
			local parts = {}
			parts[#parts + 1] = (system_file("APPEND_SYSTEM.md"))
			parts[#parts + 1] = text_opt(cfg.append)
			return #parts > 0 and table.concat(parts, "\n\n") or nil
		end,
	})

	-- `mare doctor` / `/doctor`: say plainly when the default prompt is replaced
	ctx:on("kernel/doctor", function(checks)
		local r = D.replaced()
		if r then
			checks[#checks + 1] = { name = "system prompt", ok = true, warn = true, detail = r }
		end
	end)
end
