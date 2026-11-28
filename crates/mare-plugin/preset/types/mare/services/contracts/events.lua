---@meta _
-- Built-in event-only contract roots (`policy`, `rewrite`, and standalone domain events).
local S = ...
local object, string, integer = S.object, S.string, S.integer
local open_object = { type = "object", additionalProperties = true }

local policy_base_props = {
	decision = { type = "string", enum = { "allow", "deny", "ask" } },
	reason = { type = "string" },
	category = { type = "string" },
	categories = { type = "array", items = { type = "string" } },
	grant = { type = "string", enum = { "allow", "allow_session", "allow_amend" } },
	abort = { type = "boolean" },
	unavailable = { type = "boolean" },
	human_approval = { type = "string" },
	amendment = { type = "object" },
	sandbox = {},
	sandbox_constraints = { type = "array" },
}

local function merge(extra)
	local out = {}
	for k, v in pairs(policy_base_props) do
		out[k] = v
	end
	for k, v in pairs(extra or {}) do
		out[k] = v
	end
	return { type = "object", properties = out, required = { "decision" }, additionalProperties = false }
end

local approval_result = merge({
	permissions = { type = "object" },
	permission_constraints = { type = "array", items = { type = "object" } },
	scope = { type = "string", enum = { "turn", "session" } },
	source = { type = "string" },
})

local before_result = merge({
	sandbox_grant = {},
})

return {
	["tool"] = {
		owner = "tools",
		doc = "Tool execution policy, rewrite, and pipeline events.",
		events = {
			["authorized"] = {
				doc = "A tool call received committed execution permissions.",
				mode = "emit",
				expose = "process",
				data = object({
					call = open_object,
					permissions = open_object,
					scope = string,
				}, { "call" }, true),
			},
			["after"] = {
				doc = "Inspect or rewrite a tool display result after execution.",
				mode = "waterfall",
				expose = "process",
				data = object({
					result = open_object,
					call = open_object,
					operation = open_object,
				}, { "result", "call" }, true),
				result = object({ result = open_object }, nil, true),
			},
			["before"] = {
				doc = "Policy gate evaluated on frozen tool call arguments before execution.",
				mode = "policy",
				expose = "lua",
				authority_service = "permissions",
				result = before_result,
			},
			["approval"] = {
				doc = "Tri-state tool approval decision point.",
				mode = "policy",
				expose = "lua",
				result = approval_result,
			},
			["approval-host"] = {
				doc = "Host-level tri-state tool approval decision point.",
				mode = "policy",
				expose = "lua",
				result = approval_result,
			},
			["program-result"] = {
				doc = "Transform the validated tool program result before model/display projection.",
				mode = "waterfall",
				expose = "process",
				data = object({
					result = open_object,
					call = open_object,
					operation = open_object,
				}, { "result", "call" }, true),
				result = object({ result = open_object }, nil, true),
			},
			["result"] = {
				doc = "Checked post-processing waterfall on tool display output before truncation.",
				mode = "waterfall",
				expose = "lua",
				data = object({
					result = open_object,
					call = open_object,
					operation = open_object,
				}, { "result", "call" }, true),
				result = object({ result = open_object }, nil, true),
				example = { result = {}, call = {} },
			},
			["rewrite"] = {
				doc = "Strict sequential rewrite chain run on tool arguments before freezing for policy.",
				mode = "rewrite",
				expose = "lua",
			},
			["snapshot-void"] = {
				doc = "An approval snapshot became void and will retry from rewrite.",
				mode = "emit",
				expose = "lua",
				data = object({ call = open_object, reason = string }, { "call", "reason" }),
			},
		},
	},
	["net"] = {
		owner = "permissions",
		doc = "Network access approval policy points.",
		events = {
			["approval"] = {
				doc = "Affirmative network access approval point.",
				mode = "policy",
				expose = "lua",
				affirmative = true,
			},
			["approval-host"] = {
				doc = "Host-level affirmative network access approval point.",
				mode = "policy",
				expose = "lua",
				affirmative = true,
			},
		},
	},
	["permission"] = {
		owner = "permissions",
		doc = "Permission verdict tightening, policy change, and command parse points.",
		events = {
			["changed"] = {
				doc = "Active permission preset or policy state changed.",
				mode = "emit",
				expose = "both",
				delivery = "lossy",
				ignorable = false,
				retain = "last",
				data = object({ preset = string, label = string, partial = S.boolean }, nil, true),
				example = { preset = "default", label = "DEFAULT" },
			},
			["parse"] = {
				doc = "Tighten a parsed shell command AST before rule matching.",
				mode = "waterfall",
				expose = "process",
				data = object({
					parsed = open_object,
					command = string,
					tool = string,
					shell = string,
					arguments = open_object,
				}, { "parsed" }, true),
				result = object({ parsed = open_object }, nil, true),
			},
			["verdict"] = {
				doc = "Secondary policy check that may tighten a tool/before verdict and joins ask reasons.",
				mode = "policy",
				expose = "lua",
				join_ask_reasons = true,
			},
		},
	},
	["request"] = {
		owner = "agent",
		doc = "Model request build, preflight, and readiness events.",
		events = {
			["build"] = {
				doc = "Inspect or transform an outgoing model request before dispatch.",
				mode = "waterfall",
				expose = "lua",
				emitters = { "llm" },
				data = open_object,
				result = open_object,
				example = {},
			},
			["prepare"] = {
				doc = "Abortable preflight check before building the model request.",
				mode = "serial",
				expose = "lua",
				data = open_object,
				example = { model = "m" },
			},
			["ready"] = {
				doc = "Policy gate evaluated before dispatching an LLM provider request.",
				mode = "policy",
				expose = "lua",
			},
		},
	},
	["approval"] = {
		owner = "approvals",
		doc = "Approval policy state and automated reviewer outcome events.",
		events = {
			["changed"] = {
				doc = "Approval policy or revision changed.",
				mode = "emit",
				expose = "both",
				delivery = "lossy",
				ignorable = false,
				retain = "last",
				data = object({ policy = {}, revision = integer }, { "policy", "revision" }, true),
				example = { policy = "ask", revision = 1 },
			},
			["review-result"] = {
				doc = "Automated approval reviewer outcome.",
				mode = "emit",
				expose = "both",
				delivery = "required",
				ignorable = false,
				data = object({ category = string, result = open_object }, { "category", "result" }, true),
				example = {
					category = "exec",
					result = { call = { tool_call_id = "c-1", name = "exec" }, decision = "allow", source = "reviewer" },
				},
			},
		},
	},
	["model"] = {
		owner = "llm",
		doc = "Active model selection change events.",
		events = {
			["changed"] = {
				doc = "Active model selection changed.",
				mode = "emit",
				expose = "both",
				delivery = "lossy",
				ignorable = false,
				retain = "last",
				data = object(
					{ provider_id = string, model_id = string, ref = string },
					{ "provider_id", "model_id", "ref" },
					true
				),
				example = { ref = "openai/gpt-5", provider_id = "openai", model_id = "gpt-5" },
			},
		},
	},
	["input"] = {
		owner = "agent",
		doc = "Prompt editor and queued-input UI events.",
		events = {
			["complete"] = { doc = "Prompt input completion query.", mode = "emit", expose = "lua", data = open_object },
			["follow-up"] = { doc = "Queue input as follow-up.", mode = "emit", expose = "lua", data = open_object },
			["queue-cancel"] = { doc = "Cancel active queue edit.", mode = "emit", expose = "lua" },
			["queue-edit"] = { doc = "Edit selected queued input.", mode = "emit", expose = "lua", data = open_object },
			["queue-menu"] = { doc = "Open queue action menu.", mode = "emit", expose = "lua" },
			["queue-move"] = { doc = "Reorder queued input.", mode = "emit", expose = "lua", data = open_object },
			["queue-remove"] = { doc = "Remove queued input.", mode = "emit", expose = "lua", data = open_object },
			["queue-select"] = { doc = "Select queued input.", mode = "emit", expose = "lua", data = open_object },
			["steer"] = { doc = "Submit steering input.", mode = "emit", expose = "lua", data = open_object },
			["submit"] = {
				doc = "Submit prompt input to the active UI or agent.",
				mode = "emit",
				expose = "lua",
				emitters = { "commands", "mcp" },
				data = object({
					text = string,
					images = { type = "array", items = open_object },
				}, { "text" }),
			},
			["transform"] = {
				doc = "Rewrite or expand user input message content before submission.",
				mode = "waterfall",
				expose = "lua",
				data = open_object,
				result = open_object,
			},
		},
	},
	["login"] = {
		owner = "llm",
		doc = "CLI login flow dispatch events.",
		events = {
			["cli"] = {
				doc = "CLI subcommand request for interactive login.",
				mode = "emit",
				expose = "process",
				data = open_object,
			},
		},
	},
	["option"] = {
		owner = "kernel",
		doc = "Option transaction apply, current-value, and change notification events.",
		events = {
			["apply"] = {
				doc = "Apply a staged option update inside a config transaction.",
				mode = "emit",
				expose = "process",
				scope = "host",
				data = open_object,
			},
			["changed"] = {
				doc = "A committed runtime option value changed.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				retain = "last",
				data = object({ name = string, value = {} }, { "name" }, true),
			},
			["current"] = {
				doc = "Read the current live value of an option inside a config transaction.",
				mode = "bail",
				expose = "process",
				scope = "host",
				data = open_object,
			},
		},
	},
	["app"] = {
		owner = "kernel",
		doc = "Host application lifecycle events.",
		events = {
			["exit-requested"] = {
				doc = "User requested exit while an exit guard is active.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				data = open_object,
			},
			["ready"] = {
				doc = "Host finished startup and entered interactive or headless ready state.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				data = open_object,
			},
			["shutdown"] = {
				doc = "Host is shutting down; listeners run before teardown completes.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				data = open_object,
			},
		},
	},
	["key"] = {
		owner = "kernel",
		doc = "Terminal interrupt key events.",
		events = {
			["interrupt"] = {
				doc = "User pressed the interrupt key (Esc / Ctrl+C) in the interactive frontend.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				data = object({ editor_empty = S.boolean }, nil, true),
			},
		},
	},
	["reload"] = {
		owner = "kernel",
		doc = "Full host reload lifecycle events.",
		events = {
			["completed"] = {
				doc = "Full `/reload` pass completed with added, removed, replaced, reloaded, and notices.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				data = object({
					added = { type = "array", items = string },
					removed = { type = "array", items = string },
					replaced = { type = "array", items = string },
					reloaded = { type = "array", items = string },
					notices = { type = "array", items = open_object },
				}, nil, true),
			},
		},
	},
	["trust"] = {
		owner = "kernel",
		doc = "Workspace trust decision and change events.",
		events = {
			["changed"] = {
				doc = "Workspace trust state changed.",
				mode = "emit",
				expose = "lua",
				scope = "host",
				retain = "last",
				data = object(
					{ trusted = S.boolean, saved = S.boolean, source = string },
					{ "trusted", "saved" },
					true
				),
			},
			["resolve"] = {
				doc = "Resolve workspace trust policy before falling back to trust_default or interactive prompt.",
				mode = "waterfall",
				expose = "lua",
				scope = "host",
				data = object({
					cwd = string,
					resources = open_object,
					saved = { oneOf = { S.boolean, { type = "null" } } },
					default = string,
				}, { "cwd", "default" }, true),
				result = object({ decision = S.boolean }, nil, true),
			},
		},
	},
	["peer"] = {
		owner = "session",
		doc = "Peer RPC delivery and hold notifications.",
		events = {
			["held"] = {
				doc = "Inbound peer message was held for review.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
			["inbound"] = {
				doc = "Decide handling of an inbound peer message.",
				mode = "bail",
				expose = "process",
				data = open_object,
			},
			["message"] = { doc = "Inbound peer message delivered.", mode = "emit", expose = "lua", data = open_object },
			["rejected"] = {
				doc = "Inbound peer message was rejected.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
			["released"] = {
				doc = "Held peer message was released.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
		},
	},
	["pipeline"] = {
		owner = "approvals",
		doc = "Tool approval pipeline lifecycle events.",
		events = {
			["approval-decided"] = {
				doc = "Tool pipeline approval decision resolved.",
				mode = "emit",
				expose = "process",
				emitters = { "tools" },
				data = object({ call = open_object, child_approval_id = string }, nil, true),
			},
			["approval-reviewed"] = {
				doc = "Automated reviewer outcome recorded inside the approval pipeline.",
				mode = "emit",
				expose = "process",
				data = object(
					{ category = string, snapshot = open_object, result = open_object },
					{ "category", "result" }
				),
			},
			["tool-dispatching"] = {
				doc = "A tool call cleared pipeline checks and is starting execution.",
				mode = "emit",
				expose = "process",
				emitters = { "agent" },
				data = object({ call = open_object, info = open_object }, { "call", "info" }),
			},
		},
	},
	["profile"] = {
		owner = "agent",
		doc = "Profiler instrumentation events.",
		events = {
			["tools-changed"] = {
				doc = "Declared tool set sent to the model changed.",
				mode = "emit",
				expose = "lua",
				emitters = { "profile" },
				retain = "last",
				data = open_object,
			},
		},
	},
	["projection"] = {
		owner = "tools",
		doc = "Argument/secret projection rules updated.",
		events = {
			["changed"] = {
				doc = "Registered projection secret rules changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = object({ revision = integer }, { "revision" }),
			},
		},
	},
	["provider"] = {
		owner = "llm",
		doc = "Model provider transport and stream events.",
		events = {
			["event"] = {
				doc = "Rewrite or inspect a parsed streaming provider event.",
				mode = "waterfall",
				expose = "process",
				emitters = { "llm", "providers", "provider-*" },
				data = object({ event = open_object, provider = string }, { "event" }, true),
				result = object({ event = open_object }, nil, true),
			},
			["request"] = {
				doc = "Rewrite wire HTTP request parameters before dispatch.",
				mode = "waterfall",
				expose = "process",
				emitters = { "llm", "providers", "provider-*" },
				data = open_object,
				result = open_object,
			},
			["transport"] = {
				doc = "Custom transport override before HTTP/WebSocket dispatch.",
				mode = "bail",
				expose = "process",
				emitters = { "llm", "providers", "provider-*" },
				data = object({
					provider = string,
					transport = string,
					request = open_object,
					cancel = open_object,
				}, { "provider", "request" }, true),
			},
		},
	},
	["repeat-guard"] = {
		owner = "agent",
		doc = "Repeated tool-call detector events.",
		events = {
			["reminder"] = {
				doc = "Emitted when identical tool calls repeat and trigger a reminder.",
				mode = "emit",
				expose = "lua",
				emitters = { "repeat-guard" },
				data = object({ tool = string, count = integer }, { "tool", "count" }),
			},
		},
	},
	["conversation"] = {
		owner = "conversations",
		doc = "Ephemeral side-conversation lifecycle and stream events.",
		events = {
			["changed"] = {
				doc = "Side conversation selection or parent state changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = open_object,
			},
			["closed"] = {
				doc = "Active side conversation closed and discarded.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
			["opened"] = {
				doc = "Ephemeral side conversation opened.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
			["settled"] = {
				doc = "A side conversation turn finished.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
			["stream"] = {
				doc = "Side conversation streaming delta/tool event.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
		},
	},
	["editor"] = {
		owner = "ui",
		doc = "Interactive editor buffer state change events.",
		events = {
			["changed"] = {
				doc = "Editor buffer text, cursor, selection, or revision changed.",
				mode = "emit",
				expose = "process",
				retain = "last",
				data = open_object,
			},
		},
	},
	["workflow"] = {
		owner = "workflow",
		doc = "Multi-agent workflow lifecycle and progress events.",
		events = {
			["agent-completed"] = {
				doc = "One workflow member finished.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				data = object({
					run_id = string,
					member_id = { type = "string", description = "Workflow member identifier (`run_id#seq`)." },
					total = integer,
				}, nil, true),
			},
			["agent-started"] = {
				doc = "One workflow member started.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				data = object({
					run_id = string,
					member_id = { type = "string", description = "Workflow member identifier (`run_id#seq`)." },
					total = integer,
				}, nil, true),
			},
			["changed"] = {
				doc = "Workflow run state changed.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				retain = "last",
				data = open_object,
			},
			["completed"] = {
				doc = "A workflow run completed.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				data = object({ run_id = string }, nil, true),
			},
			["log"] = {
				doc = "Workflow run progress log entry.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				data = object({ run_id = string, message = string }, nil, true),
			},
			["phase"] = {
				doc = "Workflow run entered a phase.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				data = object({ run_id = string, phase = string }, nil, true),
			},
			["started"] = {
				doc = "A workflow run started.",
				mode = "emit",
				expose = "lua",
				emitters = { "subagent" },
				data = object({
					run_id = string,
					name = string,
					total = integer,
					parent_session_id = string,
					parent_session_epoch = string,
				}, nil, true),
			},
		},
	},
	["sandbox"] = {
		owner = "session",
		doc = "Session sandbox policy change events.",
		events = {
			["changed"] = {
				doc = "Session sandbox policy changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = object({ policy = open_object }, nil, true),
			},
		},
	},
	["working-directory"] = {
		owner = "tools",
		doc = "Shell working directory tracking events.",
		events = {
			["changed"] = {
				doc = "Persistent bash shell working directory changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = object({ from = string, to = string, reason = string }, { "from", "to" }, true),
			},
		},
	},
}
