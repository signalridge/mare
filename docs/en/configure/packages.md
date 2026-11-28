# Packages

Install plugins, themes and skills from git with mare.plugins.add: declaring and pinning versions, the lock file, the mare plugin commands and startup behavior.

## Declare packages

`mare.plugins.add` takes a list. Each entry is a string or a table.

```lua
-- ~/.config/mare/init.lua
mare.plugins.add {
  "someone/mare-git-tools",                              -- newest semver tag, else the default branch
  { "someone/mare-extras", version = "^1.2" },           -- a semver range over the tags
  { "someone/monorepo:mare", only = { "tidy" } },        -- a subdirectory; load only some plugins
  { "git@example.com:team/internal-tools.git", branch = "main" },
  { dir = "~/code/my-plugin" },                          -- a local directory, never installed
  { import = "packages" },                               -- more declarations from lua/packages.lua
}
```

A short name `owner/repo` expands through the `plugins.url_format` option (default `https://github.com/%s.git`). A full address works too: a URL, an `ssh` address, or a path. Only `https`, `ssh` and `file` transports are used, and git never prompts for a password. Plain `http://` and `git://` need `plugins = { allow_insecure = true }`.

Fields of a table entry:

| Field | Meaning |
|---|---|
| `[1]` | The source: `owner/repo`, `owner/repo:subdir` or a git address. |
| `name` | A display name. With no `[1]`, it is the source. |
| `only` | Names of the plugins to load from the package. The rest are ignored. |
| `subdir` | A subdirectory of the repository (the same as `owner/repo:subdir`). |
| `version`, `tag`, `branch`, `commit`, `rev` | What to check out. See below. |
| `pin` | `true`: `update` leaves it where it is. |
| `submodules` | `false`: do not fetch git submodules. |
| `dir` | A local directory, loaded from there, never installed or locked. |
| `dependencies` | Packages that load first (strings or full entries). |
| `enabled` | `false`, or a function returning a boolean. A disabled package is neither installed nor loaded; its lock entry stays. |
| `required` | `true`: failure to install or load this package blocks model requests until repaired and `/reload`ed (separate from `optional`, which only affects declaration merging). |
| `optional` | Declaration-merge only: the entry counts only if the package is declared elsewhere too. |
| `opts` | Per-plugin defaults, `{ pluginname = { ... } }`, or a function that returns them. |
| `opts_extend` | Dotted paths in `opts` whose lists are appended across declarations instead of replaced. |
| `init` | `function(pkg)`, runs before the package's plugins load. |
| `config` | `function(pkg, opts)`, runs after its plugins load, and again on `/reload`. |
| `import` | A module under `lua/` that returns more declarations. |

A package declared more than once is merged: the last value wins for single fields, `opts` merge deeply, `dependencies` and `only` accumulate, and the last explicit `enabled` value wins (omitting `enabled` in a later declaration preserves the previous value). Options from `opts` sit under `mare.plugin`: a value you set with `mare.plugin` always wins.

```lua
mare.plugins.add {
  {
    "someone/mare-extras",
    version = "^1.2",
    opts = { tidy = { max_lines = 80 } },
    config = function(pkg, opts)
      mare.log("info", pkg.name .. " ready")
    end,
  },
}
```

### Split declarations across files

`{ import = "packages" }` loads `lua/packages.lua`, `lua/packages/init.lua`, every `lua/packages/*.lua`, and every `lua/packages/<subdir>/init.lua`. Each file returns a list of declarations. Imports nest up to 16 levels, each module is imported once, and a cycle is skipped with a warning.

```lua
-- ~/.config/mare/lua/packages/git.lua
return {
  "someone/mare-git-tools",
  { "someone/mare-extras", enabled = function() return mare.os ~= "windows" end },
}
```

## Pin versions

The first of these fields present decides the target, in this order:

| Field | Target |
|---|---|
| `commit = "<sha>"` | That commit. `update` never moves it. |
| `tag = "v1.4.0"` | That tag. |
| `branch = "main"` | The head of that branch. |
| `version = "^1.2"` | The newest tag inside the semver range. |
| `rev = "<ref>"` | Any git revision. `rev = "=<sha>"` is an exact pin. |
| none | The newest semver tag, else the default branch. |

Add `pin = true` to hold any package where it is until you change the declaration. Add `auto_update = true` (default `false`) on a source to let periodic background checks fetch updates and queue the checkout switch for the next `/reload` or start (never mid-session under loaded plugins).

By default, background checks in the TUI notify once per plugin per new revision and suggest `mare update --plugins` (`MARE_NO_UPDATE_CHECK=1` disables background checks; `MARE_OFFLINE=1` also blocks `mare plugin check`/`update` and remote clones on startup while keeping installed packages usable).

## The lock file

`~/.config/mare/mare-lock.json` (and `.mare/mare-lock.json` for project-scoped packages in a trusted project) records, for each package, its URL, the commit, the constraint it was locked for, the branch it follows, and a content hash of the checkout. Commit it with your dotfiles or repository to get the same packages on every machine. mare writes it after each package finishes, so a failed package does not lose the others. A corrupt lock file stops normal package commands without modifying checkouts; run `mare plugin restore --repair` (or `mare doctor --fix`) to rebuild it from clean installed checkouts, or fix or delete the file and run `mare plugin update`.

Selections in `plugins.json` use version 2: `{ "version": 2, "plugins": [{ "source": "owner/repo", "name": "plugin" }] }`. Shared checkouts are kept while any user or project scope still references them.

Checkouts live in `~/.local/share/mare/packages/<host>/<path>`.

## Commands

| Command | Does |
|---|---|
| `mare plugin sync` | Install missing packages at the locked commit. If the constraint in `init.lua` changed, it resolves again. |
| `mare plugin update [name]` | Move packages (or one) to the newest revision their constraint allows, list the new commits, rewrite the lock. Exact pins stay. |
| `mare plugin check [name...] [--json]` | Fetch and compare revisions without moving checkouts or changing the lock. |
| `mare plugin restore [name]` | Check out all sources, or the named one, at the locked commit. The lock is unchanged. |
| `mare plugin restore --repair` | Rebuild a missing or corrupt lock from clean installed checkouts. |
| `mare plugin list [--json]` | List declared sources with their state. |
| `mare plugin tree [--json]` | Show the composed runtime plugin tree. |
| `mare plugin install <name...>` | Enable catalog plugins by short name and install their required sources. |
| `mare plugin uninstall <name...>` | Disable catalog plugins by short name and clean unused sources. |
| `mare plugin clean` | Remove checkouts no longer declared and stale lock entries. |

```sh
mare plugin sync
mare plugin update someone/mare-extras
mare plugin list
```

Fetches run in parallel (`plugins.concurrency`, default 8), each git operation has a timeout (`plugins.timeout`, default 120 seconds), and Ctrl-C cancels them. A checkout with local edits is flagged and never deleted by `clean`. `mare plugin` commands resolve project trust from `--trust`, `--no-trust`, or your saved trust decision: in a trusted project they also load `.mare/init.lua` (executing its Lua) and can use `.mare/mare-lock.json`. Pass `--no-trust` if you want to operate on user-level config only.

Inside the TUI, `/plugins` opens a panel with the same operations for everything or for one package, plus log and diff views, and a Discover tab to turn catalogued packages on and off (stored in `~/.config/mare/plugins.json`). It also reports the state of each row: installed, behind or ahead of the lock, local changes, missing, disabled, local directory, or not loaded and why. The `plugins` options are in [Options](options.md):

On the first interactive open, mare offers to fetch the official plugin catalog and let you choose optional plugins; nothing is preselected. You can skip and use `/plugins` → Discover later. Disable this offer with `mare.plugin("onboarding", { plugins = false })`; delete `onboarding.plugins` from `state.json` to offer it again when `plugins.json` is absent.

```lua
mare.setup {
  plugins = { url_format = "https://git.example.com/%s.git", timeout = 60, concurrency = 4 },
}
```

## Startup and reload

At startup, before plugins load, mare installs declared packages that are missing, at the locked commit, and shows progress in the TUI. Then it runs each package's `init` hook, loads plugins from each installed package in dependency order, and runs each package's `config` hook last.

A required package, its hooks, or a core or explicitly required plugin can block requests until repaired and reloaded. An optional package or plugin failure is reported as a warning; other plugins and sessions continue. A failed provider blocks requests that use its models.

Package changes (a sync, an update, an edit to `init.lua`) apply on `/reload`: new packages are installed and started, removed ones are unloaded, and `config` hooks run again. mare also notices when `init.lua` or `plugins.json` changes on disk and tells you to run `/reload`.

`--clean` skip packages entirely.

## What a package contains

| Directory | Loaded by |
|---|---|
| `plugins/` | The plugin loader: `name.lua` or `name/init.lua` files. See [Plugins](../extend/plugins.md). |
| `skills/`, `prompts/` | Context: skills and prompt templates. |
| `themes/` | Theme files. See [Appearance](ui.md). |
| `types/` | LuaLS definitions, mirrored for your editor. |

An optional `mare-plugin.toml` at the root can set `requires = ">=0.4"` (a package that needs a newer mare does not load), `dependencies`, `name`, `description` and `version`. To publish your own package, push a repository with these directories and tag releases with semver tags such as `v1.0.0`. [Writing a plugin](../extend/writing-a-plugin.md) shows the layout.
