---@meta _
-- GENERATED from types/mare/services/contracts/compact.lua (service `compact`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.CompactService
local M = {}

--- Whether compaction or a branch summary is currently running.
---@return boolean
function M:busy() end

--- Stop a running compaction.
---@return table
function M:cancel() end

--- Current usage: the larger of the local estimate and provider-reported usage (or a per-source breakdown table when `{ by_source = true }`).
---@param opts? {by_source?: boolean, [string]: any}
---@return integer|table
function M:measure(opts) end

--- The offload stage on its own, for a provider's image_limit error.
---@param opts? {keep?: integer}
---@return integer|nil
---@return string|nil
function M:offload_images(opts) end

--- Measure a prepared request and compact its uniquely mapped session history when needed. The budget uses the protocol-independent request and the output cap from its resolved parameter mappings; later `provider/request` changes to the output cap are not included.
---@param request mare.LlmRequest
---@param history table<string, any>[]
---@return {compacted: boolean, messages: table<string, any>[]}
function M:prepare_request(request, history) end

--- Add a stage or replace a built-in one; order comes from the `stages` option. Withdrawn when the caller is disposed or the returned disposer is called.
---@param name string
---@param fn async fun(state: table): boolean, string|nil
---@param opts? {summary?: boolean}
---@return fun() dispose releases this registration
function M:register_stage(name, fn, opts) end

--- Compact the session's context now (`reason`: manual by default); `instructions` adds a focus to the summary. Returns `{status, tokens_before, tokens_after}` (or `{status = "summarized", tokens_before = 0, tokens_after = 0, summary, dropped}` with `summarize_only`). A failure raises `compact.not_compacted`.
---@param instructions? string
---@param opts? {messages?: table<string, any>[], reason?: "manual", summarize_only?: boolean, cancel?: (fun(): mare.Token|nil)}
---@return {dropped?: integer, status: "compacted"|"nothing_to_compact"|"unchanged"|"summarized", summary?: string, tokens_after: integer, tokens_before: integer}
function M:run(instructions, opts) end

--- Tokens at which auto compaction triggers for a model (default: current); nil when `auto` is off or budgets are invalid.
---@param model? mare.Model
---@return integer|nil
function M:threshold(model) end
