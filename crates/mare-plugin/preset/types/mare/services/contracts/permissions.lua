---@meta _
-- The `permissions` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, integer, boolean, list_of = S.object, S.string, S.integer, S.boolean, S.list_of
local open_object = { type = "object", additionalProperties = true }

local permission_profile = object({
	read = { type = "array", items = string },
	write = { type = "array", items = string },
	network = { oneOf = { boolean, { type = "array", items = string } } },
}, nil, true)

local preset_info = object({
	label = string,
	desc = string,
	like = { type = "string", enum = { "read-only", "auto", "full-access" } },
	level = string,
}, { "like", "level" }, true)

local preset_entry = object({
	name = string,
	label = string,
	desc = string,
	like = { type = "string", enum = { "read-only", "auto", "full-access" } },
	level = string,
	current = boolean,
}, { "name", "label", "desc", "like", "level", "current" })

local permission_state = object({
	preset = string,
	label = string,
	partial = boolean,
	level = string,
	like = { type = "string", enum = { "read-only", "auto", "full-access" } },
	desc = string,
	rules = { type = "array", items = open_object },
	grants = permission_profile,
	policy_revision = integer,
}, { "preset", "label", "partial", "policy_revision" })

return {
	["permissions"] = {
		doc = "Sandbox permission presets, rule evaluation, and structured permission requests.",
		essential = true,
		errors = {
			unknown_preset = { doc = "no built-in or configured permission preset has that name" },
			ceiling_exceeded = { doc = "the requested preset exceeds the sub-agent permission ceiling" },
			needs_ui = {
				doc = "switching to full access with confirm=true requires a connected UI for first-time confirmation",
			},
			sandbox_failed = { doc = "the host sandbox failed to apply the requested preset policy" },
		},
		methods = {
			get = {
				doc = "Current active permission preset and policy revision for this session (`rules` and `grants` are included for primary/in-process callers and omitted for peer callers).",
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({}),
				order = {},
				result = permission_state,
			},
			list = {
				doc = "Available permission presets (built-in and configured) with display metadata and active indicator.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({}),
				order = {},
				result = list_of(preset_entry),
			},
			preset = {
				doc = 'Current sandbox preset ("off" when the host policy is unavailable).',
				admin = true,
				params = object({}),
				order = {},
				result = string,
			},
			set = {
				doc = "Switch the active permission preset (`confirm` asks before the first switch to full access); returns the updated permission state or raises a coded error.",
				expose = "both",
				access = "primary",
				admin = true,
				scope = "session",
				params = object({
					name = string,
					confirm = boolean,
				}, { "name" }),
				order = { "name", "..." },
				result = permission_state,
				errors = { "unknown_preset", "ceiling_exceeded", "needs_ui", "sandbox_failed" },
			},
			presets = {
				doc = "List available permission preset names.",
				admin = true,
				params = object({}),
				order = {},
				result = { type = "array", items = string },
			},
			rules = {
				doc = "Effective permission rules: `init.lua` rules, persisted rules, and session rules.",
				admin = true,
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { type = "array", items = open_object },
			},
			check = {
				doc = "Evaluate the vote the permissions plugin casts for a tool call (for UIs and tests).",
				admin = true,
				lua_unchecked = true,
				params = object({
					call = open_object,
					info = object({ side_effect = string, unconfined = boolean }, nil, true),
				}, { "call" }),
				order = { "call", "info" },
				result = object({
					decision = { type = "string", enum = { "allow", "ask", "deny" } },
					reason = string,
					sandbox = {},
					amendment = open_object,
				}, { "decision" }, true),
			},
			rule_verdict = {
				doc = "What configured and remembered rules alone decide about a call (excluding session-only rules), or nil.",
				admin = true,
				lua_unchecked = true,
				params = object({
					call = open_object,
					info = object({ side_effect = string, unconfined = boolean }, nil, true),
				}, { "call" }),
				order = { "call", "info" },
				result = { oneOf = { open_object, { type = "null" } } },
				lua_result = '{decision: "allow", sandbox: any}|{decision: "deny", reason?: string}|nil',
			},
			describe = {
				doc = "Return the display metadata of a built-in or configured preset, or nil if unknown.",
				admin = true,
				params = object({ name = string }, { "name" }),
				order = { "name" },
				result = { oneOf = { preset_info, { type = "null" } } },
			},
			add_rules = {
				doc = "Add a rule source owned by the calling plugin (removed when the caller is disposed).",
				admin = true,
				lua_unchecked = true,
				params = object({ rules = { type = "array", items = open_object } }, { "rules" }),
				order = { "rules" },
				result = object({}),
			},
			request = {
				doc = "Request structured permissions through the ordinary tool policy, approval, and execution chain.",
				admin = true,
				lua_unchecked = true,
				params = object({
					profile = permission_profile,
					justification = string,
					scope = { type = "string", enum = { "turn", "session" } },
					request_id = string,
				}, { "profile", "justification" }),
				lua = {
					profile = "mare.PermissionProfile",
					cancel = "mare.Token?",
					approve = "function?",
					on_state = "function?",
				},
				order = { "profile", "..." },
				result = open_object,
				lua_result = { "table", "table?" },
			},
			grants = {
				doc = "Detached snapshot of this turn's and session's granted permission profiles.",
				admin = true,
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = permission_profile,
				lua_result = "mare.PermissionProfile",
			},
			describe_request = {
				doc = "Describe a frozen call's structured permission request without granting it.",
				admin = true,
				lua_unchecked = true,
				params = object({
					call = open_object,
					expected = string,
				}, { "call" }),
				order = { "call", "expected" },
				result = { oneOf = { permission_profile, { type = "null" } } },
				lua_result = { "mare.PermissionProfile|nil", '"turn"|"session"|nil' },
			},
			resolve_request = {
				doc = "Intersect approval constraints with a frozen permission request and current policy.",
				admin = true,
				lua_unchecked = true,
				params = object({
					call = open_object,
					constraints = { type = "array", items = permission_profile },
					scope = { type = "string", enum = { "turn", "session" } },
					expected = string,
				}, { "call" }),
				lua = {
					constraints = "mare.PermissionProfile[]?",
				},
				order = { "call", "constraints", "scope", "expected" },
				result = { oneOf = { permission_profile, { type = "null" } } },
				lua_result = { "mare.PermissionProfile|nil", "table|nil", '"turn"|"session"|nil' },
			},
			commit_request = {
				doc = "Recheck and commit a previously resolved structured permission request immediately before execution.",
				admin = true,
				lua_unchecked = true,
				params = object({ call = open_object }, { "call" }),
				order = { "call" },
				result = { type = { "boolean", "null" } },
			},
			policy_revision = {
				doc = "Monotonic revision counter incremented whenever permission policy or rules change.",
				admin = true,
				params = object({}),
				order = {},
				result = integer,
			},
			is_partial = {
				doc = "Whether the active sandbox backend cannot enforce network restrictions requested by the preset.",
				admin = true,
				params = object({}),
				order = {},
				result = boolean,
			},
		},
	},
}
