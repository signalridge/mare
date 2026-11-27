//! `mare test` subcommand: discover and execute `*_spec.lua` tests via `mare.test`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use mare_host::{Host, HostOptions};
use mare_plugin::SpecOptions;
use mare_shared::ui::Mode;

fn is_spec_file(path: &Path) -> bool {
    path.is_file() && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("_spec.lua"))
}

fn should_skip_dir(path: &Path) -> bool {
    matches!(path.file_name().and_then(|n| n.to_str()), Some(".git" | "target" | "node_modules" | "snapshots"))
}

fn collect_specs_in_dir(dir: &Path, out: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading directory {}: {e}", dir.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            if !should_skip_dir(&path) {
                collect_specs_in_dir(&path, out)?;
            }
        } else if is_spec_file(&path) {
            out.insert(path);
        }
    }
    Ok(())
}

fn discover_specs(cwd: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut found = BTreeSet::new();
    if paths.is_empty() {
        collect_specs_in_dir(cwd, &mut found)?;
    } else {
        for p in paths {
            let resolved = if p.is_absolute() { p.clone() } else { cwd.join(p) };
            if !resolved.exists() {
                return Err(format!("path not found: {}", resolved.display()));
            }
            if resolved.is_file() {
                found.insert(resolved);
            } else if resolved.is_dir() {
                collect_specs_in_dir(&resolved, &mut found)?;
            }
        }
    }
    if found.is_empty() {
        return Err(format!("no *_spec.lua files found under {}", cwd.display()));
    }
    Ok(found.into_iter().collect())
}

/// Run `mare test` and return the process exit code:
/// - `0`: all tests passed
/// - `1`: one or more test cases failed
/// - `2`: usage, discovery, or spec load/syntax error
pub(crate) async fn run(
    cwd: &Path,
    trusted: bool,
    no_plugins: bool,
    paths: &[PathBuf],
    filter: Option<&str>,
    update_snapshots: bool,
) -> Result<i32> {
    let specs = match discover_specs(cwd, paths) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("mare test: {err}");
            return Ok(2);
        }
    };

    let opts = HostOptions {
        cwd: cwd.to_path_buf(),
        mode: Mode::Print,
        config: serde_json::json!({}),
        load_init: !no_plugins,
        project_trusted: trusted,
        args: serde_json::json!({ "no_session": true }),
        no_user_plugins: no_plugins,
    };
    let host = match Host::new(opts).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("mare test: failed to start host: {e:#}");
            return Ok(2);
        }
    };

    // specs may `require` the kernel libraries (the vendored cordis-lua)
    host.kernel.expose_test_boot();

    if let Some(err) = host.config_error() {
        eprintln!("mare test: {err}");
        host.shutdown().await;
        return Ok(2);
    }
    if let Err(msg) = crate::report_config_warnings(&host, true) {
        eprintln!("mare test: {msg}");
        host.shutdown().await;
        return Ok(2);
    }

    let mut total_cases = 0usize;
    let mut total_passed = 0usize;
    let mut total_filtered = 0usize;
    let mut load_errors = Vec::new();
    let mut failures = Vec::new();

    for spec in &specs {
        let label = spec.strip_prefix(cwd).unwrap_or(spec).display().to_string();
        let source = match std::fs::read_to_string(spec) {
            Ok(s) => s,
            Err(e) => {
                load_errors.push(format!("ERROR {label}: failed to read spec file: {e}"));
                continue;
            }
        };
        let spec_opts =
            SpecOptions { filter: filter.map(str::to_owned), update_snapshots, spec_path: Some(spec.clone()) };
        match mare_plugin::run_spec_with_options(host.lua(), &label, &source, &spec_opts).await {
            Ok(report) => {
                total_cases += report.total;
                total_passed += report.passed;
                total_filtered += report.filtered;
                if report.load_error {
                    for f in report.failures {
                        load_errors.push(format!("ERROR {label} :: {}\n{}", f.name, f.error));
                    }
                } else if !report.failures.is_empty() {
                    for f in report.failures {
                        failures.push(format!("FAIL {label} :: {}\n{}", f.name, f.error));
                    }
                } else if report.total == 0 && report.filtered == 0 {
                    failures.push(format!("FAIL {label}: spec file defined 0 test cases"));
                }
            }
            Err(e) => {
                load_errors.push(format!("ERROR {label}: runner error: {e}"));
            }
        }
    }

    host.shutdown().await;

    for err in &load_errors {
        eprintln!("{err}\n");
    }
    for failure in &failures {
        eprintln!("{failure}\n");
    }

    let failed_count = failures.len();
    if total_filtered > 0 {
        println!(
            "Tests: {total_passed} passed, {failed_count} failed, {total_cases} total ({total_filtered} filtered)"
        );
    } else {
        println!("Tests: {total_passed} passed, {failed_count} failed, {total_cases} total");
    }

    if !load_errors.is_empty() {
        Ok(2)
    } else if !failures.is_empty() {
        Ok(1)
    } else if let Some(pattern) = filter
        && total_cases == 0
    {
        eprintln!("mare test: no tests matched filter {pattern:?}");
        Ok(2)
    } else {
        Ok(0)
    }
}
