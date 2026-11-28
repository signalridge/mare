---
name: plugin-authoring
description: Write a local Lua plugin tool for mare, register it in an approved local dev package, call it on the next model request, iterate with /reload, and verify failed-reload rollback. Use when asked to write, create, edit, load, or reload a local mare plugin or agent tool.
---

# Writing and reloading a local mare plugin tool

Use only mare's public plugin entry points (`mare.plugins.add`, `ctx.tools:register`, `/reload`, `/plugins`, and `mare test`). Every tool registered by a plugin goes through the same schema check, `tool/before` vote, and permission/approval pipeline as built-in tools.

## 1. Write the plugin into an approved local dev package

Confirm or use an approved local package directory (for example `~/code/my-tools` or a workspace dev directory approved by the user). A local package needs a `plugins/` folder and optionally `mare-plugin.toml` and `tests/`:

```text
my-tools/
  plugins/
    local_tagger.lua     # entry file (or plugins/local_tagger/init.lua)
  mare-plugin.toml       # optional metadata (name, version, requires)
  tests/
    local_tagger_spec.lua
```

Declare the local directory in `~/.config/mare/init.lua` (or in trusted `.mare/init.lua`) with `mare.plugins.add`:

```lua
mare.plugins.add {
  { dir = "/path/to/my-tools" },
}
```

Local `{ dir = "..." }` packages load straight from disk and are never copied or written to `mare-lock.json`.

## 2. Register the tool

Return a plugin definition table whose `apply(ctx, cfg)` registers tools on `ctx.tools`:

```lua
-- plugins/local_tagger.lua
return {
  name = "local_tagger",
  inject = { "tools" },
  apply = function(ctx)
    ctx.tools:register("tag_text", {
      snippet = "Tag text with a versioned prefix",
      description = "Return the input text prefixed with a tag.",
      parameters = {
        type = "object",
        properties = {
          text = { type = "string", description = "Text to tag" },
        },
        required = { "text" },
      },
      side_effect = "read",
      summary = function(args)
        return ("TagText(%s)"):format(tostring(args.text or ""))
      end,
      execute = function(args)
        if type(args.text) ~= "string" then
          return { content = "text must be a string", is_error = true }
        end
        return { content = "v1:" .. args.text }
      end,
    })
  end,
}
```

Rules:

- **Permissions & safety:** Set `side_effect` accurately (`"read"`, `"write"`, or `"external"`). Set `readonly = true` only when the tool has no side effects. Validate `args` and return `{ content = "...", is_error = true }` on bad input.
- **Lifetime & subagent child VMs:** Do no side-effectful work at module top level. Register tools, commands, and event listeners inside `apply(ctx)` so mare withdraws them automatically on dispose or reload. Subagents spawned in the same trusted workspace inherit project trust and run `apply(ctx)` again in a child VM (`mare.mode == "child"`). Register tools and permission hooks unconditionally so subagents can use and obey them, and guard parent-only startup actions (`app/ready`, background automation) with `if mare.mode == "child" then return end`.
- **Reload boundary:** `/reload <name>` re-reads the plugin's entry file (`plugins/<name>.lua` or `plugins/<name>/init.lua`), whereas modules loaded via `require(...)` remain cached in `package.loaded`. Keep code you iterate on in the plugin entry file (or use a full `/reload`).

## 3. Load and call it on the next request

Run `/reload` once after adding `{ dir = "..." }` to `init.lua` (or after adding a new plugin file to an already-declared local package) so mare discovers and loads the plugin.

Once active:

- `/plugins` (or `mare.plugins.status("local_tagger")`) reports the plugin as active.
- The registered tool (`tag_text`) is offered automatically on the very next model request. Call it to verify v1 behavior (`"v1:hello"`).
- Optionally add automated lifecycle tests in `tests/local_tagger_spec.lua` using `require("mare.test")` and run `mare test`.

## 4. Edit and `/reload`

Edit `plugins/local_tagger.lua` (for example changing `"v1:"` to `"v2:"`), then run:

```text
/reload local_tagger
```

(Use `/reload` with no arguments when `init.lua` or package declarations also changed.)

Call `tag_text` on the next model request and confirm the updated output (`"v2:hello"`).

## 5. Break it on purpose and confirm rollback

To verify mare protects the live session against broken edits:

1. Introduce a deliberate syntax or load error in `plugins/local_tagger.lua` (for example `return { name = "local_tagger", broken syntax !!!`).
2. Run `/reload local_tagger` (or `/reload`).
3. Inspect the reported load error (notice / `/plugins` / `mare.plugins.status("local_tagger")`).
4. Call `tag_text` again on the next request and confirm the **previous working version (v2)** is still active and returns `"v2:..."`.
5. Restore valid Lua and run `/reload local_tagger` to resume iterating.
