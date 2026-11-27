//! Integration tests for `mare test` CLI subcommand and `expect.snapshot`.

use std::path::Path;
use std::process::Stdio;

use crate::common::{ChildCommandExt, mare_cmd};

fn run_mare_test(cwd: &Path, home: &Path, args: &[&str], envs: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let mut cmd = mare_cmd();
    cmd.arg("test")
        .args(args)
        .env("MARE_HOME", home)
        .env_remove("MARE_UPDATE_SNAPSHOTS")
        .env_remove("CI")
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.run_output();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn mare_test_runs_specs_filters_and_reports_file_line_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(proj.join("tests/sub")).unwrap();

    std::fs::write(
        proj.join("tests/pass_spec.lua"),
        r#"local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect
describe("math", function()
    it("adds numbers", function()
        expect.eq(1 + 2, 3)
    end)
end)
"#,
    )
    .unwrap();

    std::fs::write(
        proj.join("tests/sub/fail_spec.lua"),
        r#"local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect
describe("failing", function()
    it("checks equality", function()
        expect.eq(1, 2)
    end)
    it("passes when filtered", function()
        expect.eq(4, 4)
    end)
end)
"#,
    )
    .unwrap();

    // 1. Running all specs fails with exit 1 and includes file path + line number.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(1), "expected exit 1 on failure:\n{combined}");
    assert!(combined.contains("fail_spec.lua:4"), "expected file:line in failure:\n{combined}");
    assert!(combined.contains("failing > checks equality"), "expected test name in failure:\n{combined}");

    // 2. --filter runs only matching test cases and exits 0.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "--filter", "passes when filtered"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 with filter:\n{combined}");
    assert!(combined.contains("1 passed"), "expected 1 passed in summary:\n{combined}");

    // 3. Running a single passing spec file directly exits 0.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "tests/pass_spec.lua"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 for single passing spec:\n{combined}");

    // 4. --filter matching 0 tests returns exit code 2 with a clear message.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "--filter", "nonexistent_filter_xyz"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(2), "expected exit 2 when --filter matches 0 tests:\n{combined}");
    assert!(combined.contains("no tests matched filter"), "expected clear filter message:\n{combined}");
}

#[test]
fn mare_test_returns_exit_2_on_missing_or_syntax_error_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&proj).unwrap();

    // 1. Empty dir (no specs found) -> exit 2.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(2), "expected exit 2 when no specs found:\n{combined}");

    // 2. Syntax error in spec -> exit 2, not counted as a test case failure.
    std::fs::write(proj.join("bad_spec.lua"), "local x = (\n").unwrap();
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "bad_spec.lua"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(2), "expected exit 2 on syntax error:\n{combined}");
    assert!(combined.contains("bad_spec.lua"), "expected spec path in syntax error:\n{combined}");
    assert!(!combined.contains("1 failed"), "syntax/load error must not be counted as a failed test case:\n{combined}");

    // 3. Broken .mare/init.lua in a trusted project -> exit 2 even if spec would pass.
    std::fs::create_dir_all(proj.join(".mare")).unwrap();
    std::fs::write(proj.join(".mare/init.lua"), "local broken = (\n").unwrap();
    std::fs::write(proj.join("ok_spec.lua"), "mare.test.it('passes', function() mare.test.expect.eq(1, 1) end)\n")
        .unwrap();
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--trust", "ok_spec.lua"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(2), "expected exit 2 on broken .mare/init.lua:\n{combined}");
}

#[test]
fn mare_test_snapshot_pending_and_update_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(proj.join("tests")).unwrap();

    let spec_path = proj.join("tests/snap_spec.lua");
    std::fs::write(
        &spec_path,
        r#"local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect
describe("snapshot", function()
    it("serializes deterministically", function()
        expect.snapshot({ b = 2, a = { 10, 20 } }, "demo")
    end)
end)
"#,
    )
    .unwrap();

    let snap_dir = proj.join("tests/snapshots");
    let snap_file = snap_dir.join("snap_spec__demo.snap");
    let pending_file = snap_dir.join("snap_spec__demo.snap.new");

    // 1. Missing snapshot with MARE_UPDATE_SNAPSHOTS=no fails and does NOT write .snap or .snap.new.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[("MARE_UPDATE_SNAPSHOTS", "no")]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(1), "expected exit 1 on missing snapshot:\n{combined}");
    assert!(!snap_file.exists(), ".snap must not be created under MARE_UPDATE_SNAPSHOTS=no");
    assert!(!pending_file.exists(), ".snap.new must not be created under MARE_UPDATE_SNAPSHOTS=no");

    // 2. Default (new, non-CI) writes .snap.new pending file and fails.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(1), "expected exit 1 on missing snapshot in default new mode:\n{combined}");
    assert!(!snap_file.exists(), ".snap must not be created in new mode");
    assert!(pending_file.exists(), ".snap.new pending snapshot should be created");

    // 3. --update-snapshots promotes/writes .snap, removes .snap.new, and exits 0.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "--update-snapshots"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 with --update-snapshots:\n{combined}");
    assert!(snap_file.exists(), ".snap should be created by --update-snapshots");
    assert!(!pending_file.exists(), ".snap.new should be removed by --update-snapshots");

    // 4. Subsequent run with MARE_UPDATE_SNAPSHOTS=no passes.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[("MARE_UPDATE_SNAPSHOTS", "no")]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 on matching snapshot:\n{combined}");

    // 5. Mismatch writes .snap.new and prints diff.
    std::fs::write(
        &spec_path,
        r#"local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect
describe("snapshot", function()
    it("serializes deterministically", function()
        expect.snapshot({ b = 999, a = { 10, 20 } }, "demo")
    end)
end)
"#,
    )
    .unwrap();
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(1), "expected exit 1 on snapshot mismatch:\n{combined}");
    assert!(pending_file.exists(), ".snap.new should be written on mismatch");
    assert!(combined.contains("-"), "expected diff in output:\n{combined}");
}

#[test]
fn mare_test_snapshot_names_with_punctuation_do_not_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(proj.join("tests")).unwrap();

    std::fs::write(
        proj.join("tests/collide_spec.lua"),
        r#"local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect
describe("collision", function()
    it("keeps slash and colon distinct", function()
        expect.snapshot("slash_val", "a/b")
        expect.snapshot("colon_val", "a:b")
    end)
    it("keeps space and underscore distinct", function()
        expect.snapshot("space_val", "a b")
        expect.snapshot("under_val", "a_b")
    end)
end)
"#,
    )
    .unwrap();

    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "--update-snapshots"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 creating distinct snapshots:\n{combined}");

    let snap_dir = proj.join("tests/snapshots");
    let snap_count = std::fs::read_dir(&snap_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "snap"))
        .count();
    assert_eq!(snap_count, 4, "expected 4 distinct .snap files, found {snap_count}");

    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[("MARE_UPDATE_SNAPSHOTS", "no")]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 verifying all 4 distinct snapshots:\n{combined}");
}

#[cfg(unix)]
#[test]
fn mare_test_handles_symlink_cycle_and_shared_dir_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(proj.join("tests/sub")).unwrap();

    std::fs::write(
        proj.join("tests/sub/sample_spec.lua"),
        r#"local describe, it, expect = mare.test.describe, mare.test.it, mare.test.expect
describe("symlink", function()
    it("runs once", function()
        expect.eq(1, 1)
    end)
end)
"#,
    )
    .unwrap();

    // Shared directory symlink: alias -> sub
    std::os::unix::fs::symlink("sub", proj.join("tests/alias")).unwrap();

    // 1. Shared directory symlink must not duplicate spec execution.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 with shared dir symlink:\n{combined}");
    assert!(
        combined.contains("Tests: 1 passed, 0 failed, 1 total"),
        "expected exactly 1 test run (no duplicate via alias):\n{combined}"
    );

    // 2. Explicit CLI path argument that is a symlink should still work.
    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean", "tests/alias"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 when passing symlink path explicitly:\n{combined}");
    assert!(
        combined.contains("Tests: 1 passed, 0 failed, 1 total"),
        "expected 1 test run via explicit symlink path:\n{combined}"
    );

    // 3. Self-referential symlink cycle: loop -> .
    std::os::unix::fs::symlink(".", proj.join("tests/loop")).unwrap();

    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 with symlink cycle:\n{combined}");
    assert!(
        combined.contains("Tests: 1 passed, 0 failed, 1 total"),
        "expected exactly 1 test run with symlink cycle present:\n{combined}"
    );
}

#[test]
fn mare_test_host_runs_third_party_plugin_lifecycle_in_isolation() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(proj.join("plugins")).unwrap();
    std::fs::create_dir_all(proj.join("tests")).unwrap();

    std::fs::write(
        proj.join("plugins/tally.lua"),
        r#"return {
    name = "tally",
    inject = { "tools", "commands", "ui" },
    config_schema = {
        warn_after = { type = "integer", default = 25 },
        show_in_status = { type = "boolean", default = true },
    },
    apply = function(ctx, cfg)
        ctx.tools:register("text_stats", {
            description = "Count lines, words, and chars",
            parameters = {
                type = "object",
                properties = { text = { type = "string" } },
                required = { "text" },
            },
            side_effect = "read",
            execute = function(args)
                local text = args.text or ""
                local _, lines = text:gsub("\n", "\n")
                local words = 0
                for _ in text:gmatch("%S+") do words = words + 1 end
                return { content = ("lines: %d\nwords: %d\nchars: %d"):format(lines + 1, words, #text) }
            end,
        })
        ctx.commands:register("tally", {
            description = "Show tally status",
            run = function() return tostring(cfg.warn_after) end,
        })
    end,
}
"#,
    )
    .unwrap();

    std::fs::write(
        proj.join("tests/tally_spec.lua"),
        r#"local t = require("mare.test")

t.describe("tally", function()
    t.it("runs inside mare.test.host with real builtins and cleans up on dispose", function()
        local host = t.host({ builtins = { "tools", "commands", "ui" } })
        local plugin = dofile("plugins/tally.lua")
        host:load(plugin, { warn_after = 10, show_in_status = false })
        t.expect.settled(host)
        t.expect.eq(host:state("tally"), "ACTIVE")

        local tool = assert(host:get("tools"):get("text_stats"))
        local res = tool.execute({ text = "alpha beta\ngamma" })
        t.expect.eq(res.content, "lines: 2\nwords: 3\nchars: 16")

        host:dispose()
        t.expect.eq(host:get("tools"), nil)
    end)
end)
"#,
    )
    .unwrap();

    let (code, stdout, stderr) = run_mare_test(&proj, &home, &["--clean"], &[]);
    let combined = format!("{stdout}\n{stderr}");
    assert_eq!(code, Some(0), "expected exit 0 for mare.test.host third-party spec:\n{combined}");
    assert!(combined.contains("Tests: 1 passed, 0 failed, 1 total"), "expected 1 passed test:\n{combined}");
}
