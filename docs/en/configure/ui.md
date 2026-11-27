# Appearance

Change how mare looks: named styles, themes and flavors, overriding UI components and tool renderers, the status line, borders and other appearance options.

## Styles

A style is a name such as `MareAccent` with a spec: foreground, background and text attributes. Set one with `mare.ui.style`; call it with one argument to read it.

```lua
mare.ui.style("MareAccent", { fg = "#7aa2f7", bold = true })
mare.ui.style("MareDim", "#5c6370")          -- a string is a foreground color
local spec = mare.ui.style("MareAccent")             -- read it back
```

A spec accepts `fg`, `bg` (a theme slot name or `#rrggbb`) and the booleans `bold`, `dim`, `italic`, `underline`, `strike`, `reverse`. Styles you set explicitly always win over a theme, and they are re-applied when the theme changes.

The `theme` option does the same for several styles at once. A string value is a foreground color.

```lua
mare.setup {
  theme = {
    MareToolName = { fg = "#7aa2f7", bold = true },
    MareDim = "#5c6370",
  },
}
```

The main style names:

| Name | Used for |
|---|---|
| `MareText`, `MareMuted`, `MareDim`, `MareSubtle` | Body text and its quieter levels. |
| `MareAccent`, `MareTitle`, `MareHeading` | Highlights, titles, Markdown headings. |
| `MareError`, `MareWarning`, `MareSuccess`, `MareInfo` | Status colors. |
| `MareUserMessage`, `MareUserPrompt`, `MareReasoning` | Your messages and model reasoning. |
| `MareToolName`, `MareToolArgs`, `MareToolOk`, `MareToolError`, `MareToolPending`, `MareToolResult` | Tool call lines. |
| `MareDiffAdd`, `MareDiffDelete`, `MareDiffAddWord`, `MareDiffDeleteWord` | Diffs. |
| `MareBorder`, `MareBorderActive`, `MareSelection` | Panels and the selected row. |
| `MareModeReadOnly`, `MareModeAuto`, `MareModeFull`, `MareModeCustom` | The permission capsule in the status line. |

`mare.ui.styles()` returns every name. In the TUI, `/lua mare.ui.styles()` prints them.

## Themes and flavors

A flavor is a complete set of styles for a dark or a light terminal. Choose one with these options:

| Option | Default | Meaning |
|---|---|---|
| `flavor` | `auto` | `auto`, `dark`, `light`, or the name of an installed theme. |
| `flavor_dark` | `dark` | The flavor `auto` picks on a dark or unknown background. |
| `flavor_light` | `light` | The flavor `auto` picks on a light background. |

With `auto`, mare asks the terminal for its background color after the first frame and switches. `/theme` opens a picker with a live preview; `/theme <name>` selects one directly. Both save the choice to `state.json`, and a `flavor` set in `init.lua` still wins on the next start.

```lua
mare.setup {
  flavor = "auto",
  flavor_dark = "night",     -- a theme you installed
  flavor_light = "light",
}
```

### Write a theme

Put a file `<name>.lua` (or a `.tmTheme` document for syntax highlighting only) in `~/.config/mare/themes/`. A package can ship the same files in its `themes/` directory. The file returns a table. The simplest form changes the palette of the dark or light base:

```lua
-- ~/.config/mare/themes/night.lua
return {
  base = "dark",                       -- "dark" or "light"
  palette = {
    accent = "#7aa2f7",
    accent2 = "#ff9e64",
    selection = "#2a2f45",
  },
}
```

Palette keys are `accent`, `accent2`, `text`, `muted`, `dim`, `subtle`, `border`, `selection`, `code_bg`, `green`, `yellow`, `red`, `blue`, `add_bg`, `del_bg`, `add_word`, `del_word` and `on_color`, each `#rrggbb`. mare derives the named styles from the palette. To control styles directly, return `styles = { MareAccent = { fg = "#7aa2f7" } }` instead of `palette`. A paired theme gives `variants = { dark = {...}, light = {...} }`; both are required, and `auto` then follows the terminal.

Without a file, define flavors in the `theme-default` plugin options:

```lua
mare.plugin("theme-default", {
  flavors = { night = { MareAccent = { fg = "#7aa2f7" } } },
  blend_background = false,   -- do not mix the palette with the terminal's background
})
```

`mare.plugin("theme-default", { dirs = { "~/themes" } })` replaces the directory that is searched.

## Override a component

The pieces of the transcript are Lua tables in `mare.ui` whose methods you can replace. `init.lua` runs after the defaults are installed, so an assignment wins.

```lua
-- replace how a tool call header looks
function mare.ui.ToolCall:header(summary, state)
  local mark = state == "ok" and "+" or state == "error" and "x" or "."
  local hl = state == "error" and "MareToolError" or "MareToolOk"
  return ui.Line({ ui.Span(mark .. " "):hl(hl), ui.Span(summary):hl("MareToolName") })
end

-- wrap a component instead of replacing it
local render = mare.ui.UserMessage.render
function mare.ui.UserMessage:render(text)
  return ui.Lines({ render(self, text), ui.Gap() })
end
```

Components you can override include `UserMessage`, `AssistantMessage` (`render`, `marker`), `Reasoning`, `TurnEnd`, `Notice`, `ToolCall`, `Status`, `Editor`, `Logo`, `Working`, and the overlay pieces `KeyHints`, `Tabs`, `Select` and `Confirm`. Components run when a message starts or finishes and when it is invalidated, never per streamed token.

`ui.Span`, `ui.Line`, `ui.Lines`, `ui.Gap`, `ui.Paragraph`, `ui.Row`, `ui.Block`, `ui.Markdown` and the rest build the nodes a component returns; the [Lua API](../extend/lua-api.md) page lists them.

### Tool renderers

`mare.ui.renderer(kind, { compact = fn, expanded = fn })` changes how one tool's finished result is drawn. `kind` is `"tool:<name>"`. `compact` is used in the scrollback and `expanded` in the transcript (`Ctrl+T`); both receive `(call, result)` and return a node. The function returns a remove function.

```lua
mare.ui.renderer("tool:bash", {
  compact = function(call, result)
    local command = call.arguments and call.arguments.command or "bash"
    return ui.Line({ ui.Span("$ "):hl("MareAccent"), ui.Span(command) })
  end,
})
```

The user and reasoning messages accept `message:user` and `message:reasoning` renderers the same way, taking `(text)` and `(text, secs)`.

Renderers and components set in `init.lua` are removed and set again on `/reload config`.

## Status line

The `statusline` plugin draws the bar under the input: permission capsule, model and effort, context gauge, directory, branch, pull request, changed files, cost. `/statusline` opens a picker to hide segments; the choice is saved in `state.json`. In `init.lua`:

```lua
mare.plugin("statusline", {
  hidden = { "pr", "cost" },     -- mode, model, effort, context, cwd, branch, pr, changes, cost, quota
  cwd_width = 32,
  gauge_width = 8,
  changed_files = true,
})
```

`layout` picks a different arrangement: `combined` (the default), `classic`, `codex` or `kimi`. `layout_opts` overrides single elements of it, and `looks` defines your own.

```lua
mare.plugin("statusline", {
  layout = "combined",
  layout_opts = { sep = "space", flash = 0, cwd_segments = 3 },
})
```

A look is data: `left` and `right` list the segments, `drop` lists the order in which they give way as the terminal narrows (`hints:short` shortens before dropping), and the other keys choose each element's form: `sep` (`dot` or `space`), `mode` (`word`, `capsule`, `dim`, `off`), `model` (`text`, `dim`, `muted`), `effort` (`reasoning`, `suffix`, `off`), `jobs` (`bracket`, `icon`), `context` (`kimi`, `left`, `gauge`), `context_color`, `hints`, `flash` (seconds a change stays visible) and `cwd_segments`.

To show the output of your own script instead, set `command` (a string run through the shell, or an argv table). It receives a JSON object on standard input (session id, cwd, width, model, context, cost) and its first output line is shown. `command_replaces = true` hides the built-in segments.

```lua
mare.plugin("statusline", { command = "~/bin/statusline.sh", command_timeout_ms = 500 })
```

Background-probed segments: `changes` (`git status`), `pr` (`gh pr view`), and external `command` output run asynchronously in the background after startup becomes ready (`ui/ready`) and refresh after turns so startup never blocks on subprocess probes (`branch` is read synchronously from `.git/HEAD` on the first frame and refreshed in the background after turns). Because external `command` runs only after `ui/ready`, it does not run in headless or RPC sessions without a UI. When a probe completes, those segments update their content in place. All other segments (`mode`, `hints`, `flags`, `model`, `effort`, `context`, `cwd`, `branch`, `cost`, `quota`, `peer_allow`, `tips`, `jobs`, `workflow`, `mcp`) render synchronously from the first frame onward.

To add a segment from your own plugin, use the `ui` service: `ctx.ui:status(id, { align = "right", order = 20, render = function() ... end })`. See [Writing a plugin](../extend/writing-a-plugin.md).

## Other appearance options

| Option | Values | Effect |
|---|---|---|
| `border` | `rounded` (default), `single`, `double`, `thick`, `hidden` | Border of panels and popups. |
| `icons` | `nerd`, `unicode`, `ascii` | Icon set (detected from the terminal when unset). Under `nerd`, callouts and fenced code block headers show Nerd Font icons (70+ language tags such as `rust`, `python`, `ts`, `go`, `lua`, `bash`, `sql`, `docker`, `yaml`). |
| `prompt` | a string, default `❯` | The input prompt symbol. |
| `terminal.ambiwidth` | `single`, `double` | Width of East Asian ambiguous characters, like Vim. |
| `spinner` | list of glyphs, or `{ frames = {...}, still = "•" }` | Glyphs of the spinners. |
| `ui.working` | `{ glyph, label, elapsed, tokens, interrupt_hint, sep, tools, interval_ms, text_throttle_ms, frames, glyphs, verbs }` | Fine-grained working indicator options (default `{ glyph = "swell", label = "glint", elapsed = "auto", tokens = true, interrupt_hint = true, sep = "dot", tools = "still", text_throttle_ms = 250 }`). |
| `ui.tool_call`, `ui.stopping` | `{ spinner = "braille" }` / `{ spinner = true }` | Tool-call and stopping spinners (`false` for static dot). |
| `markdown` | `{ glyphs = {...}, code_line_numbers = false }` | Markdown rendering (fenced code blocks show Nerd Font language icons automatically when `icons = "nerd"`). |
| `logo` | `true`, `false`, `"compact"` | Startup logo. |
| `reduce_motion` | boolean | Static text instead of animation. |
| `images_protocol` | `kitty`, `iterm2`, `sixel`, `none` | Terminal image protocol. Detected when unset. |
| `char_widths` | list of `{ from, to, width }` or `{ char, width }` | Display width overrides. |

```lua
mare.setup {
  border = "single",
  prompt = ">",
  icons = "nerd",
  ui = {
    working = {
      glyph = "swell",
      label = "glint",
      elapsed = "auto",
      tokens = true,
      interrupt_hint = true,
      sep = "dot",
      tools = "still",
      text_throttle_ms = 250,
    },
    tool_call = { spinner = "braille" },
  },
  markdown = { code_line_numbers = true },
  reduce_motion = true,
}
```

Each leaf of `ui.working` can be configured independently: `glyph` (`swell`, `ember`, `braille`, `moon`, `phase`, `wave`, `sail`, `bullet`, `pulse`, `none`, or a key in `glyphs`), `label` (`accent`, `shimmer`, `glint`, `foam`, `dots`), `elapsed` (`auto`, `seconds`, `off`), `tokens`, `interrupt_hint`, `sep` (`dot`, `bullet`), `tools` (`still`, `spin`), `interval_ms`, `text_throttle_ms`, inline `frames` / custom `glyphs`, and `verbs` to rename each phase (`reasoning`, `responding`, `tool`, `shell`, `exploring`). Everything holds still under `reduce_motion`. The [Options](options.md) page has the full list with defaults.

Plugin options that affect the transcript, such as `preview_lines` (lines of output shown per tool call in compact view) and `diff_fold_lines` (rows kept in a compact diff before folding, default `8`, or `false` to show full diffs), belong to the `ui` plugin:

```lua
mare.plugin("ui", { preview_lines = 4, diff_fold_lines = 12 })
```
