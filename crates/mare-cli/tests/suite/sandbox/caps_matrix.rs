//! Capability declarations must agree with the behavior of real sandboxed commands.

use super::host;
use crate::common::*;
use crate::suite::sandbox_expect::require_backend;
use mare_binding::ossandbox::expect;

#[derive(Clone, Copy)]
enum Row {
    FsWrite,
    FsProtect,
    FsHostProtect,
    NetBlock,
}

impl Row {
    fn name(self) -> &'static str {
        match self {
            Self::FsWrite => "fs_write",
            Self::FsProtect => "fs_protect",
            Self::FsHostProtect => "fs_host_protect",
            Self::NetBlock => "net_block",
        }
    }

    fn backends(self) -> &'static [&'static str] {
        match self {
            Self::FsWrite | Self::NetBlock => expect::ANY,
            Self::FsProtect => &["bwrap", "seatbelt"],
            Self::FsHostProtect => &["bwrap"],
        }
    }

    fn level(self) -> &'static str {
        match self {
            Self::FsWrite | Self::FsProtect => "workspace-write",
            Self::FsHostProtect | Self::NetBlock => "read-only",
        }
    }
}

struct Fixture {
    program: &'static str,
    probe: &'static str,
    control: &'static str,
    probe_arg: String,
    control_arg: String,
    side_effect: std::path::PathBuf,
    _outside: Option<tempfile::TempDir>,
    _listener: Option<std::net::TcpListener>,
}

async fn fixture(row: Row) -> (TestEnv, Fixture) {
    let env = TestEnv::new().await;
    let marker = env.cwd.join("probe-ran");
    let (program, probe, control, probe_arg, control_arg, side_effect, outside, listener) = match row {
        Row::FsWrite => {
            let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
            let path = dunce::canonicalize(outside.path()).unwrap().join("w");
            (
                "sh",
                "echo x > \"$1\"",
                "echo x > \"$1\"",
                path.display().to_string(),
                env.cwd.join("in.txt").display().to_string(),
                path,
                Some(outside),
                None,
            )
        }
        Row::FsProtect => {
            assert!(fixture_git(&env.cwd).args(["init", "-q"]).status().expect("git is required").success());
            std::fs::create_dir_all(env.cwd.join(".git/hooks")).unwrap();
            let path = env.cwd.join(".git/hooks/pre-commit");
            assert!(!path.exists(), "git fixture already contains {}", path.display());
            (
                "sh",
                "echo x > \"$1\"",
                "echo x > \"$1\"",
                path.display().to_string(),
                env.cwd.join("src.txt").display().to_string(),
                path,
                None,
                None,
            )
        }
        Row::FsHostProtect => {
            let state = env.home.path().join("state");
            mare_shared::private::create_dir_all(&state.join("host")).unwrap();
            let token = state.join("host/token");
            std::fs::write(&token, "SECRET-TOKEN").unwrap();
            let control = state.join("control.txt");
            std::fs::write(&control, "CONTROL").unwrap();
            (
                "sh",
                "echo x > \"$2\"; echo STARTED; cat \"$1\"",
                "cat \"$1\"",
                token.display().to_string(),
                control.display().to_string(),
                marker,
                None,
                None,
            )
        }
        Row::NetBlock => {
            assert!(
                std::process::Command::new("bash").arg("--version").output().is_ok(),
                "bash is required for Unix sandbox acceptance"
            );
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port().to_string();
            (
                "bash",
                "echo x > \"$2\"; echo STARTED; exec 3<>/dev/tcp/127.0.0.1/$1 && echo CONNECTED",
                "exec 3<>/dev/tcp/127.0.0.1/$1 && echo CONNECTED",
                port.clone(),
                port,
                marker,
                None,
                Some(listener),
            )
        }
    };
    (
        env,
        Fixture {
            program,
            probe,
            control,
            probe_arg,
            control_arg,
            side_effect,
            _outside: outside,
            _listener: listener,
        },
    )
}

fn expose_fixture(h: &mare_host::Host, row: Row, f: &Fixture) {
    global(h, "ROW", row.name());
    global(h, "LEVEL", row.level());
    global(h, "PROGRAM", f.program);
    global(h, "PROBE", f.probe);
    global(h, "CONTROL", f.control);
    global(h, "PROBE_ARG", f.probe_arg.clone());
    global(h, "CONTROL_ARG", f.control_arg.clone());
    global(h, "SIDE_EFFECT", f.side_effect.display().to_string());
    global(h, "NETWORK", !matches!(row, Row::NetBlock));
}

fn non_target_accept(row: Row, caps: &[String]) -> Option<Vec<&'static str>> {
    // `workspace-write` always requires `fs_protect`; backends that don't declare it (Landlock)
    // still enforce `fs_write` once that non-target gap is explicitly accepted.
    (matches!(row, Row::FsWrite) && !caps.iter().any(|cap| cap == "fs_protect")).then(|| vec!["fs_protect"])
}

async fn declared(row: Row, name: &str) {
    let (env, f) = fixture(row).await;
    let h = host(&env).await;
    if !require_backend(&h, name, row.backends()).await {
        return h.shutdown().await;
    }
    let caps: Vec<String> = eval(&h, "return mare.proc.sandbox.capabilities().caps").await;
    assert!(caps.iter().any(|cap| cap == row.name()), "{} not declared in {caps:?}", row.name());
    if matches!(row, Row::FsHostProtect) {
        assert_eq!(std::fs::read_to_string(&f.probe_arg).unwrap(), "SECRET-TOKEN");
    }
    expose_fixture(&h, row, &f);
    global(&h, "ACCEPT_NON_TARGET", non_target_accept(row, &caps));
    let (probe_code, probe_output, control_code, control_output): (i64, String, i64, String) = eval(
        &h,
        r#"
        local p = mare.proc.run({ cmd = PROGRAM, args = { "-c", PROBE, "probe", PROBE_ARG, SIDE_EFFECT },
            sandbox = { level = LEVEL, network = NETWORK, accept_partial = ACCEPT_NON_TARGET } })
        local c = mare.proc.run({ cmd = PROGRAM, args = { "-c", CONTROL, "control", CONTROL_ARG },
            sandbox = { level = LEVEL, network = true, accept_partial = ACCEPT_NON_TARGET } })
        return p.code, p.stdout .. p.stderr, c.code, c.stdout .. c.stderr
    "#,
    )
    .await;
    assert_ne!(probe_code, 0, "{} probe unexpectedly succeeded: {probe_output}", row.name());
    assert_eq!(control_code, 0, "{} control failed: {control_output}", row.name());
    match row {
        Row::FsWrite | Row::FsProtect => {
            assert!(!f.side_effect.exists(), "{} probe wrote its target", row.name());
            assert!(std::path::Path::new(&f.control_arg).exists(), "{} control did not write its target", row.name());
        }
        Row::FsHostProtect => {
            assert!(probe_output.contains("STARTED") && !probe_output.contains("SECRET-TOKEN"), "{probe_output}");
            assert!(control_output.contains("CONTROL"), "{control_output}");
        }
        Row::NetBlock => {
            assert!(probe_output.contains("STARTED") && !probe_output.contains("CONNECTED"), "{probe_output}");
            assert!(control_output.contains("CONNECTED"), "{control_output}");
        }
    }
    h.shutdown().await;
}

async fn undeclared(row: Row, name: &str) {
    let (mut env, f) = fixture(row).await;
    let first = host(&env).await;
    if !require_backend(&first, name, expect::ANY).await {
        return first.shutdown().await;
    }
    let caps: Vec<String> = eval(&first, "return mare.proc.sandbox.capabilities().caps").await;
    first.shutdown().await;
    drop(first);
    let remaining = caps.iter().filter(|cap| cap.as_str() != row.name()).cloned().collect::<Vec<_>>().join(",");
    env.set_var("MARE_SANDBOX_CAPS", remaining);
    let h = host(&env).await;
    expose_fixture(&h, row, &f);
    let mut accept = vec![row.name()];
    if let Some(extra) = non_target_accept(row, &caps) {
        accept.extend(extra);
    }
    global(&h, "ACCEPT", accept);
    let (error, enforcement, unavailable): (String, String, Vec<String>) = eval(
        &h,
        r#"
        local policy = { level = LEVEL, network = NETWORK }
        local args = { cmd = PROGRAM, args = { "-c", PROBE, "probe", PROBE_ARG, SIDE_EFFECT }, sandbox = policy }
        local ok, err = pcall(mare.proc.run, args)
        assert(not ok, "undeclared capability unexpectedly ran")
        policy.accept_partial = ACCEPT
        local r = mare.proc.run(args)
        return tostring(err), r.sandbox.enforcement, r.sandbox.unavailable
    "#,
    )
    .await;
    assert!(error.contains("SANDBOX_INSUFFICIENT") && error.contains(row.name()), "{error}");
    assert_eq!(enforcement, "partial");
    assert!(unavailable.iter().any(|cap| cap == row.name()), "{unavailable:?}");
    h.shutdown().await;
}

async fn without_backend(row: Row) {
    let (mut env, f) = fixture(row).await;
    env.set_var("MARE_SANDBOX_BACKEND", "none");
    let h = host(&env).await;
    expose_fixture(&h, row, &f);
    let error: String = eval(
        &h,
        r#"
        local ok, err = pcall(mare.proc.run, { cmd = PROGRAM,
            args = { "-c", PROBE, "probe", PROBE_ARG, SIDE_EFFECT },
            sandbox = { level = LEVEL, network = NETWORK } })
        assert(not ok, "command unexpectedly ran without a backend")
        return tostring(err)
    "#,
    )
    .await;
    assert!(error.contains("SANDBOX_UNAVAILABLE"), "{error}");
    assert!(!f.side_effect.exists(), "{} probe ran without a backend", row.name());
    h.shutdown().await;
}

macro_rules! matrix_row {
    ($row:ident, $declared:ident, $undeclared:ident, $without:ident) => {
        #[tokio::test]
        async fn $declared() {
            local(declared(Row::$row, stringify!($declared))).await
        }
        #[tokio::test]
        async fn $undeclared() {
            local(undeclared(Row::$row, stringify!($undeclared))).await
        }
        #[tokio::test]
        async fn $without() {
            local(without_backend(Row::$row)).await
        }
    };
}

matrix_row!(
    FsWrite,
    fs_write_declared_blocks_the_probe_and_allows_the_control,
    fs_write_undeclared_is_refused_then_partial_when_accepted,
    fs_write_without_a_backend_is_unavailable
);
matrix_row!(
    FsProtect,
    fs_protect_declared_blocks_the_probe_and_allows_the_control,
    fs_protect_undeclared_is_refused_then_partial_when_accepted,
    fs_protect_without_a_backend_is_unavailable
);
matrix_row!(
    FsHostProtect,
    fs_host_protect_declared_blocks_the_probe_and_allows_the_control,
    fs_host_protect_undeclared_is_refused_then_partial_when_accepted,
    fs_host_protect_without_a_backend_is_unavailable
);
matrix_row!(
    NetBlock,
    net_block_declared_blocks_the_probe_and_allows_the_control,
    net_block_undeclared_is_refused_then_partial_when_accepted,
    net_block_without_a_backend_is_unavailable
);
