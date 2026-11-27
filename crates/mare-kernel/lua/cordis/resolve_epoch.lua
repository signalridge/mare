-- Resolution epoch: a counter of the changes that can alter what a
-- context property read resolves to (docs/deviations.md).
--
-- `reflect.lua` handler.get remembers the provision a (context, property) read
-- resolved to and reuses it while this counter is unchanged. Every input of that
-- resolution bumps it when it changes:
-- - every write through `proto` to an existing object (`set`, `defineOwn`,
--   `delete`, `setPrototypeOf`): the context chain's own properties, isolate
--   tables, a fiber's `committed` and `_store` objects, `inject` declarations
--   (proto objects, also where `registry.lua` adds one to a plugin class);
--   `create` initialises an object nothing refers to yet;
-- - the fiber fields `locate` reads (`inject`, `runtime`, `committed`) where
--   `fiber.lua` assigns them on an existing fiber (a new fiber is reached only
--   through a context, a proto write);
-- - a fiber's `provisions`, where `reflect.lua` adds or deletes one.
-- The provision's `value` is read from it on every read; `locate` does not read
-- the reflect `store`. A remembered name is declared as a service in
-- `reflect.props`, and stays one: `provide` refuses another type and `accessor`
-- refuses a declared name. `internal/get` listeners are checked on every read.
local M = { n = 0 }

function M.bump()
  M.n = M.n + 1
end

return M
