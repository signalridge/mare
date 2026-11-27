---@meta
-- Host-level webserver registry primitives backing the `webserver` service.

---@class mare.webserver
mare.webserver = {}

---@return { running: boolean, url?: string, ws_url?: string, host?: string, port?: integer, tls?: boolean, trusted_hosts?: string[] }
function mare.webserver.get() end

---@return { route_id: string, method: string, path: string, owner_session_id?: string, plugin?: string }[]
function mare.webserver.list_routes() end

---@return { asset_id: string, prefix: string, dir: string, plugin?: string, index?: string, fallback?: string }[]
function mare.webserver.list_assets() end

---@param owner_name string|nil
---@param path string
---@param opts? { method?: string, handle?: fun(req: table): table }
---@return fun()
function mare.webserver.register_route(owner_name, path, opts) end

---@param owner_name string|nil
---@param dir string
---@param opts? { prefix?: string, index?: string, fallback?: string }
---@return fun()
function mare.webserver.register_assets(owner_name, dir, opts) end
