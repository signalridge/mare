---@meta _
-- GENERATED from types/mare/services/contracts/webserver.lua (service `webserver`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.WebserverService
local M = {}

--- Status and bound endpoints of the host webserver listener.
---@return {host?: string, port?: integer, running: boolean, tls?: boolean, trusted_hosts?: string[], url?: string, ws_url?: string}
function M:get() end

--- Registered static asset mounts on the host webserver.
---@return {data: {asset_id: string, dir: string, fallback?: string, index?: string, plugin?: string, prefix: string}[]}
function M:list_assets() end

--- Registered HTTP routes on the host webserver.
---@return {data: {method: string, owner_session_id?: string, path: string, plugin?: string, route_id: string}[]}
function M:list_routes() end

--- Register a static asset directory served under a URL prefix on the host webserver.
---@param dir string
---@param opts? {fallback?: string, index?: string, prefix?: string}
---@return fun() dispose releases this registration
function M:register_assets(dir, opts) end

--- Register a named HTTP route on the host webserver. Requests are forwarded to the caller handler.
---@param path string
---@param opts? {method?: "GET"|"POST"|"PUT"|"PATCH"|"DELETE"|"HEAD"|"ANY", handle?: (fun(req: table): table)}
---@return fun() dispose releases this registration
function M:register_route(path, opts) end
