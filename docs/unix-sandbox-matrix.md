# Unix sandbox acceptance matrix

This matrix is a test plan for verifying platform sandbox profiles and collecting backend evidence; it is not a claim that every cell has passed. Run on disposable machines or isolated containers; do not change host-wide namespace settings, bwrap permissions, or system security policy to fill a cell.

Record for every run: mare commit, date, OS and kernel, architecture, Rust/cc/git versions, bwrap path/version/owner/mode and mount `nosuid` status on Linux, effective UID, container restrictions, `mare doctor`, commands, exit status, and full test output. A passed test that internally skips for the backend or a missing capability is **not** evidence for that cell. Capture the actual backend and capability probe results. If a prerequisite is unavailable, mark the cell blocked with the reason; never infer success from another cell.

| Cell | Environment and required observation | Tests and additional checks |
| --- | --- | --- |
| macOS Seatbelt | Real Mac, `sandbox-exec` usable; record macOS, Xcode/CLT and SDK versions. Expected backend `seatbelt`. | Common tests and toolchain test below. Exercise an interactive PTY (shell, interrupt and job control), local git operations and the project's supported install/build commands under workspace-write; record exact commands and failures. No Mac result is claimed here. |
| Linux non-setuid bwrap | Non-setuid bwrap, unprivileged user, namespaces actually usable; expected `bwrap`. | Common tests and toolchain test. Check both network-enabled and restricted socket cases, and `net_proxy` only if its probe succeeds. Record `NoNewPrivs` in the sandbox child. |
| Linux setuid bwrap | Disposable VM/image with a root-owned setuid bwrap on a suid-enabled mount; run as unprivileged user, confirm it is actually selected and usable. Expected `bwrap`. | Same checks as non-setuid. In particular run both open-network and socket-restricted paths; installation of seccomp/no_new_privs before bwrap may prevent setuid startup. A mode bit alone does not prove privilege elevation worked; record mount and runtime conditions. |
| Linux Landlock fallback | Isolated environment with no usable bwrap and Landlock available; expected `landlock`. Record kernel ABI and capability probes. | Common tests and toolchain test; verify network and Unix socket restrictions, and report unsupported capabilities as such. |
| Linux namespace unavailable | Isolated environment where bwrap cannot create required namespaces; record the failure and whether Landlock is available. | Run common tests. Confirm the reported fallback or unavailable backend; requested unsupported capabilities must refuse or be explicitly accepted and reported partial. Do not call a fallback result a bwrap pass. |

Build first and use the same checkout and toolchain for each run. `cargo nextest` runs each test in its own process, as required by the environment-dependent tests. The first command covers spawn/PTY writes, backend refusal, capability handling and platform-specific network cases; individual tests have backend/capability guards. The second requires `cc` and `git` on PATH, creates and runs a small C program and runs `git init`/`git status` inside a temporary workspace with network enabled, and fails if the selected backend differs. It does not access the network or install packages.

```sh
cargo nextest run -p mare-cli --profile default --no-capture -E 'test(/suite::sandbox::/)'
MARE_TEST_EXPECT_BACKEND=seatbelt cargo nextest run -p mare-cli --profile default --no-capture --run-ignored ignored-only -E 'test(/suite::sandbox::unix_toolchain_matrix/)'
```

Replace `seatbelt` with `bwrap` or `landlock` as appropriate. Run the Linux proxy suite separately where `net_proxy` is available:

```sh
cargo nextest run -p mare-cli --profile default --no-capture -E 'test(/suite::net_proxy_linux::/)'
cargo test -p mare-binding base_seccomp_refuses_ptrace_without_a_socket_block -- --nocapture
```

For a controlled namespace failure or Landlock-only cell, use a disposable image/container configured by its own runtime, not global sysctls or modifications to the host bwrap binary. Test results depend on kernel, mount, container and privilege settings; record them rather than treating a container recipe as proof of a particular backend. A future deny-default or extra-hardening experiment needs its own explicit configuration and must fail closed or report a consciously accepted partial result, then repeat the matrix before any default change.
