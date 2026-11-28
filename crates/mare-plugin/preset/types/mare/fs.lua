---@meta _
-- LuaLS definitions: files, file index, file locks and the snapshot journal (entry: types/mare.lua)

---@class mare.fs
mare.fs = {}
---@type string
mare.fs.cwd = ""
---@type string|nil
mare.fs.home = nil
---@type string
mare.fs.sep = "/"
---@param p string
---@return string
function mare.fs.resolve(p) end
--- `p` without the working directory in front when it lies below it, else `p` as given
--- (Windows: separators, case and `\\?\` folded; canonical forms compared as a fallback).
---@param p string
---@return string
function mare.fs.relative(p) end
---@param p string
---@param opts? {max_bytes?:integer, no_follow?:boolean, policies?:any[], cwd?:string} Reject oversized files before reading and bound concurrent growth
---@return string|nil content, string|nil err
function mare.fs.read(p, opts) end
--- Read a 1-based line slice of `p`, returning `{lines, total, size, hash}` or `nil, err`.
---@param p string
---@param opts? {offset?: integer, limit?: integer}
---@return {lines: string[], total: integer, size: integer, hash: string}|nil window, string|nil err
function mare.fs.read_window(p, opts) end
---@param p string
---@param data string
--- `sync_boundary`: for atomic writes, also sync ancestor entries up to this vouched durable directory.
--- `atomic`: temp file + fsync + rename (keeps an existing file's permissions); `create_new`:
--- fail if the path exists (with atomic: hard-linked into place, never clobbers).
---@param opts? {mode?: integer, append?: boolean, atomic?: boolean, create_new?: boolean, sync_boundary?: string, root?: string, policies?: any[], cwd?: string, capture_snapshot?: boolean}
---@return boolean|nil ok, string|nil err
function mare.fs.write(p, data, opts) end
--- Canonical path (symlinks resolved), or the absolute path when it doesn't exist yet.
---@param p string
---@return string
function mare.fs.realpath(p) end
---Resolve the logical and physical forms of a path, failing if its identity is ambiguous.
---@param path string
---@param base? string
---@return {logical: string, physical: string, identity: string}
function mare.fs.resolve_path(path, base) end
---Current project resource root, or the session working directory when no root is present.
---@return string
function mare.fs.project_root() end
---@class mare.PathGlob
---@field matches fun(self: mare.PathGlob, path: string, conservative?: boolean): boolean
---@field overlaps fun(self: mare.PathGlob, directory: string): boolean
---@field covers_recursive fun(self: mare.PathGlob, directory: string, conservative?: boolean): boolean
---@field literal fun(self: mare.PathGlob): string|nil
---Compile a separator-aware absolute or base-relative path glob.
---@param pattern string
---@param base? string
---@return mare.PathGlob|nil, string|nil
function mare.fs.compile_path_glob(pattern, base) end
---@param path string
---@param root string
---@param conservative? boolean
---@return boolean
function mare.fs.path_within(path, root, conservative) end
---@param p string
---@return boolean
function mare.fs.exists(p) end
--- Return the validated Git directory of the worktree at `p` (Git's rules: a `.git` directory or
--- gitfile naming one with a valid HEAD, objects and refs). nil when there is none or the `.git`
--- directory is not valid (discovery goes on upward); nil, "invalid gitfile" for a `.git` file
--- that names no valid Git directory (Git's discovery stops there: not a repository). Inside a
--- tool call, an unreadable metadata path raises like the other fs functions.
---@param p string
---@return string|nil gitdir
---@return "invalid gitfile"|nil stop
function mare.fs.git_dir(p) end
--- Synchronously read the current Git branch name (or 7-character detached HEAD commit prefix) by
--- walking upward from `p` (default: working directory) to the nearest validated `.git` directory
--- or gitfile and reading `HEAD`. Returns nil outside a Git repository or at an invalid `.git`
--- file. Inside a tool call, an unreadable metadata path raises like the other fs functions.
---@param p? string
---@return string|nil branch
function mare.fs.git_branch(p) end
---@overload fun(p: string): {kind: "file"|"dir"|"other", size: integer, mtime: number, ctime: integer, mode?: integer, nlink?: integer}|nil
---@param p string
---@param opts? {policies?: any[], cwd?: string, strict?: boolean}
---@return {kind: "file"|"dir"|"other", size: integer, mtime: number, ctime: integer, mode?: integer, nlink?: integer}|nil info, string|nil err `mode`: Unix permission bits (absent on Windows); `nlink`: hard-link count (Unix)
function mare.fs.stat(p, opts) end
--- Stable cross-platform file identity (`dev:ino` on Unix; volume/index on Windows) when `p` exists.
---@param p string
---@return string|nil
function mare.fs.identity(p) end
--- Async bounded content sniff for `p` (`binary`, `"text"`/`"empty"`/`"image"`/`"pdf"`/`"binary"`, and byte `size`).
---@async
---@param p string
---@return {binary: boolean, kind: string, size: integer}|nil info, string|nil err
function mare.fs.sniff(p) end
---@param p string
function mare.fs.mkdir(p) end
---@async
---@param p string
---@param recursive? boolean
function mare.fs.remove(p, recursive) end
---@param a string
---@param b string
function mare.fs.rename(a, b) end
---@class mare.FsListEntries
---@field [integer] {name: string, kind: "file"|"dir"|"other"}
---@field truncated? boolean

---@overload fun(p: string): {name: string, kind: "file"|"dir"|"other"}[]
---@param p string
---@param opts? {strict?: boolean, no_follow?: boolean, policies?: any[], cwd?: string, max_entries?: integer, filter_denied?: boolean} When `strict` is true, return `nil, err` on `read_dir` / entry errors or policy-filtered entries instead of an empty/partial list
---@return mare.FsListEntries|nil entries, string|nil err
function mare.fs.list(p, opts) end
---@async
---@param dir? string
---@param opts? {hidden?: boolean, gitignore?: boolean, max_depth?: integer, limit?: integer, glob?: string|string[], dirs?: boolean}
---@return string[]
function mare.fs.walk(dir, opts) end

---@class mare.FileIndex
local FileIndex = {}
--- Build on first use, later re-list only the directories whose mtime changed (at most every
--- `min_interval_ms`, default 1000). Returns a generation that changes with the file list.
---@async
---@param opts? {min_interval_ms?: integer}
---@return integer
function FileIndex:refresh(opts) end
---@return string[]
function FileIndex:files() end
--- An incrementally maintained, gitignore-aware file list (the `@` completion index).
---@param dir? string
---@param opts? {hidden?: boolean, gitignore?: boolean, dirs?: boolean, limit?: integer}
---@return mare.FileIndex
function mare.fs.index(dir, opts) end

--- Watch `path` for policy-filtered filesystem changes, invoking `on_change({ changed_paths, truncated })`.
---@async
---@param path string
---@param on_change fun(ev: { changed_paths: string[], truncated: boolean })
---@return { truncated: boolean, stop: fun() }
function mare.fs.watch(path, on_change) end

---@class mare.FileLockGuard
local FileLockGuard = {}
function FileLockGuard:release() end
--- Cross-process locks (e.g. credential refreshes shared by sub-agents).
---@class mare.lock
mare.lock = {}
--- Exclusive lock on `path` (created if missing); `cancel` / `timeout_ms` only end the wait.
---@async
---@param path string
---@param opts? {timeout_ms?: number, cancel?: mare.Token, root?: string}
---@return mare.FileLockGuard
function mare.lock.file(path, opts) end
--- Exclusive cross-runtime lock keyed by an opaque string in a per-user, profile-independent lock directory.
---@async
---@param key string
---@param opts? {timeout_ms?: number, cancel?: mare.Token}
---@return mare.FileLockGuard
function mare.lock.key(key, opts) end

--- File snapshot journal: core backs files up before tools write, remove or
--- rename them. The `changes` plugin points it at the session and starts turns.
---@class mare.snapshot
mare.snapshot = {}
---@param opts {dir: string, cap_bytes?: integer}|nil nil turns it off
function mare.snapshot.configure(opts) end
---@param id string later captures belong to this turn
function mare.snapshot.begin_turn(id) end
--- Record that the active turn ran a shell command (whose file changes cannot be snapshotted).
function mare.snapshot.mark_shell() end
---@return {turn: string, shell?: boolean, files: {path: string, before?: string|false, after?: string|false, missing?: boolean}[]}[]|nil
function mare.snapshot.turns() end
---@param hash string a backup's hash
---@return string|nil content nil when trimmed
function mare.snapshot.content(hash) end
--- Restore files touched in `turn` or later to their content before it. Files changed outside
--- mare since are `conflicts` (left alone unless `force`); links now are `refused`; trimmed
--- backups are `missing`.
---@async
---@param turn string
---@param opts? {force?: boolean}
---@return {restored: string[], conflicts: string[], missing: string[], refused: string[], failed: {path: string, error: string}[]}
function mare.snapshot.restore(turn, opts) end
