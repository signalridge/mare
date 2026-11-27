//! The `mare` binary: everything lives in the `mare_cli` library (so the TUI can be driven
//! in-process by the e2e tests).

fn main() -> anyhow::Result<()> {
    mare_cli::main()
}
