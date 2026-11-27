---@meta _
-- GENERATED from types/mare/services/contracts/web.lua (service `web`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.WebService
local M = {}

--- Fetch a URL using the currently selected fetch backend under active sandbox limits.
---@param url string
---@param opts? {timeout_ms?: integer, cancel?: (fun(): mare.Token|nil)}
---@return mare.WebFetchResult
function M:fetch(url, opts) end

--- Registered web backends of `kind` sorted by provider_id, each marked `selected` when active.
---@param kind "search"|"fetch"
---@return {data: {available: boolean, provider_id: string, selected: boolean, unconfined: boolean}[], reason?: string}
function M:list_providers(kind) end

--- Register a fetch backend owned by the caller; removed on release or disconnect.
---@param opts {provider_id: string, unconfined?: boolean, available?: (fun(model?: table): boolean), fetch?: (fun(request: table, ctx?: mare.WebCallContext): mare.WebFetchResult)}
---@return fun() dispose releases this registration
function M:register_fetch(opts) end

--- Register a search backend owned by the caller; removed on release or disconnect.
---@param opts {provider_id: string, unconfined?: boolean, available?: (fun(model?: table): boolean), search?: (fun(request: table, ctx?: mare.WebCallContext): mare.WebSearchResult)}
---@return fun() dispose releases this registration
function M:register_search(opts) end

--- Search the web using the currently selected search backend under active sandbox limits.
---@param queries string[]
---@param opts? {timeout_ms?: integer, cancel?: (fun(): mare.Token|nil)}
---@return mare.WebSearchResult
function M:search(queries, opts) end
