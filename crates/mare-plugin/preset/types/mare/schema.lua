---@meta _
--- Offline, bounded supported JSON Schema validation. Unsupported keywords fail at compile.
---@class mare.schema
mare.schema = {}
---@class mare.ToolSchema
local Schema = {}
---@param value any strict lossless JSON value, no unsupported/cyclic Lua data
function Schema:validate(value) end
---@return table detached schema snapshot
function Schema:value() end
---@return string canonical schema JSON
function Schema:fingerprint() end
--- Validate references against the original schema using a private native JSON copy. No raw
--- values are returned to Lua/callbacks, and no tool executes. Unknown/revoked references or a
--- stale revision fail closed. Execution validates again inside the approved sandbox.
---@param value any
---@param revision integer
function Schema:validate_projected(value, revision) end
---@class mare.OutputSchema
local OutputSchema = {}
---@return table
function OutputSchema:value() end
---@param value any
function OutputSchema:validate(value) end
---@param text string
---@return any
function OutputSchema:validate_text(text) end
--- Compile an object-root JSON Schema into a reusable tool argument validator.
---@param schema table object-root argument schema
---@return mare.ToolSchema
function mare.schema.compile(schema) end
--- Compile a tool output JSON Schema (or clone an existing `OutputSchema`).
---@param schema table|mare.OutputSchema
---@return mare.OutputSchema
function mare.schema.compile_output(schema) end
--- Canonical outputs may be any JSON type; does not relax argument-schema validation.
---@param schema table
---@return mare.ToolSchema
function mare.schema.compile_value(schema) end
--- Schema-directed removal, independent from key/pattern-based mare.text.redact.
--- Structural JSON Schema subset: properties, additionalProperties schema, items,
--- allOf/anyOf/oneOf. Annotate a secret node with ["x-mare-secret"]=true. Every union branch
--- is visited conservatively; duplicate paths merge set=true. Object slots include missing ones;
--- record/array slots include only existing entries. JSON null is set=true; Lua nil is unset.
--- Array removals leave JSON null preserving zero-based string indexes in sidecar paths.
--- Does not mutate input. Schema depth 64/nodes 10000; unsupported constructs (including
--- $ref/conditionals) and malformed annotations error rather than silently exposing values.
---@param schema table
---@param value any
---@return {value?: any, secrets: {path: string[], set: boolean}[]}
function mare.schema.redact(schema, value) end
