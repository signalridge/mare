//! System prompt assembly: the assembled prompt for each configuration is
//! compared byte for byte with a golden file (`tests/fixtures/prompt/<case>.txt`; Windows' own
//! where its prompt differs, see `golden_path`) captured from the
//! pre-registry code, so moving sections between plugins can't change what the model sees.
//! `MARE_BLESS_PROMPTS=1` rewrites the golden files instead of comparing.
//!
//! Only `default.txt` is the whole prompt. Every other case stores just what differs from it
//! (a line diff, see `golden_diff`), so changing default wording blesses one file, while each
//! section variant is still guarded line for line.

use crate::common::*;
use serde_json::{Value, json};

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Machine-specific parts replaced by placeholders: the workspace, MARE_HOME, this crate's
/// directory, the OS and shell lines, and the sandbox's writable roots (temp dirs differ).
fn normalize(env: &TestEnv, text: &str) -> String {
    let home = env.home.path().to_string_lossy().to_string();
    let home_canon = dunce::canonicalize(env.home.path()).unwrap().to_string_lossy().to_string();
    let mut out = text
        .replace(&env.cwd.to_string_lossy().to_string(), "<CWD>")
        .replace(&home_canon, "<HOME>")
        .replace(&home, "<HOME>")
        .replace(env!("CARGO_MANIFEST_DIR"), "<CRATE>");
    out = out
        .split('\n')
        .map(|l| {
            if l.starts_with("OS: ") {
                "OS: <OS>".to_string()
            } else if l.starts_with("Shell for commands: ") {
                "Shell for commands: <SHELL>".to_string()
            } else if l.starts_with("- writing inside: ") {
                "- writing inside: <ROOTS>".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out
}

/// A case's golden file. Windows' prompt lists the `powershell` tool and its guidelines beside
/// `bash`, says how Git Bash takes paths, and inside the sandbox points at `powershell` (product
/// behaviour, ): there a case reads `prompt/windows/<case>.txt` when the line diff
/// against Windows' own `default.txt` differs from the shared one (and blessing writes there).
fn golden_path(case: &str) -> String {
    let dir = format!("{}/tests/fixtures/prompt", env!("CARGO_MANIFEST_DIR"));
    let windows = format!("{dir}/windows/{case}.txt");
    let own = std::env::var_os("MARE_BLESS_PROMPTS").is_some() || std::path::Path::new(&windows).exists();
    if cfg!(windows) && own { windows } else { format!("{dir}/{case}.txt") }
}

fn golden(case: &str, actual: &str) {
    let path = golden_path(case);
    if std::env::var_os("MARE_BLESS_PROMPTS").is_some() {
        write(std::path::Path::new(&path), actual);
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(want == actual, "{case}: the assembled system prompt changed\n--- want\n{want}\n--- got\n{actual}");
}

/// A case's golden: the line diff against the default prompt (`default.txt`), as hunks of
/// `-removed` / `+added` lines, each headed by the unchanged line it follows. Together with the
/// default golden this fixes every byte of the case's prompt.
fn golden_diff(case: &str, actual: &str) {
    let base_path = golden_path("default");
    let base = std::fs::read_to_string(&base_path).unwrap_or_else(|e| panic!("{base_path}: {e}"));
    golden(case, &line_diff(&base, actual));
}

fn line_diff(base: &str, actual: &str) -> String {
    use std::fmt::Write;
    let (a, b): (Vec<&str>, Vec<&str>) = (base.split('\n').collect(), actual.split('\n').collect());
    // longest common subsequence of lines
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = String::new();
    let mut anchor = "<start>";
    let mut in_hunk = false;
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            anchor = a[i];
            in_hunk = false;
            i += 1;
            j += 1;
            continue;
        }
        if !in_hunk {
            let _ = writeln!(out, "@@ after: {anchor}");
            in_hunk = true;
        }
        if j == b.len() || (i < a.len() && lcs[i + 1][j] >= lcs[i][j + 1]) {
            let _ = writeln!(out, "-{}", a[i]);
            i += 1;
        } else {
            let _ = writeln!(out, "+{}", b[j]);
            j += 1;
        }
    }
    out
}

/// `HOME` pointed at the test's own home for one assembly, restored afterwards: the default
/// `skill_dirs` (`~/.config/mare/skills`, `~/.agents/skills`) and other `~` paths must not pick
/// up this machine's files. Callers hold the environment lock through their `TestEnv`.
struct HomeGuard(Option<std::ffi::OsString>);

impl HomeGuard {
    fn new(env: &TestEnv) -> Self {
        let old = std::env::var_os("HOME");
        // SAFETY: the caller's TestEnv holds the global environment lock
        unsafe { std::env::set_var("HOME", env.home.path()) };
        Self(old)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: as in `new`
        unsafe {
            match self.0.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

/// The prompt as `context:system_prompt(model)` assembles it (`model_lua`: a Lua expression).
/// The workspace is a project root of its own (`.git`), so no instruction file above the temp
/// dir is picked up.
async fn assembled(env: &TestEnv, trust: &str, cfg: Value, model_lua: &str) -> String {
    let _home = HomeGuard::new(env);
    std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
    std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
    write(&env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n");
    let mut b =
        env.host().all_builtins().faux(json!([])).config(cfg).args(json!({ "no_session": true, "trust": trust }));
    b.opts.project_trusted = trust == "trusted";
    let h = b.build().await;
    global(&h, "X", h.service_table("context").unwrap());
    let text: String = eval(&h, &format!("return (X:system_prompt({model_lua}))")).await;
    h.shutdown().await;
    normalize(env, &text)
}

/// User and project prompt files, AGENTS.md (user and project) and a project skill.
fn project_files(env: &TestEnv) {
    write(&env.config_dir().join("SYSTEM.md"), "USER SYSTEM PROMPT\nsecond line\n");
    write(&env.config_dir().join("APPEND_SYSTEM.md"), "USER APPEND\n");
    write(&env.config_dir().join("AGENTS.md"), "USER AGENTS DOC");
    write(&env.cwd.join(".mare/SYSTEM.md"), "PROJECT SYSTEM PROMPT\n");
    write(&env.cwd.join(".mare/APPEND_SYSTEM.md"), "PROJECT APPEND");
    write(&env.cwd.join("AGENTS.md"), "PROJECT AGENTS DOC\n");
    write(
        &env.cwd.join(".mare/skills/proj-skill/SKILL.md"),
        "---\nname: proj-skill\ndescription: a project skill\n---\nbody",
    );
}

#[tokio::test]
async fn default_config() {
    local(async {
        let env = TestEnv::new().await;
        golden("default", &assembled(&env, "none", json!({}), "nil").await);
    })
    .await
}

#[tokio::test]
async fn trusted_project_files_take_precedence() {
    local(async {
        let env = TestEnv::new().await;
        project_files(&env);
        golden_diff("trusted_project", &assembled(&env, "trusted", json!({}), "nil").await);
    })
    .await
}

#[tokio::test]
async fn untrusted_project_files_are_ignored() {
    local(async {
        let env = TestEnv::new().await;
        project_files(&env);
        golden_diff("untrusted_project", &assembled(&env, "untrusted", json!({}), "nil").await);
    })
    .await
}

/// Files are passed through verbatim: a literal `{{x}}` (or Go-template syntax) is not a variable.
#[tokio::test]
async fn literal_braces_in_files_pass_through() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.config_dir().join("SYSTEM.md"), "USER {{x}} and {{ .Name }} and {{model}}\n");
        write(&env.config_dir().join("AGENTS.md"), "USER AGENTS {{x}}");
        write(&env.cwd.join(".mare/APPEND_SYSTEM.md"), "PROJECT APPEND {{x}} {{");
        write(&env.cwd.join("AGENTS.md"), "PROJECT AGENTS {{ .Name }}");
        golden_diff("literal_braces", &assembled(&env, "trusted", json!({}), "nil").await);
    })
    .await
}

/// The `system` / `append` / `rules` options are passed through verbatim too.
#[tokio::test]
async fn config_system_append_and_rules_pass_through() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.config_dir().join("SYSTEM.md"), "NOT USED: cfg.system wins\n");
        write(&env.config_dir().join("APPEND_SYSTEM.md"), "FILE APPEND");
        let cfg = json!({ "plugins": { "context": {
            "system": "CFG SYSTEM {{model}}", "append": "CFG APPEND {{x}}", "rules": ["extra rule {{x}}"]
        } } });
        golden_diff("config_options", &assembled(&env, "none", cfg, "nil").await);
    })
    .await
}

/// `mare.prompt.section` / `mare.prompt.remove` (user layer): a new section that sorts beside a
/// built-in one, an override of the persona section, and a removal.
#[tokio::test]
async fn user_sections_add_override_and_remove() {
    local(async {
        let env = TestEnv::new().await;
        write(&env.config_dir().join("APPEND_SYSTEM.md"), "USER APPEND");
        let cfg = json!({
            "prompt_sections": {
                "custom": { "name": "custom", "order": 800, "text": "USER SECTION {{x}}" },
                "early": { "name": "early", "order": 0, "text": "EARLY" },
                "persona": { "name": "persona", "text": "MY PERSONA" }
            },
            "prompt_removed": { "cwd": true }
        });
        golden_diff("user_sections", &assembled(&env, "none", cfg, "nil").await);
    })
    .await
}

#[tokio::test]
async fn gpt_models_get_apply_patch_and_gpt_rules() {
    local(async {
        let env = TestEnv::new().await;
        let model = r#"{ id = "gpt-5.5", provider = "openai", ref = "openai/gpt-5.5", name = "GPT-5.5" }"#;
        golden_diff("gpt", &assembled(&env, "none", json!({}), model).await);
    })
    .await
}

/// One line per MCP server; a deferred server is listed without connecting, and its description
/// (third-party text) passes through verbatim.
#[tokio::test]
async fn mcp_servers_are_listed() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "mcp": { "files": false, "servers": {
            "docs": { "url": "https://example.invalid/mcp", "description": "Docs {{x}} server" }
        } } } });
        golden_diff("mcp", &assembled(&env, "none", cfg, "nil").await);
    })
    .await
}

#[tokio::test]
async fn skills_are_listed() {
    local(async {
        let env = TestEnv::new().await;
        let dir = format!("{}/tests/fixtures/skills", env!("CARGO_MANIFEST_DIR"));
        let cfg = json!({ "plugins": { "context": { "skill_dirs": [dir] } } });
        golden_diff("skills", &assembled(&env, "none", cfg, "nil").await);
    })
    .await
}

/// The sandbox section, per preset. The project is trusted: an untrusted one always starts in
/// `untrusted_preset` (read-only), whatever `permission` says. The text only depends on the
/// policy (level, normalized roots, network), not on the backend, so it is the same on macOS and
/// Linux.
#[tokio::test]
async fn sandbox_section_per_preset() {
    local(async {
        for preset in ["auto", "read-only", "full-access"] {
            let env = TestEnv::new().await;
            let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": preset } } });
            let text = assembled(&env, "trusted", cfg, "nil").await;
            if preset == "full-access" {
                // no sandbox section: byte for byte the default prompt
                golden("default", &text);
            } else {
                golden_diff(&format!("sandbox_{preset}"), &text);
            }
        }
    })
    .await
}

/// A full replacement of the system prompt (SYSTEM.md) drops the sandbox and untrusted-content
/// sections, so a headless run says so once on stderr (the doctor line); no SYSTEM.md, no notice.
#[test]
fn a_replaced_system_prompt_is_announced_once_on_stderr() {
    let home = tempfile::tempdir().unwrap();
    let (config, work) = (home.path().join("config"), home.path().join("work"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "done" }]).to_string()).unwrap();
    let run = || {
        let out = mare_cmd()
            .args(["-p", "go", "--model", "faux/echo", "--no-session"])
            .current_dir(&work)
            .env("MARE_HOME", home.path())
            .env("HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .stdin(std::process::Stdio::null())
            .run_output();
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    assert!(!run().contains("system prompt replaced"), "no SYSTEM.md, no notice");
    write(&config.join("SYSTEM.md"), "MY PROMPT\n");
    let err = run();
    assert_eq!(err.matches("system prompt replaced by").count(), 1, "{err}");
    assert!(err.contains("SYSTEM.md — no default section is sent"), "{err}");
}
