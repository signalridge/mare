---@meta _
-- GENERATED from mare-protocol event registry and built-in service contracts: do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.EventEnvelope
---@field type string
---@field session_id? string
---@field session_epoch string
---@field seq? integer
---@field turn_id? string
---@field next? fun(data?: any): any

---@class mare.OnOptions
---@field prepend? boolean
---@field global? boolean

--- An agent activity provider was registered or removed.
--- `agent/activity-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.agent__activity_changed
---@field kind "registered"|"removed"
---@field namespace string

--- Interactive parent approval dialog request for a child subagent action.
--- `agent/approval-dialog` (`mode = bail`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__approval_dialog
---@field call? table<string, any>
---@field info? table<string, any>
---@field rule? table<string, any>
---@field verdict? table<string, any>

--- Pending subagent approval queue updated.
--- `agent/approval-queue` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__approval_queue
---@field pending table<string, any>[]

--- A child subagent approval prompt was opened on the parent.
--- `agent/child-approval` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__child_approval
---@field origin? table<string, any>
---@field request_id? string|integer

--- An agent turn commit point was queued or started.
--- `agent/commit` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__commit
---@field kind "queued"|"started"
---@field turn_id string

--- Allow an active UI or side-conversation mode to intercept agent frontend operations.
--- `agent/frontend-route` (`mode = bail`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__frontend_route
---@field op "prompt"|"continue"|"steer"|"follow_up"|"cancel"|"wait"|"busy"|"state"|"get"
---@field opts? table<string, any>
---@field text? string

--- Policy admission gate checked before accepting direct or queued input into a turn.
--- `agent/input-ready` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__input_ready
---@field attempt_id string
---@field cancel mare.Token
---@field content table<string, any>[]
---@field delivery "prompt"|"steer"|"follow_up"
---@field input_context? any
---@field input_id string
---@field revision integer
---@field session {session_epoch: string, session_id: string}
---@field source? string
---@field turn_id string

--- The active conversation message array was replaced.
--- `agent/messages-replaced` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__messages_replaced
---@field messages table<string, any>[]

--- An interrupted request was queued for automatic recovery.
--- `agent/request-resumed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__request_resumed
---@field attempt integer
---@field clean? boolean
---@field request_id string

--- Agent settled after a turn ended (busy is false once draining finishes).
--- `agent/settled` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__settled
---@field busy boolean
---@field turn_id? string

--- A model step was durably committed to session storage.
--- `agent/step-committed` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__step_committed
---@field step table<string, any>
---@field step_id string
---@field turn_id string

--- Policy gate checked before admitting each agent step.
--- `agent/step-ready` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.agent__step_ready table

--- Decide whether an idle agent turn should continue instead of stopping. Return a non-empty string or `{ message = string, source? = string }`; when `source` is provided, the continuation message is marked `notification = true` with that `source`.
--- `agent/turn-stopping` (`mode = bail`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.agent__turn_stopping
---@field cancel? table<string, any>
---@field message? table<string, any>
---@field turn integer
---@field turn_id string

--- User requested exit while an exit guard is active.
--- `app/exit-requested` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.app__exit_requested table<string, any>

--- Host finished startup and entered interactive or headless ready state.
--- `app/ready` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.app__ready table<string, any>

--- Host is shutting down; listeners run before teardown completes.
--- `app/shutdown` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.app__shutdown table<string, any>

--- Approval policy or revision changed.
--- `approval/changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.approval__changed
---@field policy any
---@field revision integer

--- Approval prompt opened or requested for an action.
--- `approval/requested` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.approval__requested
---@field approval_id string
---@field category string
---@field tool_call_id? string|nil
---@field snapshot? any
---@field child_approval_id? integer|nil
---@field truncated? boolean|nil

--- Approval prompt resolved with its decision outcome.
--- `approval/resolved` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.approval__resolved
---@field approval_id string
---@field category string
---@field tool_call_id? string|nil
---@field result {call: {tool_call_id: string, name: string}, decision: "allow"|"allow_session"|"allow_amend"|"deny"|"abort"|"unavailable", source?: ("user"|"policy"|"reviewer"|"rule"|"approval_policy"|"cancelled"|"error"|"none"|"hook"|"host")|nil, outcome?: ("timeout"|"cancelled")|nil, child_approval_id?: integer|nil, scope?: ("turn"|"session")|nil}
---@field truncated? boolean|nil

--- Automated approval reviewer outcome.
--- `approval/review-result` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.approval__review_result
---@field category string
---@field result table<string, any>

--- Workspace file changes were rewound to an earlier turn.
--- `changes/rewound` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.changes__rewound
---@field conflicts? string[]
---@field failed? table<string, any>[]
---@field missing? string[]
---@field refused? string[]
---@field restored? string[]
---@field turn string

--- Context token usage crossed the near-threshold warning watermark.
--- `compact/approaching` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.compact__approaching
---@field threshold integer|number
---@field tokens integer|number

--- Pre-compaction bail hook before starting a compaction pass.
--- `compact/before` (`mode = bail`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.compact__before
---@field reason string
---@field threshold? integer|number
---@field tokens integer|number

--- Context compaction completed (succeeded, cancelled, failed, or stalled).
--- `compact/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.compact__completed
---@field cancelled? boolean|nil
---@field changed boolean
---@field compact_id? string
---@field compactions? integer|nil
---@field error? {code: string, data?: any, message: string}
---@field message? string|nil
---@field offloaded_count? integer|nil
---@field pruned_count? integer|nil
---@field reason string
---@field stalled? boolean|nil
---@field status "completed"|"cancelled"|"failed"|"stalled"
---@field tokens_after number
---@field tokens_before number

--- Compaction became idle after clearing or settling state.
--- `compact/idle` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.compact__idle table

--- Request overflow compaction recovery after a context-overflow response.
--- `compact/overflow` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.compact__overflow table<string, any>

--- Best-effort compaction pipeline stage diagnostic.
--- `compact/stage` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = true`, `retain = false`, `on_error = continue`)
---@class mare.event.compact__stage
---@field changed? boolean
---@field phase string
---@field reason? string
---@field stage string
---@field tokens? number
---@field tokens_after? number
---@field tokens_before? number

--- Context compaction started.
--- `compact/started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.compact__started
---@field compact_id? string
---@field messages? integer
---@field reason string

--- Compaction summarization failed; listeners may supply an alternate summary.
--- `compact/summary-error` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.compact__summary_error
---@field error string
---@field kind string
---@field messages table<string, any>[]

--- Context breakdown report built for inspection.
--- `context/report` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.context__report table<string, any>

--- Post-process the assembled system prompt text.
--- `context/system` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.context__system
---@field model? table<string, any>
---@field text string

--- Transform outbound message history before each LLM request.
--- `context/transform` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.context__transform
---@field messages table<string, any>[]
---@field model? table<string, any>

--- Side conversation selection or parent state changed.
--- `conversation/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.conversation__changed table<string, any>

--- Active side conversation closed and discarded.
--- `conversation/closed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.conversation__closed table<string, any>

--- Ephemeral side conversation opened.
--- `conversation/opened` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.conversation__opened table<string, any>

--- A side conversation turn finished.
--- `conversation/settled` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.conversation__settled table<string, any>

--- Side conversation streaming delta/tool event.
--- `conversation/stream` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.conversation__stream table<string, any>

--- Clipboard or file copy completed.
--- `copy/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.copy__completed
---@field bytes integer
---@field method? string
---@field target string
---@field verified? boolean

--- Request the copy service to copy text to the clipboard or output.
--- `copy/request` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.copy__request
---@field from? string
---@field kind? string
---@field text string

--- Editor buffer text, cursor, selection, or revision changed.
--- `editor/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.editor__changed table<string, any>

--- A file edit tool could not establish exact diff line bounds.
--- `fs/changes-inexact` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.fs__changes_inexact
---@field paths string[]
---@field tool string

--- Policy gate evaluated before editing an existing workspace file.
--- `fs/edit-intent` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.fs__edit_intent table

--- A file path was observed (read or stat recorded).
--- `fs/observed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.fs__observed
---@field observation table<string, any>
---@field path string

--- Internal watch delta emitted to the owning RPC connection (forwarded as `fs/changed`).
--- `fs/watch-event` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.fs__watch_event
---@field changed_paths string[]
---@field owner integer
---@field truncated? boolean
---@field watch_id string

--- Policy gate evaluated before writing a workspace file.
--- `fs/write-intent` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.fs__write_intent table

--- Informational or warning notice from the agent or host.
--- `host/notice` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = true`, `retain = false`, `on_error = continue`)
---@class mare.event.host__notice
---@field level string
---@field text string
---@field truncated? boolean|nil

--- Prompt input completion query.
--- `input/complete` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__complete table<string, any>

--- Queue input as follow-up.
--- `input/follow-up` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__follow_up table<string, any>

--- Cancel active queue edit.
--- `input/queue-cancel` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__queue_cancel table

--- Edit selected queued input.
--- `input/queue-edit` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__queue_edit table<string, any>

--- Open queue action menu.
--- `input/queue-menu` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__queue_menu table

--- Reorder queued input.
--- `input/queue-move` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__queue_move table<string, any>

--- Remove queued input.
--- `input/queue-remove` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__queue_remove table<string, any>

--- Select queued input.
--- `input/queue-select` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__queue_select table<string, any>

--- Submit steering input.
--- `input/steer` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__steer table<string, any>

--- Submit prompt input to the active UI or agent.
--- `input/submit` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.input__submit
---@field images? table<string, any>[]
---@field text string

--- Rewrite or expand user input message content before submission.
--- `input/transform` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.input__transform table<string, any>

--- Interactive prompt opened and waiting for a responder.
--- `interaction/requested` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.interaction__requested
---@field interaction_id string
---@field kind "select"|"input"|"confirm"|"questions"|"editor"
---@field title? string|nil
---@field session_id? string|nil
---@field truncated? boolean|nil

--- Interactive prompt finished with answered, cancelled, or unavailable.
--- `interaction/resolved` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.interaction__resolved
---@field interaction_id string
---@field kind "select"|"input"|"confirm"|"questions"|"editor"
---@field outcome "answered"|"cancelled"|"unavailable"
---@field title? string|nil
---@field session_id? string|nil
---@field truncated? boolean|nil

--- Authoritative completed item. For `tool_call`, `item.message` is the sole carrier of `content`/`details`. Oversized wire payloads project images to `{ type: "image", mime, path?, bytes, truncated: true }`, truncate long strings with `truncated: true`, or fall back to `item_omitted: true`; fetch full content via `session.get_item`.
--- `item/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.item__completed
---@field turn_id? string|nil
---@field item_id string
---@field status "pending"|"running"|"completed"|"failed"|"denied"|"cancelled"|"persist_failed"
---@field item? (({type: string, entry_id?: string|nil, message?: table<string, any>|nil, content?: any, truncated?: boolean|nil, [string]: any})|({type: string, entry_id?: string|nil, message?: table<string, any>|nil, truncated?: boolean|nil, [string]: any})|({type: string, tool_call_id: string, name: string, provider_call_id?: string|nil, arguments?: any, status: "pending"|"running"|"completed"|"failed"|"denied"|"cancelled"|"persist_failed", entry_id?: string|nil, result_entry_id?: string|nil, result?: table|nil, message?: ({role: "tool_result", tool_call_id: string, content: ({type: any, text: string, truncated?: boolean}|{type: any, mime: string, data: string, path?: string, bytes?: integer, truncated?: any}|{type: any, mime: string, path?: string, bytes: integer, truncated: any}|{type: string})[], [string]: any})|nil, result_status?: ("success"|"error"|"denied"|"cancelled"|"persist_failed")|nil, execution_status?: "unknown"|nil, info?: table<string, any>|nil, truncated?: boolean|nil, [string]: any})|({type: string, entry_id?: string|nil, kind?: string|nil, message?: table<string, any>|nil, data?: any, entry?: table<string, any>|nil, truncated?: boolean|nil, [string]: any})|({type: string, entry_id?: string|nil, reason?: string|nil, tokens_before?: number|nil, tokens_after?: number|nil, pruned_count?: integer|nil, offloaded_count?: integer|nil, changed?: boolean|nil, summary?: any, stalled?: boolean|nil, error?: {code: string, message: string, data?: any}|nil, cancelled?: boolean|nil, truncated?: boolean|nil, [string]: any}))|nil
---@field item_omitted? boolean|nil

--- Append-only incremental delta for an in-flight item.
--- `item/delta` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = true`, `retain = false`, `on_error = continue`)
---@class mare.event.item__delta
---@field turn_id string
---@field item_id string
---@field kind "text"|"reasoning"|"tool_arguments"|"tool_output"|"event"
---@field delta string
---@field offset integer
---@field tool_call_id? string|nil
---@field partial? any
---@field event? any
---@field truncated? boolean|nil

--- Semantic item (message, tool call, custom record, or compaction) started. Oversized wire payloads are bounded at the outbound tap (`truncated: true` on truncated items/blocks; fetch full content via `session.get_item`).
--- `item/started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.item__started
---@field turn_id? string|nil
---@field item_id string
---@field step_id? string|nil
---@field item ({type: string, entry_id?: string|nil, message?: table<string, any>|nil, content?: any, truncated?: boolean|nil, [string]: any})|({type: string, entry_id?: string|nil, message?: table<string, any>|nil, truncated?: boolean|nil, [string]: any})|({type: string, tool_call_id: string, name: string, provider_call_id?: string|nil, arguments?: any, status: "pending"|"running"|"completed"|"failed"|"denied"|"cancelled"|"persist_failed", entry_id?: string|nil, result_entry_id?: string|nil, result?: table|nil, message?: ({role: "tool_result", tool_call_id: string, content: ({type: any, text: string, truncated?: boolean}|{type: any, mime: string, data: string, path?: string, bytes?: integer, truncated?: any}|{type: any, mime: string, path?: string, bytes: integer, truncated: any}|{type: string})[], [string]: any})|nil, result_status?: ("success"|"error"|"denied"|"cancelled"|"persist_failed")|nil, execution_status?: "unknown"|nil, info?: table<string, any>|nil, truncated?: boolean|nil, [string]: any})|({type: string, entry_id?: string|nil, kind?: string|nil, message?: table<string, any>|nil, data?: any, entry?: table<string, any>|nil, truncated?: boolean|nil, [string]: any})|({type: string, entry_id?: string|nil, reason?: string|nil, tokens_before?: number|nil, tokens_after?: number|nil, pruned_count?: integer|nil, offloaded_count?: integer|nil, changed?: boolean|nil, summary?: any, stalled?: boolean|nil, error?: {code: string, message: string, data?: any}|nil, cancelled?: boolean|nil, truncated?: boolean|nil, [string]: any})

--- Background job state changed.
--- `jobs/changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.jobs__changed
---@field job {background?: boolean, description?: string, ended_at_ms?: integer, exit_code?: integer, exit_signal?: integer, job_id: string, kill_reason?: string, kill_requested?: boolean, kind: string, label?: string, over_limit?: integer, owner: string, started_at_ms: integer, status: "running"|"completed"|"failed"|"cancelled"|"interrupted", stop_unconfirmed?: boolean, [string]: any}
---@field truncated? boolean

--- Internal job state update carrying the live in-process job handle.
--- `jobs/internal-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.jobs__internal_changed
---@field job {background?: boolean, description?: string, ended_at_ms?: integer, exit_code?: integer, exit_signal?: integer, job_id: string, kill_reason?: string, kill_requested?: boolean, kind: string, label?: string, over_limit?: integer, owner: string, started_at_ms: integer, status: "running"|"completed"|"failed"|"cancelled"|"interrupted", stop_unconfirmed?: boolean, [string]: any}

--- Dispatched serially before plugins in `plugins` are disposed for a reload; listener errors land in the reload report.
--- `kernel/before-reload` (`mode = serial`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.kernel__before_reload
---@field plugins string[]

--- Diagnostic checks collected by `/doctor`; listeners append `{ name, ok, warn?, detail? }` entries.
--- `kernel/doctor` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.kernel__doctor table<string, any>

--- A plugin listener, task, or timer raised an error reported by the kernel.
--- `kernel/error` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.kernel__error
---@field error string
---@field event string
---@field message string
---@field plugin string
---@field what string

--- Plugin reload pass finished applying definitions and validating keymaps.
--- `kernel/reloaded` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.kernel__reloaded table<string, any>

--- A plugin has been loading, unloading or waiting for `stall_ms`: announced once each time it gets stuck in a state. Observation only, nothing is aborted.
--- `kernel/stalled` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.kernel__stalled
---@field effects string[]
---@field missing string[]
---@field ms integer
---@field plugin string
---@field state "LOADING"|"UNLOADING"|"PENDING"
---@field summary string

--- User pressed the interrupt key (Esc / Ctrl+C) in the interactive frontend.
--- `key/interrupt` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.key__interrupt
---@field editor_empty? boolean

--- A repeated crash fingerprint reached the safety threshold and was released.
--- `llm/crash-limit` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__crash_limit
---@field crashes integer
---@field fingerprint string

--- A model request fell back to another model after an error.
--- `llm/fallback` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__fallback
---@field error? string
---@field from? string
---@field reason string
---@field to string

--- Provider login succeeded.
--- `llm/login` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__login
---@field provider string

--- Provider credentials were removed on logout.
--- `llm/logout` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__logout
---@field provider string
---@field removed string[]

--- In-process model or effort selection changed (carries runtime model object).
--- `llm/model-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.llm__model_changed
---@field effort? string
---@field global_effort? string
---@field model? {available: boolean, context_window?: integer, cost?: {cache_read?: number, cache_write?: number, input?: number, output?: number}, default_effort?: string, edit_tool?: "apply_patch"|"replace", efforts: string[], images: boolean, max_output?: integer, model_id: string, name?: string, parameters: ({cycleable?: boolean, default?: any, name?: string, options: ({cost?: number|table, name?: string, tooltip?: string, value: any})[], parameter_id: string, statusline?: any, tooltip?: string, type?: string, values: any[], visible?: any})[], provider: string, ready: boolean, ready_reason?: string, reasoning: boolean, ref: string, route?: string, route_error?: string, tool_update?: "addition_only", unavailable_reason?: string, wire_route?: string}
---@field params_only? boolean
---@field scope? string
---@field scoped_only? boolean

--- Pre-commit admission gate before changing the session model.
--- `llm/model-changing` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = abort`)
---@alias mare.event.llm__model_changing table

--- A model parameter value was clamped or adjusted to satisfy constraints.
--- `llm/params-adjusted` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.llm__params_adjusted table<string, any>

--- A model parameter changed.
--- `llm/params-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.llm__params_changed
---@field model {available: boolean, context_window?: integer, cost?: {cache_read?: number, cache_write?: number, input?: number, output?: number}, default_effort?: string, edit_tool?: "apply_patch"|"replace", efforts: string[], images: boolean, max_output?: integer, model_id: string, name?: string, parameters: ({cycleable?: boolean, default?: any, name?: string, options: ({cost?: number|table, name?: string, tooltip?: string, value: any})[], parameter_id: string, statusline?: any, tooltip?: string, type?: string, values: any[], visible?: any})[], provider: string, ready: boolean, ready_reason?: string, reasoning: boolean, ref: string, route?: string, route_error?: string, tool_update?: "addition_only", unavailable_reason?: string, wire_route?: string}
---@field param string
---@field requested? any
---@field value? any

--- Registered providers or their model catalogs changed.
--- `llm/providers-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.llm__providers_changed table

--- Updated subscription quota / rate-limit headers arrived from a provider.
--- `llm/rate-limits` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__rate_limits
---@field limits {credits?: string, limit_name?: string, primary?: {reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}, reached?: string, received_at_ms: integer, secondary?: {reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}, [string]: any}
---@field provider string

--- A model request error triggered a retry or reset attempt.
--- `llm/retry` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__retry
---@field attempt integer
---@field delay_ms integer
---@field error? {code: string, data?: any, message: string}
---@field kind? string
---@field max_attempts? integer
---@field reset? boolean

--- Token usage reported from a completed model request.
--- `llm/usage` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.llm__usage
---@field leaf_entry_id? string
---@field model_ref string
---@field provider_id string
---@field purpose string
---@field request_id? string
---@field session_id? string
---@field subscription? table<string, any>
---@field usage table<string, any>

--- CLI subcommand request for interactive login.
--- `login/cli` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.login__cli table<string, any>

--- An MCP server's resources, prompts, or connection state changed.
--- `mcp/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.mcp__changed
---@field server string

--- Internal CLI /mcp subcommand dispatch.
--- `mcp/cli` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.mcp__cli table<string, any>

--- MCP server exposed code-mode tools for registration.
--- `mcp/codemode` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.mcp__codemode
---@field server string
---@field tools table<string, any>[]

--- MCP server requested interactive user elicitation.
--- `mcp/elicit` (`mode = bail`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.mcp__elicit
---@field server string

--- An MCP server failed or reported a connection/refresh error.
--- `mcp/error` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.mcp__error
---@field error string
---@field server string

--- An MCP server connected and exposed its direct tools.
--- `mcp/ready` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.mcp__ready
---@field server string
---@field tools string[]

--- Active model selection changed.
--- `model/changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.model__changed
---@field model_id string
---@field provider_id string
---@field ref string

--- Affirmative network access approval point.
--- `net/approval` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.net__approval table

--- Host-level affirmative network access approval point.
--- `net/approval-host` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.net__approval_host table

--- Apply a staged option update inside a config transaction.
--- `option/apply` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.option__apply table<string, any>

--- A committed runtime option value changed.
--- `option/changed` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.option__changed
---@field name string
---@field value? any

--- Read the current live value of an option inside a config transaction.
--- `option/current` (`mode = bail`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.option__current table<string, any>

--- Inbound peer message was held for review.
--- `peer/held` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.peer__held table<string, any>

--- Decide handling of an inbound peer message.
--- `peer/inbound` (`mode = bail`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.peer__inbound table<string, any>

--- Inbound peer message delivered.
--- `peer/message` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.peer__message table<string, any>

--- Inbound peer message was rejected.
--- `peer/rejected` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.peer__rejected table<string, any>

--- Held peer message was released.
--- `peer/released` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.peer__released table<string, any>

--- Active permission preset or policy state changed.
--- `permission/changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.permission__changed
---@field label? string
---@field partial? boolean
---@field preset? string

--- Tighten a parsed shell command AST before rule matching.
--- `permission/parse` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.permission__parse
---@field command? string
---@field parsed table<string, any>
---@field shell? string
---@field tool? string

--- Secondary policy check that may tighten a tool/before verdict and joins ask reasons.
--- `permission/verdict` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.permission__verdict table

--- Tool pipeline approval decision resolved.
--- `pipeline/approval-decided` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.pipeline__approval_decided
---@field call? table<string, any>
---@field child_approval_id? string

--- Automated reviewer outcome recorded inside the approval pipeline.
--- `pipeline/approval-reviewed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.pipeline__approval_reviewed
---@field category string
---@field result table<string, any>
---@field snapshot? table<string, any>

--- A tool call cleared pipeline checks and is starting execution.
--- `pipeline/tool-dispatching` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.pipeline__tool_dispatching
---@field call table<string, any>
---@field info table<string, any>

--- Package manager action finished.
--- `plugins/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.plugins__completed
---@field action string
---@field name? string
---@field status "completed"|"failed"

--- An optional package plugin was enabled or disabled.
--- `plugins/plugin` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.plugins__plugin
---@field enabled boolean
---@field name string

--- Declared tool set sent to the model changed.
--- `profile/tools-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.profile__tools_changed table<string, any>

--- Registered projection secret rules changed.
--- `projection/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.projection__changed
---@field revision integer

--- Rewrite or inspect a parsed streaming provider event.
--- `provider/event` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.provider__event
---@field event table<string, any>
---@field provider? string

--- Rewrite wire HTTP request parameters before dispatch.
--- `provider/request` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.provider__request table<string, any>

--- Custom transport override before HTTP/WebSocket dispatch.
--- `provider/transport` (`mode = bail`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.provider__transport
---@field cancel? table<string, any>
---@field provider string
---@field request table<string, any>
---@field transport? string

--- Registered query providers or their operations changed.
--- `query/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.query__changed table

--- Agent steer/follow-up queue snapshot changed.
--- `queue/changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.queue__changed
---@field items ({input_id?: string|nil, lane: "steer"|"follow_up", text?: string|nil, [string]: any})[]
---@field action? string|nil
---@field truncated? boolean|nil

--- Full `/reload` pass completed with added, removed, replaced, reloaded, and notices.
--- `reload/completed` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.reload__completed
---@field added? string[]
---@field notices? table<string, any>[]
---@field reloaded? string[]
---@field removed? string[]
---@field replaced? string[]

--- Emitted when identical tool calls repeat and trigger a reminder.
--- `repeat-guard/reminder` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.repeat_guard__reminder
---@field count integer
---@field tool string

--- Inspect or transform an outgoing model request before dispatch.
--- `request/build` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.request__build table<string, any>

--- Abortable preflight check before building the model request.
--- `request/prepare` (`mode = serial`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.request__prepare table<string, any>

--- Policy gate evaluated before dispatching an LLM provider request.
--- `request/ready` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.request__ready table

--- Automated code review run completed.
--- `review/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.review__completed
---@field error? string
---@field hint? string
---@field target? table<string, any>
---@field text string

--- Session sandbox policy changed.
--- `sandbox/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.sandbox__changed
---@field policy? table<string, any>

--- Policy gate evaluated before forking a session.
--- `session/before-fork` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.session__before_fork table

--- Policy gate evaluated before switching sessions.
--- `session/before-switch` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.session__before_switch table

--- Policy gate evaluated before navigating the session tree.
--- `session/before-tree` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.session__before_tree table

--- Active session created, resumed, or forked.
--- `session/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.session__changed
---@field path? string
---@field reason "resume"|"new"|"fork"
---@field session_id string

--- Emitted once per retired root session after flush and close complete (switch, shutdown, or reload).
--- `session/closed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__closed
---@field cwd string
---@field path? string
---@field reason string
---@field session_id string

--- Fail-closed policy gate before committing queued input.
--- `session/commit-ready` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.session__commit_ready table

--- Corrupted lines were skipped when reading a session file.
--- `session/damaged` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__damaged
---@field fork? boolean
---@field path string
---@field skipped integer

--- A saved session was permanently deleted.
--- `session/deleted` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__deleted
---@field path? string
---@field session_id string

--- Session contents were loaded or restored into memory.
--- `session/loaded` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__loaded
---@field entries? table<string, any>[]
---@field messages? table<string, any>[]
---@field name? string
---@field navigated? boolean
---@field path? string
---@field session_id? string
---@field shell_cwd? string

--- Policy gate evaluated before a session transition mutates state.
--- `session/mutation-ready` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.session__mutation_ready table

--- The active session was assigned a human-readable name.
--- `session/named` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__named
---@field name string

--- A fresh empty session was initialized.
--- `session/new` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__new
---@field path? string
---@field session_id string

--- A custom session record was committed.
--- `session/recorded` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__recorded
---@field entry table<string, any>

--- The last user message was re-submitted via /retry.
--- `session/retry` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__retry
---@field from string

--- Initial startup session was established or resumed.
--- `session/started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__started
---@field path? string
---@field resumed boolean
---@field session_id string

--- An exclusive session switch lock completed and released.
--- `session/switch-completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.session__switch_completed table

--- A session transition record was durably committed.
--- `session/transition-committed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__transition_committed
---@field has_initial? boolean
---@field mode string
---@field source? table<string, any>
---@field status string
---@field target? table<string, any>
---@field transition_id string

--- A session transition (and optional initial turn) finished.
--- `session/transition-completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__transition_completed
---@field initial_status? string
---@field mode? string
---@field recovered? boolean
---@field source? table<string, any>
---@field status string
---@field target? table<string, any>
---@field transition_id string
---@field turn_id? string

--- A session transition or its initial turn failed or went stale.
--- `session/transition-failed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.session__transition_failed
---@field error? string
---@field mode? string
---@field recovered? boolean
---@field source? table<string, any>
---@field status string
---@field target? table<string, any>
---@field transition_id string

--- Registered spill storage backends changed.
--- `spill/backends-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.spill__backends_changed
---@field kind string
---@field name string

--- Unified agent status changed.
--- `status/changed` (`mode = emit`, `delivery = lossy`, `scope = host`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.status__changed
---@field state string
---@field message string
---@field session table<string, any>
---@field seq integer
---@field truncated? boolean|nil

--- Process exit status snapshot published at clean shutdown.
--- `status/exit` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.status__exit
---@field cwd string
---@field message string
---@field model? string
---@field pid integer
---@field seq integer
---@field session {path?: string, resume?: string[], session_id?: string}
---@field state "idle"|"working"|"blocked"

--- Active session metadata inside status updated.
--- `status/session` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.status__session
---@field cwd string
---@field message string
---@field model? string
---@field pid integer
---@field seq integer
---@field session {path?: string, resume?: string[], session_id?: string}
---@field state "idle"|"working"|"blocked"

--- Model step finished (completed, failed, or retrying).
--- `step/completed` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.step__completed
---@field turn_id string
---@field step_id string
---@field status "completed"|"cancelled"|"failed"
---@field stop_reason? string|nil
---@field usage? any
---@field error? {code: string, message: string, data?: any}|nil
---@field error_kind? string|nil
---@field will_retry? boolean|nil
---@field attempt? integer|nil
---@field max_attempts? integer|nil
---@field delay_ms? number|nil
---@field reset? boolean|nil
---@field truncated? boolean|nil

--- Model step started within a turn.
--- `step/started` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.step__started
---@field turn_id string
---@field step_id string
---@field turn? integer|nil
---@field model? any
---@field attempt? integer|nil
---@field truncated? boolean|nil

--- Subagent run completed.
--- `subagent/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.subagent__completed
---@field subagent_id string
---@field agent_id? string|nil
---@field name? string|nil
---@field label? string|nil
---@field status string
---@field run_id? string|nil
---@field seq? integer|nil
---@field parent_session_id? string|nil
---@field parent_session_epoch? string|nil
---@field text? string|nil
---@field error? {code: string, message: string, data?: any}|nil
---@field error_kind? string|nil
---@field usage? any
---@field output_truncated? boolean|nil
---@field stop_reason? string|nil
---@field truncated? boolean|nil

--- Latest coalesced progress snapshot for a subagent.
--- `subagent/progress` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = true`, `retain = false`, `on_error = continue`)
---@class mare.event.subagent__progress
---@field subagent_id string
---@field parent_session_id? string|nil
---@field parent_session_epoch? string|nil
---@field progress table<string, any>
---@field truncated? boolean|nil

--- Subagent run started.
--- `subagent/started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.subagent__started
---@field subagent_id string
---@field agent_id? string|nil
---@field name? string|nil
---@field label? string|nil
---@field run_id? string|nil
---@field seq? integer|nil
---@field task? string|nil
---@field parent_session_id? string|nil
---@field parent_session_epoch? string|nil
---@field truncated? boolean|nil

--- Active theme or style layers changed.
--- `theme/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.theme__changed
---@field active string
---@field appearance "dark"|"light"
---@field component any
---@field dark string
---@field icons table<string, string>
---@field light string
---@field preference string
---@field preview? string
---@field revision integer
---@field styles table<string, table>
---@field syntax? string

--- Transform the resolved palette before deriving theme styles.
--- `theme/palette` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.theme__palette
---@field background? string
---@field flavor? string
---@field palette table<string, any>

--- Inspect or rewrite a tool display result after execution.
--- `tool/after` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.tool__after
---@field call table<string, any>
---@field operation? table<string, any>
---@field result table<string, any>

--- Tri-state tool approval decision point.
--- `tool/approval` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.tool__approval table

--- Host-level tri-state tool approval decision point.
--- `tool/approval-host` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.tool__approval_host table

--- A tool call received committed execution permissions.
--- `tool/authorized` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.tool__authorized
---@field call table<string, any>
---@field permissions? table<string, any>
---@field scope? string

--- Policy gate evaluated on frozen tool call arguments before execution.
--- `tool/before` (`mode = policy`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.tool__before table

--- Transform the validated tool program result before model/display projection.
--- `tool/program-result` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.tool__program_result
---@field call table<string, any>
---@field operation? table<string, any>
---@field result table<string, any>

--- Checked post-processing waterfall on tool display output before truncation.
--- `tool/result` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.tool__result
---@field call table<string, any>
---@field operation? table<string, any>
---@field result table<string, any>

--- Strict sequential rewrite chain run on tool arguments before freezing for policy.
--- `tool/rewrite` (`mode = rewrite`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.tool__rewrite table

--- An approval snapshot became void and will retry from rewrite.
--- `tool/snapshot-void` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.tool__snapshot_void
---@field call table<string, any>
---@field reason string

--- Registered tool executor backends changed.
--- `tools/executors-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.tools__executors_changed
---@field api_version? integer
---@field executor_id string
---@field kind "registered"|"removed"

--- Deferred tools were dynamically loaded via tool_search.
--- `tools/loaded` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.tools__loaded
---@field tools string[]

--- Workspace trust state changed.
--- `trust/changed` (`mode = emit`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.trust__changed
---@field saved boolean
---@field source? string
---@field trusted boolean

--- Resolve workspace trust policy before falling back to trust_default or interactive prompt.
--- `trust/resolve` (`mode = waterfall`, `delivery = required`, `scope = host`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.trust__resolve
---@field cwd string
---@field default string
---@field resources? table<string, any>
---@field saved? boolean|nil

--- Agent turn completed after run-end durability confirmation.
--- `turn/completed` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.turn__completed
---@field turn_id string
---@field status "completed"|"cancelled"|"failed"
---@field termination_reason? ("completed"|"usage_limit"|"interrupted"|"turn_limit"|"error")|nil
---@field cancellation_cause? ("user"|"parent"|"hook"|"disposed")|nil
---@field text? string|nil
---@field structured? any
---@field usage? table<string, any>|nil
---@field error? {code: string, message: string, data?: any}|nil
---@field error_kind? string|nil
---@field not_sent? boolean|nil
---@field output_truncated? boolean|nil
---@field stop_reason? string|nil
---@field truncated? boolean|nil

--- Agent turn started.
--- `turn/started` (`mode = emit`, `delivery = required`, `scope = turn`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.turn__started
---@field turn_id string
---@field trigger string
---@field input_item_ids? string[]|nil
---@field text? string|nil
---@field source? string|nil
---@field truncated? boolean|nil

--- Detected terminal background polarity (dark/light).
--- `ui/background` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__background
---@field appearance? string

--- Transform detected terminal UI capabilities.
--- `ui/caps` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.ui__caps table<string, any>

--- Detected terminal UI capabilities changed.
--- `ui/caps-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.ui__caps_changed table<string, any>

--- UI card emitted via ui.card (persistent cards are also written to session records).
--- `ui/card` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__card
---@field after_entry_id? string
---@field block_id? integer
---@field card_id? string
---@field kind string
---@field markdown string
---@field meta? any
---@field persistent boolean
---@field title string

--- Lines committed into terminal scrollback.
--- `ui/commit-after` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__commit_after
---@field lines string[]

--- Rewrite or append lines before they are committed into terminal scrollback.
--- `ui/commit-before` (`mode = waterfall`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__commit_before
---@field lines string[]

--- Captured external process output lines written to the terminal.
--- `ui/external-output` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__external_output
---@field lines string[]

--- Runtime keymap bindings changed (set, delete, or detach cleanup).
--- `ui/keymap-changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.ui__keymap_changed
---@field action? string|boolean
---@field context? string
---@field key? string
---@field session_id? string

--- Caller-owned Markdown document presentation updated asynchronously.
--- `ui/markdown-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.ui__markdown_changed
---@field owner integer
---@field presentation table<string, any>

--- Request owned markdown document recomputation after a display change.
--- `ui/presentation-invalidate` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.ui__presentation_invalidate table<string, any>

--- Async markdown presenter block render completed or failed.
--- `ui/presentation-render-completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__presentation_render_completed
---@field accepted boolean
---@field block_id string
---@field document_id string
---@field render_seq? integer
---@field request_id? string
---@field revision? integer

--- Registered markdown presenter changed.
--- `ui/presenter-changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.ui__presenter_changed
---@field action "registered"|"removed"
---@field language string
---@field registration string

--- Ephemeral UI print output outside the durable session log.
--- `ui/print` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = true`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__print
---@field lines? string[]
---@field text string

--- Interactive UI completed initial mount and first frame.
--- `ui/ready` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.ui__ready table

--- Terminal transcript reflow requested.
--- `ui/reflow` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.ui__reflow table

--- Terminal viewport resized.
--- `ui/resize` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__resize
---@field height? integer
---@field width? integer

--- Interactive UI resumed after suspend.
--- `ui/resume` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.ui__resume table

--- Interactive UI prompt or approval wait completed.
--- `ui/wait-completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__wait_completed
---@field kind? string
---@field title? string
---@field wait_id? integer

--- Interactive UI prompt or approval wait started.
--- `ui/wait-started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.ui__wait_started
---@field kind string
---@field title? string
---@field wait_id? integer

--- A newer release version was found by update-check.
--- `update-check/available` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@alias mare.event.update_check__available table<string, any>

--- Session token usage and cost totals changed.
--- `usage/changed` (`mode = emit`, `delivery = lossy`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.usage__changed
---@field cache_read integer
---@field cache_write integer
---@field cache_write_1h? integer
---@field cost number
---@field cost_unknown? boolean
---@field input integer
---@field last_cache_read integer
---@field last_context integer
---@field output integer
---@field reasoning? integer
---@field requests integer
---@field side_requests integer
---@field subscription_requests integer

--- Registered web search or fetch providers changed.
--- `web/providers-changed` (`mode = emit`, `delivery = lossy`, `scope = host`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.web__providers_changed
---@field action? string
---@field kind string
---@field provider_id? string

--- One workflow member finished.
--- `workflow/agent-completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.workflow__agent_completed
---@field member_id? string
---@field run_id? string
---@field total? integer

--- One workflow member started.
--- `workflow/agent-started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.workflow__agent_started
---@field member_id? string
---@field run_id? string
---@field total? integer

--- Workflow run state changed.
--- `workflow/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@alias mare.event.workflow__changed table<string, any>

--- A workflow run completed.
--- `workflow/completed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.workflow__completed
---@field run_id? string

--- Workflow run progress log entry.
--- `workflow/log` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.workflow__log
---@field message? string
---@field run_id? string

--- Workflow run entered a phase.
--- `workflow/phase` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.workflow__phase
---@field phase? string
---@field run_id? string

--- A workflow run started.
--- `workflow/started` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = false`, `on_error = continue`)
---@class mare.event.workflow__started
---@field name? string
---@field parent_session_epoch? string
---@field parent_session_id? string
---@field run_id? string
---@field total? integer

--- Persistent bash shell working directory changed.
--- `working-directory/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.working_directory__changed
---@field from string
---@field reason? string
---@field to string

--- Allowed workspace directory roots changed.
--- `workspace/changed` (`mode = emit`, `delivery = required`, `scope = session`, `ignorable = false`, `retain = true`, `on_error = continue`)
---@class mare.event.workspace__changed
---@field dirs string[]

---@class mare.Ctx
local Ctx = {}
---@overload fun(self: mare.Ctx, name: "agent/activity-changed", listener: (fun(data: mare.event.agent__activity_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/approval-dialog", listener: (fun(data: mare.event.agent__approval_dialog, ev: mare.EventEnvelope): ({choice: "allow"|"allow_session"|"allow_amend"|"deny"|"abort", reason?: string, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/approval-queue", listener: (fun(data: mare.event.agent__approval_queue, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/child-approval", listener: (fun(data: mare.event.agent__child_approval, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/commit", listener: (fun(data: mare.event.agent__commit, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/frontend-route", listener: (fun(data: mare.event.agent__frontend_route, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/messages-replaced", listener: (fun(data: mare.event.agent__messages_replaced, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/request-resumed", listener: (fun(data: mare.event.agent__request_resumed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/settled", listener: (fun(data: mare.event.agent__settled, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/step-committed", listener: (fun(data: mare.event.agent__step_committed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "agent/turn-stopping", listener: (fun(data: mare.event.agent__turn_stopping, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "app/exit-requested", listener: (fun(data: mare.event.app__exit_requested, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "app/ready", listener: (fun(data: mare.event.app__ready, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "app/shutdown", listener: (fun(data: mare.event.app__shutdown, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "approval/changed", listener: (fun(data: mare.event.approval__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "approval/requested", listener: (fun(data: mare.event.approval__requested, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "approval/resolved", listener: (fun(data: mare.event.approval__resolved, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "approval/review-result", listener: (fun(data: mare.event.approval__review_result, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "changes/rewound", listener: (fun(data: mare.event.changes__rewound, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/approaching", listener: (fun(data: mare.event.compact__approaching, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/before", listener: (fun(data: mare.event.compact__before, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/completed", listener: (fun(data: mare.event.compact__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/idle", listener: (fun(data: mare.event.compact__idle, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/overflow", listener: (fun(data: mare.event.compact__overflow, ev: mare.EventEnvelope): ({handled?: boolean, succeeded?: boolean, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/stage", listener: (fun(data: mare.event.compact__stage, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/started", listener: (fun(data: mare.event.compact__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "compact/summary-error", listener: (fun(data: mare.event.compact__summary_error, ev: mare.EventEnvelope): ({summary?: string, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "context/report", listener: (fun(data: mare.event.context__report, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "context/system", listener: (fun(data: mare.event.context__system, ev: mare.EventEnvelope): ({text?: string, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "context/transform", listener: (fun(data: mare.event.context__transform, ev: mare.EventEnvelope): ({messages?: table<string, any>[], model?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "conversation/changed", listener: (fun(data: mare.event.conversation__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "conversation/closed", listener: (fun(data: mare.event.conversation__closed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "conversation/opened", listener: (fun(data: mare.event.conversation__opened, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "conversation/settled", listener: (fun(data: mare.event.conversation__settled, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "conversation/stream", listener: (fun(data: mare.event.conversation__stream, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "copy/completed", listener: (fun(data: mare.event.copy__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "copy/request", listener: (fun(data: mare.event.copy__request, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "editor/changed", listener: (fun(data: mare.event.editor__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "fs/changes-inexact", listener: (fun(data: mare.event.fs__changes_inexact, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "fs/observed", listener: (fun(data: mare.event.fs__observed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "fs/watch-event", listener: (fun(data: mare.event.fs__watch_event, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "host/notice", listener: (fun(data: mare.event.host__notice, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/complete", listener: (fun(data: mare.event.input__complete, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/follow-up", listener: (fun(data: mare.event.input__follow_up, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/queue-cancel", listener: (fun(data: mare.event.input__queue_cancel, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/queue-edit", listener: (fun(data: mare.event.input__queue_edit, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/queue-menu", listener: (fun(data: mare.event.input__queue_menu, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/queue-move", listener: (fun(data: mare.event.input__queue_move, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/queue-remove", listener: (fun(data: mare.event.input__queue_remove, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/queue-select", listener: (fun(data: mare.event.input__queue_select, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/steer", listener: (fun(data: mare.event.input__steer, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/submit", listener: (fun(data: mare.event.input__submit, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "input/transform", listener: (fun(data: mare.event.input__transform, ev: mare.EventEnvelope): (table<string, any>|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "interaction/requested", listener: (fun(data: mare.event.interaction__requested, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "interaction/resolved", listener: (fun(data: mare.event.interaction__resolved, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "item/completed", listener: (fun(data: mare.event.item__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "item/delta", listener: (fun(data: mare.event.item__delta, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "item/started", listener: (fun(data: mare.event.item__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "jobs/changed", listener: (fun(data: mare.event.jobs__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "jobs/internal-changed", listener: (fun(data: mare.event.jobs__internal_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "kernel/before-reload", listener: (fun(data: mare.event.kernel__before_reload, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "kernel/doctor", listener: (fun(data: mare.event.kernel__doctor, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "kernel/error", listener: (fun(data: mare.event.kernel__error, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "kernel/reloaded", listener: (fun(data: mare.event.kernel__reloaded, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "kernel/stalled", listener: (fun(data: mare.event.kernel__stalled, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "key/interrupt", listener: (fun(data: mare.event.key__interrupt, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/crash-limit", listener: (fun(data: mare.event.llm__crash_limit, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/fallback", listener: (fun(data: mare.event.llm__fallback, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/login", listener: (fun(data: mare.event.llm__login, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/logout", listener: (fun(data: mare.event.llm__logout, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/model-changed", listener: (fun(data: mare.event.llm__model_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/model-changing", listener: (fun(data: mare.event.llm__model_changing, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/params-adjusted", listener: (fun(data: mare.event.llm__params_adjusted, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/params-changed", listener: (fun(data: mare.event.llm__params_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/providers-changed", listener: (fun(data: mare.event.llm__providers_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/rate-limits", listener: (fun(data: mare.event.llm__rate_limits, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/retry", listener: (fun(data: mare.event.llm__retry, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "llm/usage", listener: (fun(data: mare.event.llm__usage, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "login/cli", listener: (fun(data: mare.event.login__cli, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "mcp/changed", listener: (fun(data: mare.event.mcp__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "mcp/cli", listener: (fun(data: mare.event.mcp__cli, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "mcp/codemode", listener: (fun(data: mare.event.mcp__codemode, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "mcp/elicit", listener: (fun(data: mare.event.mcp__elicit, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "mcp/error", listener: (fun(data: mare.event.mcp__error, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "mcp/ready", listener: (fun(data: mare.event.mcp__ready, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "model/changed", listener: (fun(data: mare.event.model__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "option/apply", listener: (fun(data: mare.event.option__apply, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "option/changed", listener: (fun(data: mare.event.option__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "option/current", listener: (fun(data: mare.event.option__current, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "peer/held", listener: (fun(data: mare.event.peer__held, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "peer/inbound", listener: (fun(data: mare.event.peer__inbound, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "peer/message", listener: (fun(data: mare.event.peer__message, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "peer/rejected", listener: (fun(data: mare.event.peer__rejected, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "peer/released", listener: (fun(data: mare.event.peer__released, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "permission/changed", listener: (fun(data: mare.event.permission__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "permission/parse", listener: (fun(data: mare.event.permission__parse, ev: mare.EventEnvelope): ({parsed?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "pipeline/approval-decided", listener: (fun(data: mare.event.pipeline__approval_decided, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "pipeline/approval-reviewed", listener: (fun(data: mare.event.pipeline__approval_reviewed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "pipeline/tool-dispatching", listener: (fun(data: mare.event.pipeline__tool_dispatching, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "plugins/completed", listener: (fun(data: mare.event.plugins__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "plugins/plugin", listener: (fun(data: mare.event.plugins__plugin, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "profile/tools-changed", listener: (fun(data: mare.event.profile__tools_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "projection/changed", listener: (fun(data: mare.event.projection__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "provider/event", listener: (fun(data: mare.event.provider__event, ev: mare.EventEnvelope): ({event?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "provider/request", listener: (fun(data: mare.event.provider__request, ev: mare.EventEnvelope): (table<string, any>|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "provider/transport", listener: (fun(data: mare.event.provider__transport, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "query/changed", listener: (fun(data: mare.event.query__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "queue/changed", listener: (fun(data: mare.event.queue__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "reload/completed", listener: (fun(data: mare.event.reload__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "repeat-guard/reminder", listener: (fun(data: mare.event.repeat_guard__reminder, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "request/build", listener: (fun(data: mare.event.request__build, ev: mare.EventEnvelope): (table<string, any>|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "request/prepare", listener: (fun(data: mare.event.request__prepare, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "review/completed", listener: (fun(data: mare.event.review__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "sandbox/changed", listener: (fun(data: mare.event.sandbox__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/changed", listener: (fun(data: mare.event.session__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/closed", listener: (fun(data: mare.event.session__closed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/damaged", listener: (fun(data: mare.event.session__damaged, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/deleted", listener: (fun(data: mare.event.session__deleted, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/loaded", listener: (fun(data: mare.event.session__loaded, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/named", listener: (fun(data: mare.event.session__named, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/new", listener: (fun(data: mare.event.session__new, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/recorded", listener: (fun(data: mare.event.session__recorded, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/retry", listener: (fun(data: mare.event.session__retry, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/started", listener: (fun(data: mare.event.session__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/switch-completed", listener: (fun(data: mare.event.session__switch_completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/transition-committed", listener: (fun(data: mare.event.session__transition_committed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/transition-completed", listener: (fun(data: mare.event.session__transition_completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "session/transition-failed", listener: (fun(data: mare.event.session__transition_failed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "spill/backends-changed", listener: (fun(data: mare.event.spill__backends_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "status/changed", listener: (fun(data: mare.event.status__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "status/exit", listener: (fun(data: mare.event.status__exit, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "status/session", listener: (fun(data: mare.event.status__session, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "step/completed", listener: (fun(data: mare.event.step__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "step/started", listener: (fun(data: mare.event.step__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "subagent/completed", listener: (fun(data: mare.event.subagent__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "subagent/progress", listener: (fun(data: mare.event.subagent__progress, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "subagent/started", listener: (fun(data: mare.event.subagent__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "theme/changed", listener: (fun(data: mare.event.theme__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "theme/palette", listener: (fun(data: mare.event.theme__palette, ev: mare.EventEnvelope): ({palette?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tool/after", listener: (fun(data: mare.event.tool__after, ev: mare.EventEnvelope): ({result?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tool/authorized", listener: (fun(data: mare.event.tool__authorized, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tool/program-result", listener: (fun(data: mare.event.tool__program_result, ev: mare.EventEnvelope): ({result?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tool/result", listener: (fun(data: mare.event.tool__result, ev: mare.EventEnvelope): ({result?: table<string, any>, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tool/snapshot-void", listener: (fun(data: mare.event.tool__snapshot_void, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tools/executors-changed", listener: (fun(data: mare.event.tools__executors_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "tools/loaded", listener: (fun(data: mare.event.tools__loaded, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "trust/changed", listener: (fun(data: mare.event.trust__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "trust/resolve", listener: (fun(data: mare.event.trust__resolve, ev: mare.EventEnvelope): ({decision?: boolean, [string]: any}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "turn/completed", listener: (fun(data: mare.event.turn__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "turn/started", listener: (fun(data: mare.event.turn__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/background", listener: (fun(data: mare.event.ui__background, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/caps", listener: (fun(data: mare.event.ui__caps, ev: mare.EventEnvelope): (table<string, any>|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/caps-changed", listener: (fun(data: mare.event.ui__caps_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/card", listener: (fun(data: mare.event.ui__card, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/commit-after", listener: (fun(data: mare.event.ui__commit_after, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/commit-before", listener: (fun(data: mare.event.ui__commit_before, ev: mare.EventEnvelope): ({lines: string[]}|nil)), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/external-output", listener: (fun(data: mare.event.ui__external_output, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/keymap-changed", listener: (fun(data: mare.event.ui__keymap_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/markdown-changed", listener: (fun(data: mare.event.ui__markdown_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/presentation-invalidate", listener: (fun(data: mare.event.ui__presentation_invalidate, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/presentation-render-completed", listener: (fun(data: mare.event.ui__presentation_render_completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/presenter-changed", listener: (fun(data: mare.event.ui__presenter_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/print", listener: (fun(data: mare.event.ui__print, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/ready", listener: (fun(data: mare.event.ui__ready, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/reflow", listener: (fun(data: mare.event.ui__reflow, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/resize", listener: (fun(data: mare.event.ui__resize, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/resume", listener: (fun(data: mare.event.ui__resume, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/wait-completed", listener: (fun(data: mare.event.ui__wait_completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "ui/wait-started", listener: (fun(data: mare.event.ui__wait_started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "update-check/available", listener: (fun(data: mare.event.update_check__available, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "usage/changed", listener: (fun(data: mare.event.usage__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "web/providers-changed", listener: (fun(data: mare.event.web__providers_changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/agent-completed", listener: (fun(data: mare.event.workflow__agent_completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/agent-started", listener: (fun(data: mare.event.workflow__agent_started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/changed", listener: (fun(data: mare.event.workflow__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/completed", listener: (fun(data: mare.event.workflow__completed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/log", listener: (fun(data: mare.event.workflow__log, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/phase", listener: (fun(data: mare.event.workflow__phase, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workflow/started", listener: (fun(data: mare.event.workflow__started, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "working-directory/changed", listener: (fun(data: mare.event.working_directory__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@overload fun(self: mare.Ctx, name: "workspace/changed", listener: (fun(data: mare.event.workspace__changed, ev: mare.EventEnvelope): any), options?: boolean|mare.OnOptions): fun()
---@param name string
---@param listener function
---@param options? boolean|mare.OnOptions
---@return fun() dispose
function Ctx:on(name, listener, options) end
