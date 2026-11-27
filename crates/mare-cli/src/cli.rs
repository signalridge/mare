//! The command line: `Cli` and its subcommands (clap), the long version text and parsing
//! (a usage error in `--json` mode still ends with one `settled` line).

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Arg, ArgAction, CommandFactory, FromArgMatches, Parser, Subcommand};
use mare_shared::flags::{FlagSpec, FlagType, FlagValue};

#[derive(Parser, Debug)]
#[command(
    name = "mare",
    version,
    long_version = LONG_VERSION,
    about = "Agent harness (Rust core, Lua plugins)"
)]
pub(crate) struct Cli {
    /// Initial prompt (interactive) or the prompt to run (with -p)
    pub(crate) prompt: Vec<String>,
    /// Headless: run the prompt, print the final answer, exit
    #[arg(short, long)]
    pub(crate) print: bool,
    /// Output mode: text (default), json (event stream), rpc (stdin/stdout JSON lines)
    #[arg(long, value_parser = ["text", "json", "rpc"])]
    pub(crate) mode: Option<String>,
    /// Shorthand for --mode json
    #[arg(long, conflicts_with = "mode")]
    pub(crate) json: bool,
    /// With --json: also emit the streaming deltas (`item/delta`);
    /// without it the stream is item level (starts, messages, tool calls, usage)
    #[arg(long)]
    pub(crate) stream_deltas: bool,
    /// JSON Schema file that the final assistant output must conform to (headless runs: -p / --json)
    #[arg(long, value_name = "FILE", conflicts_with = "rpc")]
    pub(crate) output_schema: Option<PathBuf>,
    /// Bidirectional JSON-RPC 2.0 over stdin/stdout (same as --mode rpc; `mare rpc schema`)
    #[arg(long, conflicts_with_all = ["print", "json", "mode"])]
    pub(crate) rpc: bool,
    /// Model, e.g. openai/gpt-6.1-terra
    #[arg(short, long, global = true)]
    pub(crate) model: Option<String>,
    /// Use a registered agent type for the initial session
    #[arg(long, global = true, value_name = "NAME")]
    pub(crate) agent: Option<String>,
    /// Reasoning effort (off, minimal, low, medium, high, xhigh, max)
    #[arg(short, long, global = true)]
    pub(crate) effort: Option<String>,
    /// Only enable these tools (comma separated), e.g. --tools read,bash
    #[arg(long, global = true, value_delimiter = ',')]
    pub(crate) tools: Option<Vec<String>>,
    /// Exclude these tools (repeatable or comma separated; supports `*`), e.g. --exclude-tools bash,write
    #[arg(long = "exclude-tools", value_name = "TOOLS", global = true, action = ArgAction::Append, value_delimiter = ',')]
    pub(crate) exclude_tools: Vec<String>,
    /// Add an extra directory to the workspace (repeatable; existing absolute physical directories only)
    #[arg(long = "add-dir", value_name = "DIR", action = ArgAction::Append)]
    pub(crate) add_dir: Vec<PathBuf>,
    /// Append text to the system prompt
    #[arg(long, global = true, value_name = "TEXT")]
    pub(crate) append_system_prompt: Option<String>,
    /// Continue the most recent session in this directory (with --all: in any directory)
    #[arg(short = 'c', long = "continue")]
    pub(crate) continue_: bool,
    /// Resume a session: its id (or an unambiguous prefix), its name, -N (the Nth most recent)
    /// or a file path; without a value opens the picker
    #[arg(short, long, num_args = 0..=1, default_missing_value = "", allow_negative_numbers = true)]
    pub(crate) resume: Option<String>,
    /// With -c / --resume: look at sessions of every directory, not just this one
    #[arg(long)]
    pub(crate) all: bool,
    /// With -c / --resume: continue in a copy (a new session), leaving the original as it was
    #[arg(long)]
    pub(crate) fork_session: bool,
    /// Use this id for the new session (8-64 letters, digits, - or _; e.g. a UUID)
    #[arg(long, value_name = "ID")]
    pub(crate) session_id: Option<String>,
    /// Name the session (also `mare session new --name …`)
    #[arg(long, value_name = "NAME", global = true)]
    pub(crate) name: Option<String>,
    /// Do not persist this session
    #[arg(long)]
    pub(crate) no_session: bool,
    /// Stop after this many model turns in one prompt (headless exit code 4)
    #[arg(long, value_name = "N")]
    pub(crate) max_turns: Option<u32>,
    /// Stop the run after this long, e.g. 90s, 5m, 1h (headless exit code 4)
    #[arg(long, value_name = "DURATION", value_parser = crate::headless::parse_duration, conflicts_with = "rpc")]
    pub(crate) timeout: Option<std::time::Duration>,
    /// Permission preset for this session: read-only | auto | full-access
    #[arg(long, value_parser = ["read-only", "auto", "full-access"])]
    pub(crate) permission: Option<String>,
    /// Independent approval policy; never rejects requests without prompting
    #[arg(long, value_parser = ["ask", "never", "untrusted"])]
    pub(crate) approval_policy: Option<String>,
    /// Shorthand for --permission full-access
    #[arg(long, conflicts_with = "permission")]
    pub(crate) yolo: bool,
    /// Headless / --rpc: also serve RPC on this session's peer socket and register it
    /// (interactive sessions always do unless `peer = false`)
    #[arg(long)]
    pub(crate) peer: bool,
    /// Enable the host HTTP/WebSocket RPC listener
    #[arg(long, conflicts_with_all = ["print", "json"])]
    pub(crate) web: bool,
    /// Bind address (`HOST:PORT`) for the host HTTP/WebSocket RPC listener (`--web`)
    #[arg(long, value_name = "ADDR", conflicts_with_all = ["print", "json"])]
    pub(crate) web_listen: Option<String>,
    /// Override config values: --set plugins.tools.edit_mode=replace
    #[arg(long = "set", value_name = "KEY=VALUE", global = true, value_parser = mare_config::parse_override)]
    pub(crate) set: Vec<(String, serde_json::Value)>,
    /// Refuse to start (exit code 2) when the configuration has warnings (unknown options, type
    /// errors, unparseable MARE_* values) or init.lua errors instead of continuing
    #[arg(long, global = true)]
    pub(crate) strict_config: bool,
    /// Trust this project for this run (load its plugins, skills, prompts, system prompt);
    /// overrides the saved decision and saves nothing
    #[arg(long, global = true, conflicts_with = "no_trust")]
    pub(crate) trust: bool,
    /// Don't trust this project for this run, whatever was saved
    #[arg(long, global = true)]
    pub(crate) no_trust: bool,
    /// Approve the project's and $MARE_MCP_CONFIG's MCP servers (remembered by name and
    /// configuration hash) without asking — for headless runs
    #[arg(long, global = true)]
    pub(crate) approve_mcps: bool,
    /// Like `nvim --clean`: no user/project config, user plugins, or packages
    #[arg(long, global = true)]
    pub(crate) clean: bool,
    /// Write startup timing to a file (like Neovim's --startuptime)
    #[arg(long, value_name = "FILE", global = true)]
    pub(crate) startuptime: Option<PathBuf>,
    /// Start the TUI, quit once it is ready, and print how long each startup stage took
    #[arg(long, global = true)]
    pub(crate) profile_startup: bool,
    /// Working directory
    #[arg(short = 'C', long, global = true)]
    pub(crate) cwd: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Cmd {
    /// Manage plugins
    Plugin {
        #[command(subcommand)]
        action: PluginCmd,
    },
    /// Run as an Agent Client Protocol server over stdio
    Acp,
    /// Trust (or revoke trust for) a directory (default: current project root, git worktree root, or cwd)
    Trust {
        dir: Option<std::path::PathBuf>,
        #[arg(long)]
        revoke: bool,
    },
    /// Sessions: create, list, fork, archive, unarchive, or delete saved sessions
    Session {
        #[command(subcommand)]
        action: SessionCmd,
    },
    /// Print the runtime directory (read-only copies of builtin plugins and types)
    Runtime,
    /// Create a default ~/.config/mare/init.lua and .luarc.json
    Init,
    /// Check config, credentials, connectivity and plugins
    Doctor {
        /// Safe fixes only: create missing directories, tighten permissions (0700 / 0600)
        #[arg(long)]
        fix: bool,
        /// Print the checks as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// Log in to a provider; `mare login status` lists every provider
    Login {
        /// provider name, or `status`
        provider: Option<String>,
        /// Read an API key from stdin: `printenv OPENAI_API_KEY | mare login openai --with-api-key`
        #[arg(long, conflicts_with_all = ["device_auth", "oauth"])]
        with_api_key: bool,
        /// Sign in with a device code (no browser here, e.g. over SSH)
        #[arg(long, conflicts_with_all = ["with_api_key", "oauth"])]
        device_auth: bool,
        /// Sign in using browser OAuth
        #[arg(long, conflicts_with_all = ["with_api_key", "device_auth"])]
        oauth: bool,
    },
    /// Remove a provider's stored credentials (all of them; tokens are revoked first)
    Logout { provider: Option<String> },
    /// Model catalogue: `mare model refresh` re-reads every logged-in provider's model list
    Model {
        #[command(subcommand)]
        action: ModelCmd,
    },
    /// Print a shell completion script: `mare completion zsh > ~/.zfunc/_mare`
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// sudo askpass helper (run by sudo through SUDO_ASKPASS while the TUI is open)
    #[command(hide = true)]
    Askpass {
        #[arg(long, hide = true)]
        sock: Option<String>,
        prompt: Option<String>,
    },
    /// MCP servers: list, list-tools, enable, disable, login
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// Talk to mare sessions over their peer sockets
    Peer {
        #[command(subcommand)]
        action: PeerCmd,
    },
    /// RPC protocol helpers
    Rpc {
        #[command(subcommand)]
        action: RpcCmd,
    },
    /// Windows enhanced sandbox: install, remove or inspect
    Sandbox {
        #[command(subcommand)]
        action: SandboxCmd,
    },
    /// Run Lua *_spec.lua tests using the mare.test harness (cases in the same spec file share an isolated file-level Lua environment)
    Test {
        /// Spec files or directories to run (default: current directory)
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
        /// Only run test cases whose full name contains this substring
        #[arg(short, long, value_name = "PATTERN")]
        filter: Option<String>,
        /// Update snapshot files directly instead of failing or writing .snap.new
        #[arg(short, long)]
        update_snapshots: bool,
    },
    /// Host runtime authentication token management
    Token {
        #[command(subcommand)]
        action: TokenCmd,
    },
    /// Host HTTP/WebSocket listener helpers
    Web {
        #[command(subcommand)]
        action: WebCmd,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum WebCmd {
    /// Print the authenticated login URL for the running host webserver
    Url,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TokenCmd {
    /// Generate a new 32-byte host token (0600) and immediately invalidate the previous token
    Rotate {
        /// Accept missing sandbox protection explicitly (e.g. fs_host_protect)
        #[arg(long)]
        accept_partial: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SandboxCmd {
    /// Install the enhanced tier (elevates once): sandbox accounts, deny-read ACLs, WFP filters
    Setup {
        /// Undo everything setup installed
        #[arg(long)]
        remove: bool,
        /// Extra path that sandboxed commands must not read (repeatable; also `sandbox.deny_read`)
        #[arg(long = "deny-read", value_name = "PATH")]
        deny_read: Vec<std::path::PathBuf>,
        /// Don't apply the built-in deny-read list (credentials, browser profiles)
        #[arg(long)]
        no_default_deny_read: bool,
    },
    /// Show the installed setup and run the self-test
    Status {
        /// Exit non-zero unless all sandbox isolation capabilities are fully enforced
        #[arg(long)]
        require_full: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ModelCmd {
    /// Fetch the model lists (and models.dev metadata) now
    Refresh,
}

#[derive(Subcommand, Debug)]
pub(crate) enum McpCmd {
    /// Connect to every enabled server and print its state, tools and errors (exit 1 on problems)
    List,
    /// Print a server's tools with their input schemas
    ListTools { server: String },
    /// Enable a server in this project (kept in the state directory, not the config file)
    Enable { server: String },
    /// Disable a server in this project
    Disable { server: String },
    /// Log in to an OAuth server in the browser
    Login { server: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum PeerCmd {
    /// Send a message to a session: by default the one this process runs under
    /// ($MARE_PEER_SOCKET, set for tools and hooks); held until its user confirms
    Send {
        /// a session's socket path or registered name (default: $MARE_PEER_SOCKET)
        #[arg(long)]
        to: Option<String>,
        /// the message (read from stdin when omitted)
        text: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionCmd {
    /// Create an empty session in this directory and print its id
    New,
    /// List saved sessions (newest first; current directory by default)
    List {
        /// Include sessions from every workspace
        #[arg(long)]
        all: bool,
        /// List archived sessions instead of active ones
        #[arg(long)]
        archived: bool,
        /// Maximum number of sessions to show
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Print JSON matching RPC `session.list`
        #[arg(long)]
        json: bool,
    },
    /// Fork a saved session (by id, prefix, name, or path) and print the new session id
    Fork {
        /// Session id, unique prefix, name, or path
        session: String,
    },
    /// Archive a saved session (by id, prefix, name, or path)
    Archive {
        /// Session id, unique prefix, name, or path
        session: String,
    },
    /// Restore an archived session (by id, prefix, name, or path)
    Unarchive {
        /// Session id, unique prefix, name, or path
        session: String,
    },
    /// Permanently delete a saved session (by id, prefix, name, or path)
    Delete {
        /// Session id, unique prefix, name, or path
        session: String,
        /// Skip the interactive confirmation prompt (required when stdin is not a TTY)
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum RpcCmd {
    /// Print the JSON Schema of the RPC protocol
    Schema,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum PluginCmd {
    /// List declared and installed plugins
    List {
        /// Print the list as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print the composed runtime plugin tree
    Tree {
        /// Print the tree as JSON
        #[arg(long)]
        json: bool,
    },
    /// Install plugins declared in init.lua (respects mare-lock.json)
    Sync,
    /// Update plugins to their latest revision and rewrite mare-lock.json
    Update {
        name: Option<String>,
        /// Save local changes and untracked files in git stash before updating
        #[arg(long)]
        stash: bool,
    },
    /// Check for updates without moving checkouts or changing the lock
    Check {
        names: Vec<String>,
        /// Print structured results as JSON
        #[arg(long)]
        json: bool,
    },
    /// Restore plugins to the commits recorded in mare-lock.json
    Restore {
        name: Option<String>,
        /// Rebuild a missing or corrupt lock from clean installed checkouts
        #[arg(long)]
        repair: bool,
    },
    /// Remove checkouts that are no longer declared
    Clean {
        /// Allow removal of ignored files and saved git stashes
        #[arg(long)]
        force: bool,
    },
    /// Install one or more catalog plugins
    Install {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Uninstall one or more catalog plugins
    Uninstall {
        #[arg(required = true)]
        names: Vec<String>,
    },
}

/// `mare --version`: version, commit, commit date, target (build.rs).
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("MARE_GIT_COMMIT"),
    "\ndate: ",
    env!("MARE_GIT_DATE"),
    "\ntarget: ",
    env!("MARE_TARGET"),
);

#[path = "cli_prescan.rs"]
mod prescan;
pub(crate) use prescan::pre_scan_args;

pub(crate) struct ParsedCli {
    pub cli: Cli,
    pub flags: serde_json::Map<String, serde_json::Value>,
}

fn format_flag_help(spec: &FlagSpec) -> Option<String> {
    let def_str = spec.default.as_ref().map(|d| match d {
        FlagValue::Boolean(b) => b.to_string(),
        FlagValue::String(s) => format!("{s:?}"),
    });
    match (&spec.description, def_str) {
        (Some(desc), Some(def)) => Some(format!("{desc} (default: {def})")),
        (Some(desc), None) => Some(desc.clone()),
        (None, Some(def)) => Some(format!("(default: {def})")),
        (None, None) => None,
    }
}

/// Build the CLI parser extended with discovered extension flags and parse `args`.
pub(crate) fn parse_cli_from<I, S>(args: I, specs: &[FlagSpec]) -> Result<ParsedCli, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let mut cmd = Cli::command();
    let mut attached: Vec<(String, FlagSpec)> = Vec::new();
    if !specs.is_empty() {
        let builtin_longs: HashSet<String> = cmd
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .chain(["help".to_string(), "version".to_string()])
            .collect();

        for spec in specs {
            if builtin_longs.contains(&spec.name) {
                tracing::warn!("ignoring extension CLI flag --{}: conflicts with built-in flag", spec.name);
                continue;
            }
            let id = format!("ext_flag:{}", spec.name);
            let mut arg = Arg::new(id.clone()).long(spec.name.clone()).help_heading("Plugin CLI Flags");
            if let Some(help) = format_flag_help(spec) {
                arg = arg.help(help);
            }
            arg = match spec.flag_type {
                FlagType::Boolean => arg.action(ArgAction::SetTrue),
                FlagType::String => arg.value_name("VALUE").num_args(1),
            };
            cmd = cmd.arg(arg);
            attached.push((id, spec.clone()));
        }
    }

    let matches = cmd.try_get_matches_from(args)?;
    let cli = Cli::from_arg_matches(&matches)?;
    let mut flags = serde_json::Map::new();
    for (id, spec) in attached {
        if matches.value_source(&id) == Some(clap::parser::ValueSource::CommandLine) {
            match spec.flag_type {
                FlagType::Boolean => {
                    if matches.get_flag(&id) {
                        flags.insert(spec.name, serde_json::Value::Bool(true));
                    }
                }
                FlagType::String => {
                    if let Some(v) = matches.get_one::<String>(&id) {
                        flags.insert(spec.name, serde_json::Value::String(v.clone()));
                    }
                }
            }
        }
    }
    Ok(ParsedCli { cli, flags })
}

fn needs_extension_flag_discovery(err: &clap::Error) -> bool {
    use clap::error::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            | ErrorKind::UnknownArgument
            | ErrorKind::InvalidSubcommand
    )
}

/// Parse `args` using only built-in flags first; invoke `discover` only when `--help`/`-h`/`help`
/// or an unknown flag/subcommand needs extension flag definitions.
pub(crate) fn parse_cli_with_discovery_from<I, S, F>(args: I, mut discover: F) -> Result<ParsedCli, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
    F: FnMut(&std::path::Path, bool, Option<bool>) -> Vec<FlagSpec>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    match parse_cli_from(args.iter().cloned(), &[]) {
        Ok(parsed) => Ok(parsed),
        Err(err) if needs_extension_flag_discovery(&err) => {
            let pre = pre_scan_args(args.iter().cloned());
            if pre.skip_discovery || pre.no_plugins {
                return Err(err);
            }
            let pre_cwd = pre
                .cwd
                .as_deref()
                .and_then(|p| dunce::canonicalize(p).ok())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let flag_specs = discover(&pre_cwd, pre.no_plugins, pre.cli_trust);
            if flag_specs.is_empty() {
                return Err(err);
            }
            parse_cli_from(args, &flag_specs)
        }
        Err(err) => Err(err),
    }
}

/// Parse the command line, running `discover` only if built-in parsing requires extension flags.
/// A usage error in `--json` mode still ends with one `settled` line (exit code 2), so a script
/// reading the event stream always gets the outcome.
pub(crate) fn parse_cli_with_discovery<F>(discover: F) -> ParsedCli
where
    F: FnMut(&std::path::Path, bool, Option<bool>) -> Vec<FlagSpec>,
{
    let args: Vec<OsString> = std::env::args_os().collect();
    match parse_cli_with_discovery_from(args.iter().cloned(), discover) {
        Ok(p) => p,
        Err(e) => {
            use clap::error::ErrorKind;
            let json = pre_scan_args(args.iter().cloned()).json;
            if json && !matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let first = e.to_string();
                let msg = first.lines().next().unwrap_or("invalid arguments").trim_start_matches("error: ");
                mare_binding::platform::exit(crate::headless::usage_error(true, msg));
            }
            e.exit()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_tree_rejects_the_removed_dump_flag() {
        assert!(Cli::try_parse_from(["mare", "plugin", "tree", "--dump"]).is_err());
        assert!(Cli::try_parse_from(["mare", "plugin", "tree", "--json"]).is_ok());
    }

    #[test]
    fn plugin_commands_and_trust_flags_use_the_unified_names() {
        for args in [
            vec!["mare", "plugin", "list", "--json"],
            vec!["mare", "plugin", "tree", "--json"],
            vec!["mare", "plugin", "sync"],
            vec!["mare", "plugin", "update", "demo"],
            vec!["mare", "plugin", "restore", "demo"],
            vec!["mare", "plugin", "clean"],
            vec!["mare", "plugin", "install", "one", "two"],
            vec!["mare", "plugin", "uninstall", "one", "two"],
            vec!["mare", "model", "refresh"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(cli.command.is_some(), "{args:?} was parsed as a prompt");
        }
        for flag in [concat!("--no-", "plugins"), concat!("--", "approve"), concat!("--no-", "approve")] {
            assert!(Cli::try_parse_from(["mare", flag]).is_err(), "accepted {flag}");
        }
        assert!(Cli::try_parse_from(["mare", concat!("--", "thinking"), "high"]).is_err());
        assert!(Cli::try_parse_from(["mare", "--trust"]).is_ok());
        assert!(Cli::try_parse_from(["mare", "--no-trust"]).is_ok());
    }

    #[test]
    fn parses_extension_boolean_and_string_flags_and_prints_help() {
        let specs = vec![
            FlagSpec {
                name: "ext-bool".into(),
                flag_type: FlagType::Boolean,
                description: Some("Enable custom mode".into()),
                default: Some(FlagValue::Boolean(false)),
                source: Some("plugin:demo".into()),
            },
            FlagSpec {
                name: "ext-str".into(),
                flag_type: FlagType::String,
                description: Some("Custom tag".into()),
                default: Some(FlagValue::String("dev".into())),
                source: Some("init.lua".into()),
            },
            // Built-in collision must be ignored safely
            FlagSpec {
                name: "model".into(),
                flag_type: FlagType::Boolean,
                description: Some("Should not override --model".into()),
                default: None,
                source: Some("plugin:bad".into()),
            },
        ];

        let parsed = parse_cli_from(["mare", "--ext-bool", "--ext-str", "prod", "--model", "m1", "hello"], &specs)
            .expect("valid extension flags");
        assert_eq!(parsed.cli.model.as_deref(), Some("m1"));
        assert_eq!(parsed.cli.prompt, vec!["hello"]);
        assert_eq!(parsed.flags.get("ext-bool"), Some(&serde_json::json!(true)));
        assert_eq!(parsed.flags.get("ext-str"), Some(&serde_json::json!("prod")));
        assert!(!parsed.flags.contains_key("model"));

        // Omitting flags does not fabricate CommandLine values
        let omitted = parse_cli_from(["mare", "hello"], &specs).unwrap();
        assert!(omitted.flags.is_empty());

        // Unknown flags and missing string values still fail
        assert!(parse_cli_from(["mare", "--unknown-flag"], &specs).is_err());
        assert!(parse_cli_from(["mare", "--ext-str"], &specs).is_err());

        // --help includes the plugin flags section
        let help = parse_cli_from(["mare", "--help"], &specs).err().unwrap().to_string();
        assert!(help.contains("Plugin CLI Flags"), "{help}");
        assert!(help.contains("--ext-bool"), "{help}");
        assert!(help.contains("--ext-str <VALUE>"), "{help}");
        assert!(help.contains("Enable custom mode"), "{help}");
    }

    /// The pre-scan decides which project's `init.lua` runs (and whether `--trust` applies)
    /// before clap parses, so it must read the working directory and trust flags exactly as
    /// clap will: attached short values (`-C/b`), short clusters (`-pC/b`), and values of other
    /// options that look like these flags.
    #[test]
    fn pre_scan_agrees_with_clap_on_the_working_directory_and_trust() {
        let cases: &[&[&str]] = &[
            &["mare", "-C/safe/b", "--trust", "-p", "hi"],
            &["mare", "-C=/safe/b", "--trust"],
            &["mare", "-pC/safe/b", "--trust", "hi"],
            &["mare", "-C", "/safe/b", "--no-trust"],
            &["mare", "--cwd=/safe/b"],
            &["mare", "--cwd", "/safe/b", "--trust"],
            &["mare", "-r", "-2", "-C", "/safe/b"],
            &["mare", "--append-system-prompt=--trust", "-C/safe/b"],
            &["mare", "-m", "x/-C/elsewhere", "-C/safe/b"],
            &["mare", "--", "-C/elsewhere", "--trust"],
            &["mare", "-p", "--", "--trust"],
        ];
        for args in cases {
            let cli = Cli::try_parse_from(*args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let pre = pre_scan_args(args.iter().copied());
            assert_eq!(pre.cwd, cli.cwd, "cwd for {args:?}");
            let trust = if cli.trust {
                Some(true)
            } else if cli.no_trust {
                Some(false)
            } else {
                None
            };
            assert_eq!(pre.cli_trust, trust, "trust for {args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pre_scan_preserves_non_utf8_cwd_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::path::PathBuf;

        let bad_bytes = b"/tmp/bad-\xff";
        let expected = PathBuf::from(OsString::from_vec(bad_bytes.to_vec()));

        let cases: Vec<Vec<OsString>> = vec![
            vec![OsString::from("mare"), OsString::from_vec(b"-C/tmp/bad-\xff".to_vec()), OsString::from("--trust")],
            vec![OsString::from("mare"), OsString::from_vec(b"-C=/tmp/bad-\xff".to_vec())],
            vec![OsString::from("mare"), OsString::from_vec(b"-pC/tmp/bad-\xff".to_vec())],
            vec![OsString::from("mare"), OsString::from("-C"), OsString::from_vec(bad_bytes.to_vec())],
            vec![OsString::from("mare"), OsString::from_vec(b"--cwd=/tmp/bad-\xff".to_vec())],
            vec![OsString::from("mare"), OsString::from("--cwd"), OsString::from_vec(bad_bytes.to_vec())],
            vec![
                OsString::from("mare"),
                OsString::from("--append-system-prompt=--trust"),
                OsString::from_vec(b"-C/tmp/bad-\xff".to_vec()),
            ],
        ];

        for args in &cases {
            let cli = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let pre = pre_scan_args(args.iter().cloned());
            assert_eq!(pre.cwd.as_ref(), Some(&expected), "cwd for {args:?}");
            assert_eq!(pre.cwd, cli.cwd, "cwd agrees with clap for {args:?}");
            assert_eq!(pre.cwd.as_ref().unwrap().as_os_str().as_bytes(), bad_bytes);
        }

        // Non-UTF-8 value on another flag must not prevent pre_scan_args from finding -C.
        let pre = pre_scan_args([
            OsString::from("mare"),
            OsString::from_vec(b"--append-system-prompt=\xff".to_vec()),
            OsString::from_vec(b"-C/tmp/bad-\xff".to_vec()),
        ]);
        assert_eq!(pre.cwd.as_ref(), Some(&expected));
        assert_eq!(pre.cli_trust, None);
    }

    #[test]
    fn fast_path_skips_extension_discovery_for_builtin_args_and_triggers_when_needed() {
        let ext_specs = vec![
            FlagSpec {
                name: "ext-bool".into(),
                flag_type: FlagType::Boolean,
                description: Some("Custom flag".into()),
                default: None,
                source: Some("init.lua".into()),
            },
            FlagSpec {
                name: "ext-str".into(),
                flag_type: FlagType::String,
                description: None,
                default: None,
                source: Some("init.lua".into()),
            },
        ];

        for args in [
            vec!["mare", "-p", "hi"],
            vec!["mare", "--model", "m1", "hello"],
            vec!["mare", "-C", ".", "--trust", "-p", "hi"],
            vec!["mare", "--cwd=--help"],
            vec!["mare", "--set", "foo=--trust", "-p", "hi"],
            vec!["mare", "--", "--help", "-C/other", "--trust"],
        ] {
            let mut calls = 0usize;
            let parsed = parse_cli_with_discovery_from(args.iter().copied(), |_, _, _| {
                calls += 1;
                ext_specs.clone()
            })
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(calls, 0, "unexpected discovery for {args:?}");
            assert!(parsed.flags.is_empty(), "{args:?}");
        }

        // `--cwd --help`, `--version` / `-V`, and `--clean --help` / `--clean --ext-bool` must also skip discovery.
        for args in [
            vec!["mare", "--cwd", "--help"],
            vec!["mare", "--version"],
            vec!["mare", "-V"],
            vec!["mare", "--clean", "--help"],
            vec!["mare", "--clean", "--ext-bool"],
        ] {
            let mut calls = 0usize;
            let err = parse_cli_with_discovery_from(args.iter().copied(), |_, _, _| {
                calls += 1;
                ext_specs.clone()
            })
            .err()
            .unwrap_or_else(|| panic!("expected clap exit/error for {args:?}"));
            assert_eq!(calls, 0, "unexpected discovery for {args:?} (err={:?})", err.kind());
        }

        // `--help`, `-h`, `help`, and extension flags trigger discovery with accurate pre-scan context.
        for args in [vec!["mare", "--help"], vec!["mare", "-h"], vec!["mare", "help"]] {
            let mut calls = 0usize;
            let err = parse_cli_with_discovery_from(args.iter().copied(), |_, no_plugins, cli_trust| {
                calls += 1;
                assert!(!no_plugins);
                assert_eq!(cli_trust, None);
                ext_specs.clone()
            })
            .err()
            .unwrap();
            assert_eq!(calls, 1, "expected discovery for {args:?}");
            assert!(err.to_string().contains("--ext-bool"), "{args:?}: {err}");
        }

        let mut calls = 0usize;
        let parsed = parse_cli_with_discovery_from(
            ["mare", "--ext-bool", "--ext-str", "prod", "--no-trust", "-p", "hi"],
            |_, no_plugins, cli_trust| {
                calls += 1;
                assert!(!no_plugins);
                assert_eq!(cli_trust, Some(false));
                ext_specs.clone()
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(parsed.flags.get("ext-bool"), Some(&serde_json::json!(true)));
        assert_eq!(parsed.flags.get("ext-str"), Some(&serde_json::json!("prod")));
    }
}
