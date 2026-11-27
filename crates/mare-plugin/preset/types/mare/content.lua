---@meta _
-- LuaLS definitions: unified media attachments and content parts (entry: types/mare.lua).

---@alias mare.AttachmentKind "file"|"image"|"video"|"audio"

---@class mare.Attachment
---@field kind? mare.AttachmentKind
---@field path? string
---@field data? string base64 media payload (with `mime`)
---@field mime? string
---@field url? string media URL or `data:` URI
---@field name? string

---@class mare.ContentPart
---@field type "text"|"image"|"video"|"audio"|"file"
---@field text? string
---@field data? string
---@field mime? string
---@field url? string
---@field path? string
---@field name? string
