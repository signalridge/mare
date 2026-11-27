//! Standard test entries surface security-test skips rather than hiding them.
//!
//! A sandbox or HTTP security test that lacks a capability prints `SKIPPED[capability=…]` and
//! passes; nextest hides a passing test's output unless `.config/nextest.toml` overrides
//! `success-output` for it. This drives the real entry — `cargo nextest run` with each profile
//! the acceptance run and CI use — with the capability taken away, and requires the marker in
//! nextest's own report. The same run with the overrides stripped from the config must *not*
//! show it, so removing the overrides makes this test fail.
//!
//! It only means something under nextest. `scripts/accept.sh` requires nextest; `just test` falls
//! back to plain `cargo test -- --show-output` where nextest is not installed, so the markers stay
//! visible there too, this test's own
//! `SKIPPED[capability=nextest]` included. Under nextest (`NEXTEST=1`, set by nextest for every
//! test process) the inner runs are required.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Profiles that run the workspace tests: default (plain `cargo nextest run`), gate (`scripts/accept.sh`), and ci (`ci.yml`).
const PROFILES: &[&str] = &["default", "gate", "ci"];

/// `(test filter, env that takes the capability away, marker the skip prints)`.
const SKIPS: &[(&str, &str, &str)] = &[
    (
        "test(=ossandbox::host_protection_tests::bwrap_refuses_relative_symlink_dotdot_mare_home_before_token)",
        "MARE_SANDBOX_BACKEND=none",
        "SKIPPED[capability=bwrap]: bwrap_refuses_relative_symlink_dotdot_mare_home_before_token",
    ),
    (
        "test(=http_serve::sec_tests::per_listener_cookie_names_do_not_collide_on_same_port)",
        "MARE_TEST_NO_SECOND_LOOPBACK=1",
        "SKIPPED[capability=second_loopback]: per_listener_cookie_names_do_not_collide_on_same_port",
    ),
];

fn workspace() -> PathBuf {
    dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// Host triple for synthetic nextest `binaries-metadata.json`.
fn host_triple() -> String {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    if let Ok(out) = Command::new(rustc).arg("-vV").output()
        && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(triple) = line.strip_prefix("host: ") {
                let triple = triple.trim();
                if !triple.is_empty() {
                    return triple.to_string();
                }
            }
        }
    }
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        _ => "unknown-unknown-unknown",
    }
    .to_string()
}

/// Write `cargo-metadata.json` and `binaries-metadata.json` pointing directly at the currently
/// running `mare-binding` unit-test binary so inner `cargo nextest run` invocations never rebuild
/// crates (especially on sharded CI runners running from `--archive-file` without `target/`).
fn write_reused_binary_metadata(dir: &Path) -> (PathBuf, PathBuf) {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .current_dir(workspace())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .expect("cargo metadata must succeed");
    assert!(out.status.success(), "cargo metadata failed:\n{}", String::from_utf8_lossy(&out.stderr));
    let cargo_meta_path = dir.join("cargo-metadata.json");
    std::fs::write(&cargo_meta_path, &out.stdout).unwrap();

    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let target_dir = meta["target_directory"].as_str().expect("target_directory in cargo metadata");
    let pkg_id = meta["packages"]
        .as_array()
        .expect("packages in cargo metadata")
        .iter()
        .find(|p| p["name"] == "mare-binding")
        .and_then(|p| p["id"].as_str())
        .expect("mare-binding package id in cargo metadata");
    let current_exe = dunce::canonicalize(std::env::current_exe().expect("current test binary")).unwrap();
    let triple = host_triple();

    let binaries_meta = serde_json::json!({
        "rust-build-meta": {
            "target-directory": target_dir,
            "build-directory": target_dir,
            "base-output-directories": ["debug"],
            "non-test-binaries": {},
            "build-script-out-dirs": {},
            "build-script-info": {},
            "linked-paths": [],
            "platforms": {
                "host": {
                    "platform": {
                        "triple": triple,
                        "target-features": "unknown"
                    },
                    "libdir": {
                        "status": "unavailable",
                        "reason": "other"
                    }
                },
                "targets": []
            },
            "target-platforms": [{
                "triple": triple,
                "target-features": "unknown"
            }],
            "target-platform": null
        },
        "rust-binaries": {
            "mare-binding": {
                "binary-id": "mare-binding",
                "binary-name": "mare_binding",
                "package-id": pkg_id,
                "kind": "lib",
                "binary-path": current_exe,
                "build-platform": "target"
            }
        }
    });
    let binaries_meta_path = dir.join("binaries-metadata.json");
    std::fs::write(&binaries_meta_path, serde_json::to_vec(&binaries_meta).unwrap()).unwrap();
    (cargo_meta_path, binaries_meta_path)
}

/// `cargo nextest run` of one test under `profile`, reusing the already-built test binary and
/// returning nextest's whole report.
fn nextest(
    profile: &str,
    filter: &str,
    env: &str,
    config: Option<&Path>,
    cargo_meta: &Path,
    binaries_meta: &Path,
) -> String {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(workspace()).args(["nextest", "run", "--cargo-metadata"]);
    cmd.arg(cargo_meta);
    cmd.arg("--binaries-metadata").arg(binaries_meta);
    cmd.args(["--profile", profile, "--color", "never", "-E", filter]);
    if let Some(config) = config {
        cmd.arg("--config-file").arg(config);
    }
    // the outer nextest's variables (profile, run id, …) must not steer the inner run
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("NEXTEST") {
            cmd.env_remove(k);
        }
    }
    let (k, v) = env.split_once('=').unwrap();
    let out = cmd
        .env(k, v)
        .env_remove("MARE_TEST_EXPECT_BACKEND")
        .env_remove("CI")
        .output()
        .expect("cargo nextest must be installed");
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "inner nextest run failed:\n{report}");
    assert!(!report.contains("Compiling "), "inner nextest run unexpectedly rebuilt crates:\n{report}");
    assert!(report.contains("1 passed"), "the inner run did not run exactly the target test:\n{report}");
    report
}

/// Whether this test process was started by nextest.
fn under_nextest() -> bool {
    std::env::var("NEXTEST").as_deref() == Ok("1")
}

#[test]
fn standard_nextest_profiles_report_security_test_skips() {
    if !under_nextest() {
        println!(
            "SKIPPED[capability=nextest]: standard_nextest_profiles_report_security_test_skips (not run by nextest)"
        );
        return;
    }
    let real = workspace().join(".config/nextest.toml");
    let text = std::fs::read_to_string(&real).unwrap();
    // the same config without its per-test overrides
    let stripped_dir = tempfile::tempdir().unwrap();
    let stripped = stripped_dir.path().join("nextest.toml");
    let kept: Vec<&str> = text
        .split("\n[")
        .filter(|section| !section.starts_with("[profile.") || !section.contains(".overrides]]"))
        .collect();
    std::fs::write(&stripped, kept.join("\n[")).unwrap();
    assert!(!kept.join("\n[").contains("success-output"), "the stripped config still has an override");

    let (cargo_meta, binaries_meta) = write_reused_binary_metadata(stripped_dir.path());

    for profile in PROFILES {
        for (filter, env, marker) in SKIPS {
            let shown = nextest(profile, filter, env, None, &cargo_meta, &binaries_meta);
            assert!(shown.contains(marker), "profile {profile}: nextest's report hides the skip ({marker}):\n{shown}");
            let hidden = nextest(profile, filter, env, Some(&stripped), &cargo_meta, &binaries_meta);
            assert!(
                !hidden.contains(marker),
                "profile {profile}: the skip shows even without the overrides, so this check proves nothing:\n{hidden}"
            );
        }
    }
}
