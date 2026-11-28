---@meta _
-- GENERATED from types/mare/services/contracts/agent.lua (service `agent`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.AgentService
local M = {}

--- Synchronously capture detached agent activity state without yielding, queueing, or persisting partials.
---@return mare.AgentActivity
function M:activity() end

--- Atomically install a prepared session state snapshot without yielding or emitting events.
---@param snapshot {session_id: string, messages: mare.Message[], tool_states: table, queue?: table, revision: any}
---@return table
function M:adopt_session(snapshot) end

--- Append a message to the active conversation; returns `ok, err`.
---@param msg mare.Message|nil
---@param opts? {operation?: table<string, any>}
---@return boolean
---@return string?
function M:append(msg, opts) end

--- Non-yielding snapshot cut of agent runtime state and active turn items.
---@return table<string, any>
function M:baseline_cut() end

--- Cancel or halt the active turn (optionally checking `turn_id` and/or clearing queued inputs) and return any removed queued items.
---@param opts? {clear_queue?: boolean, mode?: "cancel"|"halt", reason?: string, request_id?: string, turn_id?: string}
---@return {duplicate?: boolean, removed: ({content?: table[], images: {data?: string, mime: string, path?: string, size?: integer}[], input_context?: any, input_id: string, lane: "steer"|"follow_up", text: string})[], turn_id?: string}
function M:cancel(opts) end

--- Declare feature capabilities implemented by the active agent service.
---@return {input_admission: integer, [string]: any}
function M:capabilities() end

--- Return false for a stale or foreign session revision token, synchronously and without mutation or events.
---@param revision any
---@return boolean
function M:check_session_revision(revision) end

--- Resume an interrupted turn or run queued inputs (`steer` and `follow_up`).
---@param opts? {effort?: string, expected_session?: {session_epoch: string, session_id: string}, output_schema?: table, source?: string, system?: string, tools?: (string|table)[], cancel?: mare.Token, history_effort?: string, history_model?: string, input_context?: table, messages?: mare.Message[], transition_id?: string, turn_id?: string}
---@return {turn_id: string}
function M:continue(opts) end

--- Remove one queued input item by `input_id`.
---@param input_id string
---@return table
function M:delete_queued(input_id) end

--- Durably enqueue an input item into the active session inbox without starting a turn, returning a receipt scoped to the active session owner.
---@param input string|({data?: string, duration_secs?: number, mime?: string, name?: string, path?: string, text?: string, transcription?: string, type: "text"|"image"|"file"|"audio"|"video"})[]
---@param owner {session_epoch: string, session_id: string}
---@param opts? {attachments?: table<string, any>[], delivery?: "follow_up"|"steer", image_markers?: table<string, any>[], images?: table<string, any>[], input_context?: any, request_id?: string, source?: string, cancel?: mare.Token?, [string]: any}
---@return {duplicate?: boolean, durable?: boolean, error?: string, input_id?: string, session_epoch?: string, session_id?: string, status: "queued"|"stale"|"rejected"|"cancelled"|"indeterminate", [string]: any}
function M:enqueue(input, owner, opts) end

--- Snapshot of current agent activity (`busy`, `turn_id`, `queued` count, `model`, `effort`, `activity`).
---@return {activity?: {extra: table, extra_errors: table}, busy: boolean, effort?: string, interrupted?: boolean, message_count?: integer, model?: string, queued: integer, session_id?: string, turn_cancelled?: boolean, turn_id?: string}
function M:get() end

--- True when the conversation context ends mid-turn and the loop can resume via `continue()`.
---@return boolean
function M:interrupted() end

--- Current conversation messages visible to the model (optionally paginated by cursor and limit).
---@param opts? {cursor?: string, limit?: integer}
---@return {data: table[], next_cursor?: string}
function M:list_messages(opts) end

--- Queued inputs in delivery order (steer items first, then follow-ups; image metadata only).
---@return {data: ({content?: table[], images: {data?: string, mime: string, path?: string, size?: integer}[], input_context?: any, input_id: string, lane: "steer"|"follow_up", text: string})[]}
function M:list_queue() end

--- Raw lossless conversation messages array for bounded in-process host inspection.
---@return mare.Message[]
function M:messages() end

--- Queue a notification message for the model (joins the next step while busy, or wakes a new turn when `wake = true`).
---@param text string
---@param opts? {wake?: boolean}
---@return table
function M:notify(text, opts) end

--- Return the interrupted request that a resume will retry, or nil.
---@return {attempt: integer, request_id: string, [string]: any}|nil
function M:pending_request() end

--- Return the current persistence fault error message, or nil when healthy.
---@return string|nil
function M:persist_error() end

--- Prepare one atomic session-owned inbox/message publication; external readers use `list_queue()`.
---@param inbox table<string, any>
---@param messages table<string, any>[]
---@param turn_id? string
---@return function
---@return function
---@return table[]
function M:prepare_commit(inbox, messages, turn_id) end

--- Submit user input to the agent (queued inputs enter follow_up by default, or steer via `agent.steer`); starts a turn when idle, or handles busy state via `when_busy` (`queue` default, or `reject`).
---@param input string|({data?: string, duration_secs?: number, mime?: string, name?: string, path?: string, text?: string, transcription?: string, type: "text"|"image"|"file"|"audio"|"video"})[]
---@param opts? {effort?: string, expected_session?: {session_epoch: string, session_id: string}, output_schema?: table, request_id?: string, source?: string, system?: string, tools?: (string|table)[], when_busy?: "queue"|"reject", cancel?: mare.Token, content?: table, custom_message?: table, history_effort?: string, history_model?: string, image_markers?: table[], images?: table[], input_context?: table, messages?: mare.Message[], peer_message?: boolean, queue_only?: boolean, queued?: boolean, transition_id?: string, turn_id?: string}
---@return {duplicate?: boolean, input_id?: string, status: "started"|"queued", turn_id?: string}
function M:prompt(input, opts) end

--- Queue or append a custom message with delivery mode (`next_turn` | `pending` | `steer` | `follow_up`).
---@param msg table<string, any>
---@param mode? "next_turn"|"pending"|"steer"|"follow_up"
---@param opts? {operation?: table<string, any>, text?: string, [string]: any}
---@return table|nil
function M:queue_custom(msg, mode, opts) end

--- Register a namespaced synchronous JSON snapshot provider owned by the caller ctx; returns an unregister disposer.
---@param namespace string
---@param snapshot_fn fun(): any
---@return fun() dispose releases this registration
function M:register_activity(namespace, snapshot_fn) end

--- Un-park and start a turn for a durably queued input scoped to the active session owner.
---@param owner {session_epoch: string, session_id: string}
---@param opts? {input_id?: string, [string]: any}
---@return {error?: string, input_id?: string, status: "started"|"stale"|"busy"|"empty"|"rejected", turn_id?: string, [string]: any}
function M:resume_input(owner, opts) end

--- Return an opaque provider-owned revision token for the current session state, synchronously and without mutation.
---@return any
function M:session_revision() end

--- Replace the active conversation message array (`notify = false` is synchronous and event-free for guarded publication).
---@param msgs mare.Message[]
---@param opts? {notify?: boolean}
---@return table
function M:set_messages(msgs, opts) end

--- Record an interrupted request marker found on resume so it can be retried with the same request id.
---@param rec? {attempt: integer, request_id: string, [string]: any}|nil
---@param opts? {clean?: boolean}
---@return table
function M:set_pending_request(rec, opts) end

--- Restore queued inputs on session resume so they run as follow-ups.
---@param q {steer?: mare.QueuedInput[], follow_up?: mare.QueuedInput[]}
---@return table
function M:set_queued(q) end

--- Bind the active session id for durable queue persistence.
---@param session_id? string|nil
---@return table
function M:set_session_id(session_id) end

--- Restore recorded tool call states on resume (`{ [tool_call_id] = {state, side_effect, idempotent} }`).
---@param states table<string, any>
---@return table
function M:set_tool_states(states) end

--- Inject steering input into the currently running turn.
---@param input string|({data?: string, duration_secs?: number, mime?: string, name?: string, path?: string, text?: string, transcription?: string, type: "text"|"image"|"file"|"audio"|"video"})[]
---@param opts? {expected_turn_id?: string, request_id?: string, source?: string, cancel?: mare.Token, content?: table, custom_message?: table, image_markers?: table[], images?: table[], input_context?: table, peer_message?: boolean}
---@return {duplicate?: boolean, input_id: string}
function M:steer(input, opts) end

--- Register a namespaced model-step participant whose prepared JSON state commits atomically before each model request.
---@param name string
---@param prepare fun(previous: any, step: {step_id: string, turn_id: string, step: integer, cancel: mare.Token, messages: mare.Message[], tools?: string[]}): any
---@param opts? {failure_policy?: "required"|"defer", output?: "state"|"transaction"}
---@return fun() dispose releases this registration
function M:step_participant(name, prepare, opts) end

--- Return the frozen candidate state of the active step (`step_id`), or the committed state when omitted.
---@param name string
---@param step_id? string
---@return any
---@return string?
function M:step_preview(name, step_id) end

--- Return a detached JSON snapshot of the last committed step participant state, restored per session.
---@param name string
---@return any
function M:step_state(name) end

--- Update the content and/or lane of one queued input item.
---@param input_id string
---@param opts? {input?: string|({data?: string, duration_secs?: number, mime?: string, name?: string, path?: string, text?: string, transcription?: string, type: "text"|"image"|"file"|"audio"|"video"})[], lane?: "steer"|"follow_up", content?: table, image_markers?: table[], images?: table[]}
---@return {content?: table[], images: {data?: string, mime: string, path?: string, size?: integer}[], input_context?: any, input_id: string, lane: "steer"|"follow_up", text: string}
function M:update_queued(input_id, opts) end

--- Block until the specified (or active) turn settles and return its `Turn` snapshot.
---@param turn_id? string
---@param opts? {timeout_ms?: integer}
---@return {cancellation_cause?: "user"|"parent"|"hook"|"disposed", completed_at_ms?: integer, error?: {code: string, data?: any, message: string}, error_kind?: string, messages?: table[], not_sent?: boolean, save_warning?: string, started_at_ms: integer, status: "completed"|"cancelled"|"failed"|"running", stop_reason?: string, structured?: any, termination_reason?: "completed"|"usage_limit"|"interrupted"|"aborted"|"turn_limit"|"error", text?: string, timed_out?: boolean, truncated?: boolean, turn_id: string, usage?: table}
function M:wait(turn_id, opts) end
