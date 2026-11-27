---@meta _
-- LuaLS types for `mare.Service` (mare_kernel/service.lua): the one
-- recognised way to provide a service. A `cordis.Service` subclass: declared methods are wrapped with
-- their contract guard at construction; the published value is the traceable instance itself.
-- Providers write `cordis.class("X", mare.Service)` and call `X.super.constructor(self, ctx, "x")`.

---@class mare.Service : cordis.Service
mare.Service = {}

--- Construct and provide service `name` from `ctx` under its contract (admitted before anything is
--- published; a refusal fails the providing fiber).
---@param ctx cordis.Context
---@param name string
---@param options? mare.ServiceProvideOptions
function mare.Service.constructor(self, ctx, name, options) end

--- A new instance (`cordis.class` constructor form).
---@param ctx cordis.Context
---@param ... any
---@return mare.Service
function mare.Service.new(ctx, ...) end

--- Whether `value` is an instance of this class.
---@param value any
---@return boolean
function mare.Service.is(value) end
