---@meta _
-- GENERATED from types/mare/services/contracts/context.lua (service `context`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.ContextService
local M = {}

--- Fill a template body with shell-style split arguments ($1, $@, ${1:-default}...).
---@param body string
---@param args? string
---@return string
function M:expand_template(body, args) end

--- The context files read into the prompt: {path, content}.
---@return {content: string, path: string, [string]: any}[]
function M:files() end

--- Drop the cached prompt; the next request assembles it again.
---@return table
function M:invalidate() end

--- Preview the assembled prompt with candidate sections and section diagnostics without modifying the registry.
---@param model? mare.Model
---@param agent? string
---@param opts? {replace?: boolean, suppress_runtime?: boolean, sections?: mare.PromptSection[]?}
---@return mare.PromptPreview
function M:preview(model, agent, opts) end

--- Whether SYSTEM.md or the `system` option replaces the whole default prompt, else nil.
---@return string|nil
function M:replaced() end

--- Per-source /context report table.
---@return table<string, any>
function M:report() end

--- Register a prompt section (`name`, `order`, `text`, `when`, `scope`, ...); released with the caller or returned disposer.
---@param def mare.PromptSection
---@return fun() dispose releases this registration
function M:section(def) end

--- What the last skill scan warned about or skipped: {path, message, skipped?}.
---@return {message: string, path: string, skipped?: boolean, [string]: any}[]
function M:skill_problems() end

--- The skills offered to the model: {name, description, path}.
---@return {description: string, name: string, path: string, [string]: any}[]
function M:skills() end

--- Hide a section by name in every scope until released; sections registered under that name later stay hidden too.
---@param name string
---@return fun() dispose releases this registration
function M:suppress(name) end

--- Hide non-required runtime sections (`runtime = true`, not `required = true`) in the matching layer until released.
---@param opts? {agent?: string, scope?: "global"|"user"|"agent"}
---@return fun() dispose releases this registration
function M:suppress_runtime(opts) end

--- The assembled prompt (cached per model and agent); raises when it can't be built.
---@param model? mare.Model
---@param agent? string
---@return string
function M:system_prompt(model, agent) end

--- Prompt templates, each registered as /name unless a command with that name exists.
---@return {name: string, path: string, [string]: any}[]
function M:templates() end

--- Register a `{{name}}` for interpolated sections (name: ^[a-z][a-z0-9_]*$) until released.
---@param name string
---@param fn fun(info: {agent?: string, model?: table, scope: string}): any
---@return fun() dispose releases this registration
function M:variable(name, fn) end
