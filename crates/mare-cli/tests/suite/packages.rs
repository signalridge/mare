//! Packages (lazy.nvim style) and extras (LazyVim), `/rewind` and `/undo`,
//! `mare.g`, and lua-console (local-only commands).

use crate::common::*;
use serde_json::json;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

mod blocking;
mod console;
mod extras;
mod install;
mod local_plugin_tool_skill;
mod rewind;
mod rpc;
mod startup;

fn git(dir: &Path, args: &[&str]) {
    let ok = fixture_git(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
}

/// A package repo with one plugin `<plugin>` (sets `mare.g[plugin] = true` when applied),
/// optional manifest, tagged `tag`.
pub(crate) fn package_repo(plugin: &str, manifest: Option<&str>, tag: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("plugins").join(plugin);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(
        p.join("init.lua"),
        format!("return {{ name = {plugin:?}, apply = function() mare.g[{plugin:?}] = true end }}\n"),
    )
    .unwrap();
    if let Some(m) = manifest {
        std::fs::write(d.path().join("mare-plugin.toml"), m).unwrap();
    }
    git(d.path(), &["init", "-q", "-b", "main"]);
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "init"]);
    git(d.path(), &["tag", tag]);
    // a newer untagged commit: `version = "^1"` must stay on the tag
    std::fs::write(d.path().join("NEWER"), "x").unwrap();
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "newer"]);
    d
}

/// A fake extras catalog: a git repository in a temporary directory,
/// reached through a `file://` address — git's `url.<file://…>.insteadOf` stands in for
/// signalridge/mare-plugins — and fetched the way `/plugins` does. Keep the returned directory alive.
pub(crate) fn catalog(env: &mut TestEnv) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    for (name, toml) in [
        (
            "rust",
            "category = \"lang\"\ndescription = \"Rust\"\nconflicts = [\"alt\"]\n[deps]\nrequired = [\"someone/rust-tools\"]\n[recommended]\nroot = [\"Cargo.toml\"]\n",
        ),
        ("alt", "category = \"lang\"\ndescription = \"another Rust setup\"\n[recommended]\nroot = [\"alt.marker\"]\n"),
        ("docs", "category = \"tools\"\ndescription = \"docs\"\nversion = \"1.1.0\"\n"),
    ] {
        let d = root.join("plugins").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mare-plugin.toml"), format!("name = {name:?}\n{toml}")).unwrap();
        std::fs::write(d.join("init.lua"), format!("return {{ name = {name:?} }}\n")).unwrap();
    }
    std::fs::write(root.join("plugins/docs/NEWS.md"), "## 1.1.0\n- docs got better\n").unwrap();
    std::fs::write(
        root.join("plugins.toml"),
        "[renamed]\n\"rusty\" = \"rust\"\n[deprecated]\n\"old\" = \"merged into docs\"\n",
    )
    .unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "catalog"]);
    let url = format!("file://{}", root.display());
    env.set_var("GIT_CONFIG_COUNT", "1");
    env.set_var("GIT_CONFIG_KEY_0", format!("url.{url}.insteadOf"));
    env.set_var("GIT_CONFIG_VALUE_0", "https://github.com/signalridge/mare-plugins.git");
    let mut log = vec![];
    let r = mare_plugin_manager::fetch_catalog(&mut |l| log.push(l));
    assert!(r.is_ok(), "{r:?} {log:?}");
    let checkout = env.home.path().join("data/packages/.catalogs/github.com/signalridge/mare-plugins");
    assert!(checkout.join(".git").exists() && checkout.join("plugins/rust/mare-plugin.toml").exists(), "{log:?}");
    repo
}

/// Point `https://github.com/someone/rust-tools.git` at a local path via git `insteadOf`,
/// extending the `GIT_CONFIG_*` entry set by [`catalog`].
pub(super) fn package_mirror(env: &mut TestEnv, target: &str) {
    env.set_var("GIT_CONFIG_COUNT", "2");
    env.set_var("GIT_CONFIG_KEY_1", format!("url.file://{target}.insteadOf"));
    env.set_var("GIT_CONFIG_VALUE_1", "https://github.com/someone/rust-tools.git");
}

/// The `request/ready` deny reason (startup or reload incomplete), or None when requests may run.
async fn blocked_reason(h: &mare_host::Host) -> Option<String> {
    let verdict = h.kernel.policy("request/ready", mlua::MultiValue::new()).await.unwrap();
    let (decision, reason): (String, Option<String>) =
        (verdict.get("decision").unwrap(), verdict.get("reason").unwrap());
    (decision == "deny").then(|| reason.unwrap_or_default())
}

/// Checkout path for `url` under an isolated `MARE_HOME` directory without mutating process env.
pub(super) fn checkout_dir_for(home: &Path, url: &str) -> std::path::PathBuf {
    let rel = mare_plugin_manager::checkout_dir(url)
        .strip_prefix(mare_shared::paths::packages_dir())
        .expect("checkout_dir lives under packages_dir")
        .to_path_buf();
    home.join("data/packages").join(rel)
}
