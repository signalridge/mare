use std::path::Path;

/// `git` for a test fixture, run in `dir`, with every `GIT_*` variable this process inherited
/// removed (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, …): a hook or a worktree
/// script that starts the tests exports them, and a fixture command would then resolve to (and
/// write into) the repository the tests were started from instead of `dir` — a fixture `git init`
/// once set `core.bare = true` on the developer's checkout that way.
pub fn fixture_git(dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(dir);
    cmd
}
