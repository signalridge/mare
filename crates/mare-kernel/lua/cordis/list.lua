-- Ported from cordis packages/core/src/utils.ts (DisposableList, `:8-50`) (MIT, (c) Shigma)
local Map = require("cordis.ordered").Map

---An ordered list whose entries are removed through the disposers returned on insertion.
---@class cordis.DisposableList
local DisposableList = {}
DisposableList.__index = DisposableList
DisposableList.__name = "cordis.DisposableList"

local WEAK_KEYS = { __mode = "k" }

local function emptyIter()
  return nil
end

function DisposableList.new()
  return setmetatable({
    sn = 0,
    head = nil,
    tail = Map.new(),
    weak = setmetatable({}, WEAK_KEYS),
  }, DisposableList)
end

function DisposableList.is(x)
  return getmetatable(x) == DisposableList
end

function DisposableList:length()
  local head = self.head
  return (head and head._size or 0) + self.tail._size
end

function DisposableList:push(value)
  self.sn = self.sn + 1
  local sn = self.sn
  local tail = self.tail
  tail:set(sn, value)
  self.weak[value] = sn
  return function()
    return tail:delete(sn)
  end
end

function DisposableList:unshift(value)
  self.sn = self.sn + 1
  local sn = -self.sn
  local head = self.head
  if not head then
    head = Map.new()
    self.head = head
  end
  head:set(sn, value)
  self.weak[value] = sn
  return function()
    return head:delete(sn)
  end
end

function DisposableList:delete(value)
  local sn = self.weak[value]
  if not sn then
    return false
  end
  if sn < 0 then
    local head = self.head
    return head ~= nil and head:delete(sn)
  end
  return self.tail:delete(sn)
end

function DisposableList:clear()
  local values = self:values()
  if self.head then
    self.head:clear()
  end
  self.tail:clear()
  return values
end

---Entries in order: unshifted ones (most recent first), then pushed ones.
function DisposableList:values()
  local r = {}
  local n = 0
  local head = self.head
  if head then
    local node = head._tail
    while node do
      n = n + 1
      r[n] = node.v
      node = node.prev
    end
  end
  local node = self.tail._head
  while node do
    n = n + 1
    r[n] = node.v
    node = node.next
  end
  return r
end

function DisposableList:pairs()
  if self:length() == 0 then
    return emptyIter
  end
  local values = self:values()
  local i = 0
  return function()
    i = i + 1
    return values[i]
  end
end

DisposableList.__len = DisposableList.length

return DisposableList
