-- Insertion-ordered Map and Set (Lua `pairs` has no defined order,
-- while JS Map / Set / object keys iterate in insertion order).
--
-- Implemented as a doubly linked list plus an index. As with JS Map, entries
-- deleted during iteration are skipped and entries appended during iteration
-- are visited.
local M = {}

---@class cordis.Map
local Map = {}
Map.__index = Map
Map.__name = "cordis.Map"
M.Map = Map

function Map.new()
  return setmetatable({ _index = nil, _head = nil, _tail = nil, _size = 0 }, Map)
end

function Map.is(x)
  return getmetatable(x) == Map
end

function Map:get(k)
  local idx = self._index
  local node = idx and idx[k]
  if node then
    return node.v
  end
end

function Map:has(k)
  local idx = self._index
  return idx ~= nil and idx[k] ~= nil
end

function Map:set(k, v)
  assert(k ~= nil, "Map key cannot be nil")
  local idx = self._index
  if not idx then
    idx = {}
    self._index = idx
  else
    local node = idx[k]
    if node then
      node.v = v
      return self
    end
  end
  local tail = self._tail
  local node = { k = k, v = v, prev = tail }
  if tail then
    tail.next = node
  else
    self._head = node
  end
  self._tail = node
  idx[k] = node
  self._size = self._size + 1
  return self
end

function Map:delete(k)
  local idx = self._index
  if not idx then
    return false
  end
  local node = idx[k]
  if not node then
    return false
  end
  idx[k] = nil
  node.dead = true
  if node.prev then
    node.prev.next = node.next
  else
    self._head = node.next
  end
  if node.next then
    node.next.prev = node.prev
  else
    self._tail = node.prev
  end
  -- keep `node.next` so that a live iterator positioned here can continue;
  -- if it was the tail, remember the predecessor to pick up later appends
  node.after = node.prev
  self._size = self._size - 1
  return true
end

function Map:clear()
  local node = self._head
  while node do
    node.dead = true
    node = node.next
  end
  self._index, self._head, self._tail, self._size = nil, nil, nil, 0
end

function Map:size()
  return self._size
end

local function advance(self, node)
  local nxt = node.next
  if nxt == nil and node.dead then
    -- `node` was the tail when deleted: continue from its nearest live
    -- predecessor so that entries appended since are still visited
    local p = node.after
    while p and p.dead do
      p = p.after
    end
    if p then
      nxt = p.next
    else
      -- no live predecessor: every live entry was appended after `node` died
      nxt = self._head
    end
  end
  return nxt
end

function Map:pairs()
  local node = nil
  local started = false
  return function()
    if not started then
      started = true
      node = self._head
    else
      node = node.dead and advance(self, node) or node.next
    end
    while node and node.dead do
      node = advance(self, node)
    end
    if node then
      return node.k, node.v
    end
  end
end

function Map:keys()
  local r = {}
  local n = 0
  local node = self._head
  while node do
    n = n + 1
    r[n] = node.k
    node = node.next
  end
  return r
end

function Map:values()
  local r = {}
  local n = 0
  local node = self._head
  while node do
    n = n + 1
    r[n] = node.v
    node = node.next
  end
  return r
end

function Map:entries()
  local r = {}
  local n = 0
  local node = self._head
  while node do
    n = n + 1
    r[n] = { node.k, node.v }
    node = node.next
  end
  return r
end

function Map:forEach(cb)
  local node = self._head
  while node do
    cb(node.v, node.k)
    node = node.dead and advance(self, node) or node.next
    while node and node.dead do
      node = advance(self, node)
    end
  end
end

---@class cordis.Set
local Set = {}
Set.__index = Set
Set.__name = "cordis.Set"
M.Set = Set

function Set.new(list)
  local s = setmetatable({ _map = Map.new() }, Set)
  if list then
    for _, v in ipairs(list) do
      s:add(v)
    end
  end
  return s
end

function Set.is(x)
  return getmetatable(x) == Set
end

function Set:add(v)
  self._map:set(v, true)
  return self
end

function Set:has(v)
  return self._map:has(v)
end

function Set:delete(v)
  return self._map:delete(v)
end

function Set:clear()
  self._map:clear()
end

function Set:size()
  return self._map:size()
end

function Set:values()
  return self._map:keys()
end

function Set:pairs()
  local map = self._map
  local node = nil
  local started = false
  return function()
    if not started then
      started = true
      node = map._head
    else
      node = node.dead and advance(map, node) or node.next
    end
    while node and node.dead do
      node = advance(map, node)
    end
    if node then
      return node.k
    end
  end
end

return M
