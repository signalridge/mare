//! `mare --version` (long form): commit, commit date and target triple, like `rustc -vV`.
//! Outside a git checkout (crates.io, tarballs) the commit is "unknown".

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(env!("CARGO_MANIFEST_DIR")).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Shared CARGO_TARGET_DIRs are used by linked worktrees. Their manifest directories
    // differ even when package name/version and the common git ref paths overlap, so Cargo
    // must rerun this script when the build switches to a different checkout.
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    // Linked worktrees keep HEAD locally, but branch refs and packed-refs in the common git dir.
    // Ask git for each path so Cargo watches the files that actually supply the version.
    let mut paths = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(reference);
    }
    for path in paths {
        if let Some(path) = git(&["rev-parse", "--git-path", &path]) {
            // Repositories with only loose refs need no packed-refs watch; a missing input
            // makes Cargo rerun the script on every build. Packing removes the watched loose ref.
            if path.ends_with("packed-refs") && !std::path::Path::new(&path).exists() {
                continue;
            }
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date = git(&["log", "-1", "--format=%cs"]).unwrap_or_else(|| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=MARE_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=MARE_GIT_DATE={date}");
    println!("cargo:rustc-env=MARE_TARGET={target}");
}
