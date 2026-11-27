-- compact.stages.prompts: the summarizer's system prompt and instruction texts;
-- part of the lazy `compact.stages` module (lua/compact/stages.lua),
-- loaded at the start of its factory, i.e. on the first compaction.
local SYSTEM = "You are a context summarization assistant. Your task is to read a conversation between a user "
	.. "and an AI assistant, then produce a structured summary following the exact format specified. "
	.. "Do NOT continue the conversation. Do NOT respond to any questions in the conversation. Do NOT call tools. "
	.. "ONLY output the structured summary."

local FORMAT = [[## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Open Questions
- [User questions not yet answered, or "(none)"]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.]]

local SUMMARIZE = "The messages above are a conversation to summarize. Create a structured context checkpoint "
	.. "summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n"
	.. FORMAT

local UPDATE = [[The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Merge them into ONE updated structured summary (do not just append). RULES:
- PRESERVE all existing information from the previous summary that is still relevant
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages

Use this EXACT format:

]] .. FORMAT

local TURN_PREFIX =
	[[The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages.]]

local REMOTE_PLACEHOLDER = "The earlier conversation was compacted by the model provider's server; its "
	.. "content is only readable by that provider's models."

return {
	SYSTEM = SYSTEM,
	SUMMARIZE = SUMMARIZE,
	UPDATE = UPDATE,
	TURN_PREFIX = TURN_PREFIX,
	REMOTE_PLACEHOLDER = REMOTE_PLACEHOLDER,
}
