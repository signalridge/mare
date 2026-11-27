---@meta _
-- The `copy` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, integer, boolean = S.object, S.string, S.integer, S.boolean

local copy_result = object({
	ok = boolean,
	method = string,
	verified = boolean,
	bytes = integer,
	error = string,
}, { "ok", "bytes" }, true)

return {
	["copy"] = {
		doc = "Clipboard and file export helpers for assistant responses and code blocks.",
		methods = {
			last = {
				doc = "Copy the Nth-last response (default 1); opens a code-block picker when code blocks are present.",
				lua_unchecked = true,
				params = object({ n = integer }),
				order = { "n" },
				result = { oneOf = { copy_result, string, { type = "null" } } },
			},
			text = {
				doc = "Copy text to the clipboard, reporting the path used, and emit `copy/completed`.",
				params = object({ text = string, target = string }, { "text" }),
				order = { "text", "target" },
				result = copy_result,
			},
			write = {
				doc = "Write text to `mare-copy-<time>.<ext>` in the working directory and return the path, or nil on error.",
				params = object({ text = string, lang = string }, { "text" }),
				order = { "text", "lang" },
				result = { type = { "string", "null" } },
			},
			response = {
				doc = "Markdown source of the Nth-last assistant response (1 = last), or nil.",
				params = object({ n = integer }),
				order = { "n" },
				result = { type = { "string", "null" } },
			},
			blocks = {
				doc = "Extract fenced code blocks from Markdown text, in order.",
				params = object({ md = string }, { "md" }),
				order = { "md" },
				result = {
					type = "array",
					items = object({ lang = string, code = string, first = string }, { "lang", "code", "first" }),
				},
			},
			align_tables = {
				doc = "Re-pad every GFM table in Markdown text so its columns line up.",
				params = object({ md = string }, { "md" }),
				order = { "md" },
				result = string,
			},
		},
		events = {
			["completed"] = {
				doc = "Clipboard or file copy completed.",
				mode = "emit",
				expose = "process",
				data = object(
					{ method = string, bytes = integer, target = string, verified = boolean },
					{ "bytes", "target" },
					true
				),
			},
			["request"] = {
				doc = "Request the copy service to copy text to the clipboard or output.",
				mode = "emit",
				expose = "lua",
				data = object({ text = string, kind = string, from = string }, { "text" }, true),
			},
		},
	},
}
