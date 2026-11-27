---@meta _
-- The `context` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string = S.object, S.string
return {
	["context"] = {
		doc = "System prompt assembly: sections, suppressions and variables plugins register, plus the "
			.. "context files, skills and prompt templates it reads.",
		methods = {
			section = {
				doc = "Register a prompt section (`name`, `order`, `text`, `when`, `scope`, ...); released with the caller or returned disposer.",
				lua = { def = "mare.PromptSection" },
				order = { "def" },
				registration = true,
			},
			suppress_runtime = {
				doc = "Hide non-required runtime sections (`runtime = true`, not `required = true`) in the matching layer until released.",
				params = object({
					scope = { type = "string", enum = { "global", "user", "agent" } },
					agent = string,
				}),
				order = { "..." },
				registration = true,
			},
			suppress = {
				doc = "Hide a section by name in every scope until released; sections registered under that name later stay hidden too.",
				params = object({ name = string }, { "name" }),
				order = { "name" },
				registration = true,
			},
			variable = {
				doc = "Register a `{{name}}` for interpolated sections (name: ^[a-z][a-z0-9_]*$) until released.",
				params = object({ name = string }, { "name" }),
				lua = { fn = "fun(info: {agent?: string, model?: table, scope: string}): any" },
				order = { "name", "fn" },
				registration = true,
			},
			system_prompt = {
				doc = "The assembled prompt (cached per model and agent); raises when it can't be built.",
				lua_unchecked = true,
				params = object({ agent = string }),
				lua = { model = "mare.Model?" },
				order = { "model", "agent" },
				result = string,
			},
			preview = {
				doc = "Preview the assembled prompt with candidate sections and section diagnostics without modifying the registry.",
				lua_unchecked = true,
				params = object({
					agent = string,
					replace = S.boolean,
					suppress_runtime = S.boolean,
				}),
				lua = {
					model = "mare.Model?",
					sections = "mare.PromptSection[]?",
				},
				order = { "model", "agent", "..." },
				result = { type = "object", additionalProperties = true },
				lua_result = "mare.PromptPreview",
			},
			replaced = {
				doc = "Whether SYSTEM.md or the `system` option replaces the whole default prompt, else nil.",
				params = object({}),
				order = {},
				result = { type = { "string", "null" } },
			},
			files = {
				doc = "The context files read into the prompt: {path, content}.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = {
					type = "array",
					items = object({ path = string, content = string }, { "path", "content" }, true),
				},
			},
			skills = {
				doc = "The skills offered to the model: {name, description, path}.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = {
					type = "array",
					items = object(
						{ name = string, description = string, path = string },
						{ "name", "description", "path" },
						true
					),
				},
			},
			skill_problems = {
				doc = "What the last skill scan warned about or skipped: {path, message, skipped?}.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = {
					type = "array",
					items = object(
						{ path = string, message = string, skipped = S.boolean },
						{ "path", "message" },
						true
					),
				},
			},
			report = {
				doc = "Per-source /context report table.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { type = "object", additionalProperties = true },
			},
			templates = {
				doc = "Prompt templates, each registered as /name unless a command with that name exists.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = {
					type = "array",
					items = object({ name = string, path = string }, { "name", "path" }, true),
				},
			},
			expand_template = {
				doc = "Fill a template body with shell-style split arguments ($1, $@, ${1:-default}...).",
				params = object({ body = string, args = string }, { "body" }),
				order = { "body", "args" },
				result = string,
			},
			invalidate = {
				doc = "Drop the cached prompt; the next request assembles it again.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = object({}),
			},
		},
		events = {
			["report"] = {
				doc = "Context breakdown report built for inspection.",
				mode = "emit",
				expose = "lua",
				data = { type = "object", additionalProperties = true },
			},
			["system"] = {
				doc = "Post-process the assembled system prompt text.",
				mode = "waterfall",
				expose = "process",
				data = object(
					{ text = string, model = { type = "object", additionalProperties = true } },
					{ "text" },
					true
				),
				result = object({ text = string }, nil, true),
			},
			["transform"] = {
				doc = "Transform outbound message history before each LLM request.",
				mode = "waterfall",
				expose = "process",
				emitters = { "agent" },
				data = object({
					messages = { type = "array", items = { type = "object", additionalProperties = true } },
					model = { type = "object", additionalProperties = true },
				}, { "messages" }, true),
				result = object({
					messages = { type = "array", items = { type = "object", additionalProperties = true } },
					model = { type = "object", additionalProperties = true },
				}, nil, true),
			},
		},
	},
}
