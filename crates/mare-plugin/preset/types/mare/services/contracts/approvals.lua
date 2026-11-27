---@meta _
-- The `approvals` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, integer, boolean = S.object, S.string, S.integer, S.boolean
local open_object = { type = "object", additionalProperties = true }

local approval_policy = {
	oneOf = {
		{ type = "string", enum = { "ask", "never", "untrusted" } },
		object({
			mode = { type = "string", enum = { "granular" } },
			categories = { type = "object", additionalProperties = boolean },
		}, { "mode", "categories" }),
	},
}

local approval_result = object({
	decision = {
		type = "string",
		enum = { "allow", "allow_session", "allow_amend", "deny", "abort", "unavailable" },
	},
	reason = string,
	source = string,
	human_approval = string,
	outcome = { type = "string", enum = { "timeout", "cancelled" } },
	permissions = open_object,
	permission_constraints = { type = "array", items = open_object },
	scope = { type = "string", enum = { "turn", "session" } },
}, { "decision" }, true)

return {
	["approvals"] = {
		doc = "Independent approval policy, gating, reviewer registration, and approval requests.",
		essential = true,
		methods = {
			policy = {
				doc = "Current effective approval policy.",
				admin = true,
				params = object({}),
				order = {},
				result = approval_policy,
				lua_result = "mare.ApprovalPolicy",
			},
			set = {
				doc = "Persist a session override before changing the effective approval policy.",
				admin = true,
				lua_unchecked = true,
				params = object({ value = approval_policy }, { "value" }),
				order = { "value" },
				result = approval_policy,
				lua_result = "mare.ApprovalPolicy",
			},
			revision = {
				doc = "Monotonic revision counter incremented whenever policy or reviewers change.",
				admin = true,
				params = object({}),
				order = {},
				result = integer,
			},
			version = {
				doc = "Reload-safe service incarnation plus policy/reviewer revision.",
				admin = true,
				params = object({}),
				order = {},
				result = string,
			},
			gate = {
				doc = "Evaluate the independent policy gate for a category; returns a denial or nil.",
				admin = true,
				params = object({ category = string }, { "category" }),
				order = { "category" },
				result = { oneOf = { approval_result, { type = "null" } } },
				lua_result = "mare.ApprovalResult?",
			},
			request = {
				doc = "Apply the independent gate, selected reviewer, cancellation and audit lifecycle for an approval request.",
				admin = true,
				lua_unchecked = true,
				params = object({
					category = string,
					approval_id = string,
					child_approval_id = integer,
					emit_resolved = boolean,
				}, { "category" }),
				lua = {
					action = "table",
					policy = "async fun(binding: mare.ApprovalBinding): table",
					answer = "async fun(binding: mare.ApprovalBinding): mare.ApprovalResult",
					cancel = "mare.Token?",
				},
				order = { "category", "action", "policy", "answer", "cancel", "..." },
				result = approval_result,
				lua_result = { "mare.ApprovalResult", "mare.HumanApprovalReceipt?" },
			},
			register_reviewer = {
				doc = "Register a named approval reviewer owned by the calling ctx; automatically withdrawn on dispose.",
				admin = true,
				registration = true,
				lua_unchecked = true,
				params = object({ name = string }, { "name" }),
				lua = {
					review = 'async fun(action: table, cancel?: mare.Token): {decision: "allow"|"deny", reason?: string}',
					admit = "(async fun(cancel?: mare.Token): boolean, string?)?",
				},
				order = { "name", "review", "..." },
			},
			admit = {
				doc = "Validate the selected reviewer within the configured review timeout.",
				admin = true,
				lua_unchecked = true,
				params = object({}),
				lua = { cancel = "mare.Token?" },
				order = { "cancel" },
				result = { type = { "boolean", "null" } },
				lua_result = { "boolean?", "string?" },
			},
			inheritance = {
				doc = "All ancestor policy ceilings plus this session's live policy, copied for delegation.",
				admin = true,
				params = object({}),
				order = {},
				result = { type = "array", items = approval_policy },
				lua_result = "mare.ApprovalPolicy[]",
			},
			review_required = {
				doc = "Whether every request requires review under current configuration or inherited flags.",
				admin = true,
				params = object({}),
				order = {},
				result = boolean,
			},
			delegation_services = {
				doc = "Services that delegation requires even when a child changes provider configuration.",
				admin = true,
				params = object({}),
				order = {},
				result = { type = "array", items = string },
			},
		},
	},
}
