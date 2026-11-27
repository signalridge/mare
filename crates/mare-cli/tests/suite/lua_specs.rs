//! `mare.test` Lua spec runner.
//!
//! Each directory under `crates/mare-plugin/preset/tests/<dir>/` has one Rust test entry here.
//! `lua_spec_inventory` ensures no directory is left without an entry.

use std::path::{Path, PathBuf};

use crate::common::{TestEnv, local};

macro_rules! register_spec_dirs {
    ($($dir:ident),* $(,)?) => {
        const REGISTERED_DIRS: &[&str] = &[$(stringify!($dir)),*];
        const EXECUTABLE_ENTRIES: &[&str] = &[$(stringify!($dir)),*];

        $(
            #[tokio::test]
            async fn $dir() {
                local(run_spec_dir(stringify!($dir))).await;
            }
        )*
    };
}

register_spec_dirs!(lib, test_harness, text, tools, web_fetch_http);

/// The kernel's own Lua specs (crates/mare-kernel/lua-specs/cordis_host): the cordis host,
/// boot.lua and the host plugins, with the kernel libraries enabled. They test kernel internals,
/// so they live with the kernel, outside the plugin public-API audit of preset/tests.
#[tokio::test]
async fn cordis_host() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mare-kernel/lua-specs/cordis_host");
    if let Err(report) = local(try_run_spec_dir(&dir)).await {
        panic!("{report}");
    }
}

fn preset_tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset/tests")
}

async fn try_run_spec_dir(dir: &Path) -> Result<usize, String> {
    if !dir.is_dir() {
        return Err(format!("missing spec directory {}: expected a directory of *_spec.lua files", dir.display()));
    }
    let mut specs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading spec directory {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("_spec.lua")))
        .collect();
    specs.sort();

    if specs.is_empty() {
        return Err(format!("empty spec directory {}: expected at least one *_spec.lua file", dir.display()));
    }

    let env = TestEnv::new().await;
    let is_cordis_host = dir.file_name().is_some_and(|n| n == "cordis_host");
    let mut builder = env.host();
    if is_cordis_host {
        crate::common::install_probe_events_plugin(&env, "cordis_host", &[("probe", "emit")]);
        builder = builder.user_plugins();
    }
    let host = builder.build().await;
    // the kernel's cordis_host specs (crates/mare-kernel/lua-specs): kernel libraries on
    if is_cordis_host {
        host.kernel.expose_test_boot();
    }

    let mut total_cases = 0usize;
    let mut failures = Vec::new();

    for spec in &specs {
        let label = spec.display().to_string();
        let source = match std::fs::read_to_string(spec) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{label}: failed to read spec file: {e}"));
                continue;
            }
        };
        match mare_plugin::run_spec(host.lua(), &label, &source).await {
            Ok(report) => {
                total_cases += report.total;
                if !report.failures.is_empty() {
                    for f in report.failures {
                        failures.push(format!("{label}: {}\n{}", f.name, f.error));
                    }
                } else if report.total == 0 {
                    failures.push(format!("{label}: spec file defined 0 test cases"));
                }
            }
            Err(e) => {
                failures.push(format!("{label}: runner error: {e}"));
            }
        }
    }

    host.shutdown().await;

    if failures.is_empty() {
        Ok(total_cases)
    } else {
        Err(format!("{} Lua spec failure(s) in {}:\n\n{}", failures.len(), dir.display(), failures.join("\n\n")))
    }
}

async fn run_spec_dir(name: &str) {
    let dir = preset_tests_dir().join(name);
    if let Err(report) = try_run_spec_dir(&dir).await {
        panic!("{report}");
    }
}

fn verify_spec_inventory(root: &Path, registered_dirs: &[&str], executable_entries: &[&str]) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("missing preset/tests directory: {}", root.display()));
    }

    let mut actual_dirs = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())?.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            return Err(format!("unexpected non-directory entry in preset/tests: {}", path.display()));
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let has_spec = std::fs::read_dir(&path).map_err(|e| e.to_string())?.filter_map(Result::ok).any(|e| {
            e.path().is_file()
                && e.path().file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("_spec.lua"))
        });
        if !has_spec {
            return Err(format!("empty spec directory in preset/tests: {}", path.display()));
        }
        actual_dirs.push(name);
    }
    actual_dirs.sort();

    let mut expected: Vec<String> = registered_dirs.iter().map(|s| (*s).to_string()).collect();
    expected.sort();

    if actual_dirs != expected {
        return Err(format!(
            "preset/tests directories ({actual_dirs:?}) and REGISTERED_DIRS ({expected:?}) in lua_specs.rs must match"
        ));
    }
    for dir in registered_dirs {
        if !executable_entries.contains(dir) {
            return Err(format!(
                "spec directory {dir:?} is listed in REGISTERED_DIRS without an executable test entry"
            ));
        }
    }
    Ok(())
}

#[test]
fn lua_spec_inventory() {
    verify_spec_inventory(&preset_tests_dir(), REGISTERED_DIRS, EXECUTABLE_ENTRIES).unwrap();
}

#[test]
fn inventory_rejects_registered_dir_without_executable_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("foo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("foo_spec.lua"), "mare.test.it('ok', function() end)\n").unwrap();

    let err = verify_spec_inventory(tmp.path(), &["foo"], &[]).unwrap_err();
    assert!(err.contains("foo") && err.contains("executable"), "unexpected error: {err}");
}

#[tokio::test]
async fn empty_spec_directory_produces_diagnostic_failure() {
    local(async {
        let tmp = tempfile::tempdir().unwrap();
        let err = try_run_spec_dir(tmp.path()).await.unwrap_err();
        assert!(err.contains("empty spec directory"), "unexpected error: {err}");
    })
    .await;
}

#[tokio::test]
async fn spec_failures_continue_across_cases_and_files_with_file_and_line() {
    local(async {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a_empty_spec.lua"), "local x = 1\n").unwrap();
        std::fs::write(
            tmp.path().join("b_fail_spec.lua"),
            concat!(
                "local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect\n",
                "describe('suite', function()\n",
                "  it('first', function()\n",
                "    expect.eq(1, 2)\n",
                "  end)\n",
                "  it('second', function()\n",
                "    expect.truthy(false)\n",
                "  end)\n",
                "end)\n",
            ),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("c_later_fail_spec.lua"),
            concat!(
                "mare.test.it('later failure', function()\n",
                "  mare.test.expect.eq('after-b', 'expected')\n",
                "end)\n",
            ),
        )
        .unwrap();

        let err = try_run_spec_dir(tmp.path()).await.unwrap_err();
        assert!(err.contains("a_empty_spec.lua: spec file defined 0 test cases"), "{err}");
        assert!(err.contains("b_fail_spec.lua:4:"), "missing line 4 in:\n{err}");
        assert!(err.contains("b_fail_spec.lua:7:"), "missing line 7 in:\n{err}");
        assert!(err.contains("c_later_fail_spec.lua:2:"), "missing line 2 of 3rd file in:\n{err}");
    })
    .await;
}
