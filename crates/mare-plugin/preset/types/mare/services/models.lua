---@meta _
-- Shared LuaLS data classes and aliases referenced by generated service contracts (`*.gen.lua`).
-- Service method declarations live exclusively in `types/mare/services/*.gen.lua`.

-- --- LLM & model types ------------------------------------------------------

---@alias mare.NativeMap table<string, table> Wire-keyed passthrough metadata (`native[wire_name]`).

---@class mare.Citation
---@field url? string
---@field title? string
---@field start_index? integer
---@field end_index? integer

---@class mare.HostedTool
---@field type "hosted"
---@field name string Hosted tool name (e.g. `"web_search"`)
---@field options? table Wire-specific hosted tool options

---@class mare.Model
---@field id string
---@field ref string "provider/id"
---@field provider string
---@field name? string
---@field context_window? integer measured input limit
---@field max_output? integer
---@field reasoning? boolean
---@field efforts? string[]
---@field cost? {input?: number, output?: number, cache_read?: number, cache_write?: number} USD per million tokens
---@field edit_tool? "apply_patch"|"replace"
---@field truncation? {mode: "tokens"|"bytes", limit: integer} tool-output budget
---@field images? boolean false: the model takes no images — they become "Attachment: <path>" text
--- a provider with several routes: the catalogue's route for this model (llm/routes layer 3)
---@field route? "responses"|"anthropic"|"chat"|"google"|string
---@field tool_update? "addition_only"
---@field cache_retention? "none"|"short"|"long"
--- models.dev's wire protocol for a listed model (llm/routes layer 4, when the provider uses it)
---@field protocol? "responses"|"messages"|"chat-completions"|"generate-content"

--- Content block shapes on `mare.Message.content`:
--- - `{ type = "text", text = string, citations?: mare.Citation[], native?: mare.NativeMap }`
--- - `{ type = "reasoning", text = string, redacted?: boolean, native?: mare.NativeMap }`
--- - `{ type = "tool_call", id = string, name = string, arguments = table, hosted?: boolean, result?: table, native?: mare.NativeMap }`
---   (`hosted = true` calls are executed server-side and carry their neutral `result`; cross-model/cross-wire history drops them, so wire encoders only ever receive their own hosted calls.)
--- - `{ type = "image", data?: string, mime?: string, path?: string, native?: mare.NativeMap }`
--- - Extension blocks `{ type = "<wire>.<name>", text?: string, native: mare.NativeMap }`: unknown blocks are kept verbatim in history, skipped by default UI renderers, and fall back to `text` in `llm:text_of` / compaction / cross-model replay.
---@class mare.Message
---@field role "user"|"assistant"|"tool_result"
---@field content table[]
---@field usage? {input?: integer, output?: integer, cache_read?: integer, cache_write?: integer, cache_write_1h?: integer, reasoning?: integer, cost?: number, cost_unknown?: boolean}
---@field stop_reason? "stop"|"tool_use"|"length"|"error"|"aborted"|"paused"|"filtered"|"other"
---@field error_kind? string
---@field error_code? string
---@field error_data? {kind?: string, error_code?: string, original_error_kind?: string, raw_code?: string, raw_type?: string, status?: integer, provider?: string, request_id?: string, source?: string}
---@field error? string
---@field tool_call_id? string
---@field tool_name? string
---@field is_error? boolean
---@field provider_id? string
---@field model_id? string
---@field route? string
---@field native? mare.NativeMap

---@class mare.LlmRequestRoutes
---@field gateway? string
---@field source? string
---@field wire? string
---@field auth? string
---@field config? table
---@field describe? table
---@field stream? fun(endpoint: mare.WireEndpoint, req: mare.LlmRequest, sink: fun(ev: mare.StreamEvent)): mare.Message

---@class mare.LlmParamsOverlay
---@field route? string
---@field body? table
---@field headers? table
---@field reserved? integer
---@field operations? table[]
---@field effort? string

---@class mare.LlmRequest
---@field model? mare.Model
---@field system? string
---@field messages mare.Message[]
---@field tools? table[]
---@field tool_choice? any
---@field max_tokens? integer
---@field effort? string
---@field output_schema? table
---@field params? table<string, any>
---@field native? mare.NativeMap
---@field routes? mare.LlmRequestRoutes per-request resolved route snapshot set by `llm.stream`
---@field params_overlay? mare.LlmParamsOverlay prepared wire parameter overlay attached before send
---@field cancel? mare.Token
---@field cache? boolean|"read"
---@field cache_retention? "none"|"short"|"long" prompt cache retention tier (default "short")
---@field session_id? string
---@field request_id? string
---@field no_fallback? boolean
---@field purpose? string
---@field subagent? string
---@field attempt? integer
---@field on_attempt? fun(attempt: integer): boolean|nil
---@field header_timeout_ms? integer
---@field idle_timeout_ms? integer
---@field first_event_timeout_ms? integer
---@field sse_max_event_bytes? integer
---@field sse_max_line_bytes? integer
---@field fresh_connection? boolean

---@class mare.StreamEvent
---@field type "text_delta"|"reasoning_delta"|"tool_call_start"|"tool_call_delta"|"hosted_tool_start"|"hosted_tool_update"|"hosted_tool_done"|"annotation"|string
---@field delta? string
---@field tool_call_id? string
---@field name? string
---@field id? string
---@field status? string
---@field action? table
---@field result? table
---@field native? mare.NativeMap

---@class mare.Provider
---@field provider string
---@field label? string
---@field models? mare.Model[]
---@field default_model? string
---@field stream? fun(req: mare.LlmRequest, sink: fun(ev: mare.StreamEvent)): mare.Message
---@field wire? string wire name (required for data-tier entries when `wire_api` is omitted)
---@field wire_api? "chat"|"responses"|"anthropic"|"google"|string shorthand wire selector
---@field base_url? string endpoint base URL for data-tier provider entries
---@field dialect? string vendor dialect id understood by the target wire plugin
---@field models_dev? string|boolean models.dev provider id, or `false` to disable enrichment
---@field api_key? string|{cmd: string[], refresh_interval_ms?: integer}
---@field api_key_env? string default environment variable name for credentials
---@field key_prefixes? string[] unambiguous API key prefixes for `/login` provider identification
---@field endpoint? fun(req: mare.LlmRequest): table (reserved)
---@field aliases? table<string, string|{family: string}>
---@field model_defaults? table<string, table<string, any>>
---@field hosted_tools? {web_search?: boolean|string|{supported?: boolean, mode?: "disabled"|"cached"|"indexed"|"live", config?: table}}
---@field errors? {codes?: table<string, string|{kind?: string, retryable?: boolean, hint?: string}>, map?: (fun(raw: table): table|nil), overflow?: (fun(msg: string, code?: any): boolean), image_limit?: (fun(msg: string, code?: any): boolean)}
---@field login? {methods: ("oauth"|"device"|"api_key")[], run: async fun(lio?: table)}|(async fun(lio?: table))
---@field login_label? string
---@field auth_keys? string[]
---@field is_configured? fun(): boolean
---@field prepare? async fun(model: mare.Model, opts: {cancel?: any})
---@field tool_id_policy? {max?: integer}
---@field compact? fun(req: {model: mare.Model, system: string, tools: table[], messages: mare.Message[], session_id?: string, cancel?: any}): ({items: table[], model: string}|nil), string|nil
---@field route? "responses"|"anthropic"|"chat"|"google"|string
---@field defer_loading? boolean
---@field no_key? boolean
---@field routes? table<string, boolean>
---@field model_routes? table<string, string> glob over model ids (or provider/id) → route; longest wins
---@field models_dev_routes? boolean take a listed model's route from its models.dev protocol
---@field unavailable_for? fun(model: mare.Model): string|nil
---@field catalog? fun(for_fetch?: boolean): table|nil
---@field on_unauthorized? fun(err: table, info?: table): boolean|nil
---@field replace? boolean allow replacing an existing provider of the same name

---@class mare.WireDecoder
---@field event fun(ev: table): boolean|nil
---@field body? fun(text: string, headers: table)
---@field finish fun(): mare.Message

---@class mare.WireDescribe
---@field route? string
---@field path? string
---@field auth? string[]
---@field hosted_tools? string[]
---@field provider_fields? table<string, string>
---@field model_fields? table<string, string>

---@class mare.WireSpec
---@field name? string required for register_wire(spec)
---@field route? string
---@field path? string
---@field auth? string[]
---@field config? table
---@field hosted_tools? string[] hosted tool names supported by this wire (e.g. `{ "web_search" }`)
---@field provider? fun(name: string, entry: table): table factory turning a data-tier provider entry into runtime hooks
---@field provider_fields? table<string, string> wire-specific allowed provider entry fields and their type names
---@field model_fields? table<string, string> wire-specific allowed model entry fields and their type names
---@field enrich_model? fun(model: table, entry: table, mark: fun(field: string)) optional models.dev enrichment hook for wire-specific model fields
---@field encode fun(req: mare.LlmRequest, endpoint: table): table, any?
---@field filter_tools? fun(tools: table[], req: mare.LlmRequest, meta: table): table[] optional pre-bind tool compatibility filter so wire declarations and local dispatch bindings match
---@field finalize? fun(req: mare.LlmRequest, endpoint: table, wire: table, state?: any)
---@field decoder fun(sink: fun(ev: mare.StreamEvent), context?: {endpoint: table, request: table, response?: table, state?: any}): mare.WireDecoder
---@field error? fun(payload: any): {api_code?: any, api_type?: string, message?: string, status?: integer, request_id?: string, trace_id?: string}|nil
---@field errors? {codes?: table<string, string|{kind?: string, retryable?: boolean, hint?: string}>, overflow?: (fun(msg: string, code?: any): boolean), image_limit?: (fun(msg: string, code?: any): boolean)}
---@field retry? fun(wire: table, err: table): boolean
---@field estimate_hints? mare.TokenEstimateHint Wire-specific image shapes and capped opaque fields for `mare.text.tokens`.

---@alias mare.WireEndpoint table|(fun(req: mare.LlmRequest): table)

---@class mare.ReadinessArgs
---@field model? mare.Model
---@field cancel? mare.Token

---@class mare.ReadinessStep
---@field order number ascending; llm 10, mcp 20, context 30 (mare_lib.READY_ORDER)
---@field prepare? fun(args: mare.ReadinessArgs)
---@field ready? fun(args: mare.ReadinessArgs): table|nil
---@field timeout_ms? integer a positive cap on `prepare`; absent or 0: none

-- --- Tools, agent, jobs, permissions types ----------------------------------

---@class mare.ToolResult
---@field content string|table[]
---@field is_error? boolean
---@field details? table
---@field display? mare.Node
---@field max_line_chars? integer
---@field no_truncate? boolean
---@field output_notice? string Recovery notice reserved intact within the final output budget (text results).
---@field error_code? "tool_timeout"|string set by the tools service (e.g. the call hit its timeout)
---@field cancelled? boolean the call was cancelled (Esc, or its plugin was reloaded)
---@field refused? boolean
---@field terminate? boolean when every result of a tool batch sets it, the run stops instead of asking the model again (`agent/turn-stopping` still decides)

---@class mare.QueuedInput
---@field input_id string Stable inbox identity, retained by persistence and abort.
---@field text string
---@field images? table[] Ordered attachment blocks, preserved without rewriting their data.
---@field image_markers? {text:string,offset:integer}[] Exact editor marker identities and UTF-8 byte offsets.

---@alias mare.AgentTerminationReason "completed"|"usage_limit"|"interrupted"|"turn_limit"|"error"

---@class mare.AgentActivity
---@field version integer currently 1
---@field busy boolean
---@field queued integer
---@field turn_cancelled boolean
---@field session_id? string
---@field turn_id? string
---@field messages mare.Message[]
---@field request? {system: string, source_message_count: integer, messages: mare.Message[], tools: table[], model?: string, params: table}
---@field streaming {type: "text"|"reasoning"|"tool_call", text?: string, id?: string, name?: string, raw_arguments?: string}[]
---@field running_tools {call: table, output_tail: string}[]
---@field extra table<string, any>
---@field extra_errors table<string, string>

---@class mare.JobInfo
---@field job_id string "<kind>-<16 hex>"
---@field kind string
---@field description string|nil
---@field label string short display label (label > description > kind)
---@field owner string
---@field status "running"|"completed"|"failed"|"cancelled"|"interrupted"
---@field exit_code integer|nil
---@field exit_signal integer|nil
---@field background boolean
---@field started_at_ms integer epoch ms
---@field ended_at_ms integer|nil
---@field handle mare.Job|table

---@class mare.PermissionProfile
---@field read? string[] exact readable paths
---@field write? string[] existing physical writable directories; never implicitly widened to parents
---@field network? boolean|string[]

-- --- Session, context, commands, conversations, changes types ---------------

---@class mare.Session
---@field session_id string
---@field session_epoch? string
---@field name? string
---@field name_revision? integer
---@field cwd string
---@field path? string
---@field parent_session_id? string
---@field source_leaf_entry_id? string
---@field leaf_entry_id? string
---@field loaded boolean
---@field status? "idle"|"busy"
---@field ephemeral? boolean
---@field archived? boolean
---@field created_at_ms integer
---@field updated_at_ms integer

---@class mare.LiveSession
---@field session_id string
---@field session_epoch string
---@field name? string
---@field name_revision? integer
---@field cwd string
---@field path? string
---@field parent_session_id? string
---@field leaf_entry_id? string
---@field loaded boolean
---@field status? "idle"|"busy"
---@field ephemeral? boolean
---@field archived? boolean
---@field created_at_ms integer
---@field updated_at_ms integer

---@alias mare.CommandAvailability "always"|"idle"|"session"|"main"|"running"|"tui"

---@class mare.CommandInput
---@field content? mare.ContentPart[]

---@class mare.PromptSection
---@field name string unique per layer
---@field order? number lower first (default 500); 0–999 stable general sections, 1000–9999 plugin sections, >= 10000 runtime facts
---@field text string|fun(model: table, info: {agent?: string, model?: table, scope: string}): string|nil nil or "" skips the section
---@field tag? string the XML tag the model sees (default: name)
---@field when? fun(model: table): boolean
---@field scope? "global"|"user"|"agent" the layer (default global)
---@field agent? string only for the agent of this name (agent layer)
---@field complete? boolean this section is the whole prompt; two active complete sections fail the assembly
---@field raw? boolean don't wrap in <tag> tags
---@field interpolate? boolean default true
---@field runtime? boolean a runtime-context section (cwd, environment, …): `suppress_runtime` can hide it
---@field required? boolean a runtime section `suppress_runtime` leaves in place

---@class mare.PromptPreviewSection
---@field name string
---@field tag string
---@field order number
---@field layer "global"|"user"|"agent"
---@field owner string
---@field complete boolean
---@field raw boolean
---@field runtime boolean
---@field required boolean
---@field candidate boolean one of the preview's `opts.sections`
---@field active boolean
---@field preserved boolean whether its rendered text survived the `context/system` post-hook
---@field shadowed_by? {name: string, layer: "global"|"user"|"agent", owner: string}
---@field status "active"|"shadowed"|"removed"|"suppressed"|"inactive"|"preempted_by_complete"

---@class mare.PromptPreview
---@field text string the prompt after the `context/system` post-hook (a complete section's own text)
---@field complete? string the active complete section's name
---@field runtime_suppressed boolean
---@field sections mare.PromptPreviewSection[]

---@class mare.OperationIdentity
---@field request_id string Stable caller identity, independent of the transport request id.
---@field kind string Host-assigned operation kind; never an authorization grant.
---@field fingerprint string Canonical input SHA256.
---@field session_id? string Expected actual session owner.
---@field session_epoch? string Expected actual owner session epoch.

---@class mare.OperationReceipt: mare.OperationIdentity
---@field version 1
---@field session_id string
---@field result table Complete detached semantic result.

---@class mare.OperationClaim
---@field status "claimed"|"replayed"
---@field request_id? string
---@field session_id? string
---@field session_epoch? string
---@field claim_id? string Lifecycle correlation only, not an authorization token.
---@field receipt? mare.OperationReceipt

---@class mare.OperationReceiptLookup
---@field status "replayed"|"missing"
---@field receipt? mare.OperationReceipt

---@class mare.SessionTransitionSource
---@field session_id string
---@field session_epoch string

---@class mare.SessionTransitionSpec
---@field mode? "new"|"workspace"
---@field data? {cwd?:string,carry?:boolean}
---@field cwd? string
---@field carry? boolean
---@field records? {kind:string,data:any,scope?:"session"|"branch"}[]
---@field messages? mare.Message[]
---@field initial? {kind:"input",content:string|table[],source?:string,input_context?:any}|{kind:"continue",opts?:table}

---@class mare.ConversationParentStatus
---@field state "idle"|"working"|"needs_approval"|"needs_input"|"interrupted"|"failed"|"closed"|"finished"
---@field detail? string
---@field actionable? boolean

---@class mare.ConversationInfo
---@field conversation_id string
---@field session_id? string
---@field selected boolean
---@field busy boolean
---@field model? string
---@field params? table<string, any>
---@field message_count integer
---@field parent_status? mare.ConversationParentStatus
---@field turn_id? string
---@field ui_context_id? string

---@class mare.ChangeFile
---@field path string
---@field before? string|false backup hash before this turn
---@field after? string|false backup hash after this turn
---@field missing? boolean the backup was trimmed
---@field old? string content before this turn (diffs only)
---@field new? string content after this turn (diffs only)
---@field adds? integer added lines (diffs only)
---@field dels? integer removed lines (diffs only)

---@class mare.ChangeTurn
---@field turn string
---@field label string
---@field shell boolean
---@field files mare.ChangeFile[]

-- --- Cells, message, spill, terminal, theme, web types ----------------------

---@class mare.SandboxServiceConfig: mare.SandboxOptions
---@field max_message_bytes? integer 0 inherits max_bytes (default)
---@field max_output_bytes? integer 0 inherits max_bytes (default)
---@field execution_mode? "per-call"|"persistent-in-memory"|"vm-script"
---@field script_persistence? boolean
---@field max_sub_calls_per_minute? integer
---@field max_parallel_sub_calls? integer

---@class mare.CellResult
---@field cell_id string
---@field status "running"|"completed"|"failed"|"cancelled"
---@field content any[]
---@field result? any
---@field error? string
---@field stop_unconfirmed? boolean

---@alias mare.MessageDeliverAs "steer"|"follow_up"|"next_turn"

---@class mare.CustomMessageDelivery
---@field trigger_turn? boolean
---@field deliver_as? mare.MessageDeliverAs
---@field wait? boolean

---@class mare.CustomMessageInput
---@field type string
---@field content string|table[]
---@field display? boolean
---@field details? any
---@field created_at_ms? integer
---@field delivery? mare.CustomMessageDelivery
---@field request_id? string

---@class mare.CustomMessageSpec
---@field validate? fun(msg: mare.CustomMessageInput): boolean?, string?
---@field render? fun(msg: table, opts: {expanded: boolean}): mare.Node?
---@field compact? fun(msg: table, opts: {expanded: boolean}): mare.Node?
---@field expanded? fun(msg: table, opts: {expanded: boolean}): mare.Node?

---@alias mare.SpillServiceRef {spill_id: string, size: integer, path: string, retrieval_hint: string, backend: string, source?: mare.SpillSource}

---@class mare.TerminalSessionSnapshot
---@field terminal_id string
---@field name? string
---@field pid? integer
---@field cols integer
---@field rows integer
---@field status "running"|"completed"|"failed"|"cancelled"
---@field exit_code? integer
---@field exit_signal? integer
---@field command_exit_code? integer
---@field output_offset integer
---@field truncated boolean
---@field screen? string
---@field view_truncated? boolean
---@field alternate_screen? boolean
---@field prompt_seq? integer

---@class mare.ThemeVariant
---@field syntax? string Valid tmTheme XML document, loaded with mare.fs.read if file-backed
---@field base? "dark"|"light"
---@field palette? table<string,string> #rrggbb values, known semantic palette keys only
---@field styles? table<string,mare.Style> Named styles; fg/bg must be #rrggbb

---@class mare.ThemeDefinition : mare.ThemeVariant
---@field name string Nonempty printable text, max 128 bytes; auto/dark/light are reserved
---@field icons? table<string,string> Printable glyph replacements, restored on theme changes
---@field label? string
---@field variants? {dark: mare.ThemeVariant, light: mare.ThemeVariant} Paired theme; both required

---@class mare.ThemeSnapshot
---@field preference string Confirmed flavor (auto or registered name); preview never changes it
---@field active string Resolved theme name
---@field appearance "dark"|"light"
---@field dark string auto mode's dark choice
---@field light string auto mode's light choice
---@field preview? string Temporary choice
---@field revision integer Monotonic within this provider lifecycle
---@field icons table<string,string> Active glyph replacements
---@field styles table<string,mare.Style> Effective named styles
---@field syntax? string Effective tmTheme XML for RPC/headless frontends
---@field component mare.Node JSON-safe sample component, usable by Lua and RPC frontends

---@class mare.WebCallContext
---@field cancel mare.Token
---@field cwd? string
---@field tool_call_id? string
---@field model? table

---@class mare.WebSearchResult
---@field data {published_at_ms?:integer,snippet?:string,summary?:string,title?:string,url:string}[]
---@field content? string
---@field truncated? boolean

---@class mare.WebFetchResult
---@field requested_url string
---@field url? string actual final URL, never guess from requested_url
---@field status? integer
---@field content_type? string
---@field content string
---@field truncated? boolean

---@class mare.WebSearchBackend
---@field provider_id string unique per kind; duplicate registrations fail
---@field unconfined? boolean callback runs outside the local process sandbox
---@field available? fun(model?:table):boolean metadata-only inspection; network and host mutation are refused in guarded inspection; failures mean unavailable
---@field search async fun(request:{queries:string[],mode:"cached"|"indexed"|"live",options?:table},ctx:mare.WebCallContext):mare.WebSearchResult

---@class mare.WebFetchBackend
---@field provider_id string
---@field unconfined? boolean
---@field available? fun(model?:table):boolean
---@field fetch async fun(request:{url:string},ctx:mare.WebCallContext):mare.WebFetchResult
