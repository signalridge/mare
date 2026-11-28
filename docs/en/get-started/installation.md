# Installation

Install mare using the method best suited to your operating system, then run `mare init` and `mare doctor` to verify the setup.

## Supported platforms

Release artifacts are built for six targets (`.github/workflows/release.yaml`):

| Platform | Target | Artifacts |
| --- | --- | --- |
| macOS, Apple silicon | `aarch64-apple-darwin` | `.tar.gz`, Homebrew formula, Nix |
| macOS, Intel | `x86_64-apple-darwin` | `.tar.gz`, Homebrew formula, Nix |
| Linux, x86-64 (static musl) | `x86_64-unknown-linux-musl` | `.tar.gz`, `.deb`, `.rpm`, Homebrew formula, AUR, Nix, GHCR container |
| Linux, ARM64 (static musl) | `aarch64-unknown-linux-musl` | `.tar.gz`, `.deb`, `.rpm`, Homebrew formula, AUR, Nix, GHCR container |
| Windows, x86-64 | `x86_64-pc-windows-msvc` | `.zip`, Scoop manifest |
| Windows, ARM64 | `aarch64-pc-windows-msvc` | `.zip`, Scoop manifest |

mare keeps its files in XDG-style directories on every platform, including Windows (see [Files mare uses](#files-mare-uses)).

## macOS

### Quick install script

Download the latest macOS archive (`aarch64-apple-darwin` or `x86_64-apple-darwin`) and verify its SHA-256 checksum against `checksums.txt`:

```sh
curl -fsSL https://signalridge.github.io/mare/install.sh | sh
```

By default, the script installs `mare` into `~/.local/bin`. Override `MARE_INSTALL_DIR` to change the target directory, or set `MARE_VERSION=vX.Y.Z` to install a specific release.

### Homebrew

```sh
brew tap signalridge/tap
brew trust signalridge/tap
brew install signalridge/tap/mare
```

### Nix (Flake and NUR)

```sh
# Flake (run directly or install to your profile)
nix run github:signalridge/mare
nix profile install github:signalridge/mare#mare

# NUR prebuilt binary
nix profile install github:signalridge/nur-packages#mare
```

### Manual archive (`.tar.gz`)

Download `mare-$VERSION-$TARGET.tar.gz` and its `.sha256` file from [GitHub Releases](https://github.com/signalridge/mare/releases), verify the digest with `shasum -a 256`, and install:

```sh
VERSION=vX.Y.Z
TARGET=aarch64-apple-darwin   # or x86_64-apple-darwin for Intel Macs
ARCHIVE="mare-$VERSION-$TARGET.tar.gz"
EXPECTED=$(awk 'NR==1 && length($1)==64 && $1 ~ /^[[:xdigit:]]+$/{print tolower($1)}' "$ARCHIVE.sha256")
ACTUAL=$(shasum -a 256 "$ARCHIVE" | awk 'NR==1{print tolower($1)}')
if [ -n "$EXPECTED" ] && [ "$ACTUAL" = "$EXPECTED" ]; then
  mkdir -p ~/.local/bin &&
  tar xzf "$ARCHIVE" &&
  install -m 755 "mare-$VERSION-$TARGET/mare" ~/.local/bin/mare &&
  ~/.local/bin/mare --version
else
  echo "Checksum mismatch: expected $EXPECTED, got $ACTUAL" >&2
  false
fi
```

If you downloaded the archive with a browser, clear the macOS quarantine attribute before running it: `xattr -d com.apple.quarantine ./mare`.

## Linux

### Quick install script

Download the latest Linux archive (`x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`, statically linked) and verify its SHA-256 checksum against `checksums.txt`:

```sh
curl -fsSL https://signalridge.github.io/mare/install.sh | sh
```

By default, the script installs `mare` into `~/.local/bin`. Override `MARE_INSTALL_DIR` to change the target directory, or set `MARE_VERSION=vX.Y.Z` to install a specific release.

### Debian / Ubuntu (`.deb`)

Download the `.deb` package matching your architecture (`amd64` or `arm64`) from [GitHub Releases](https://github.com/signalridge/mare/releases), verify its digest against `checksums.txt`, and install:

```sh
sudo apt install ./mare_<version>_linux_amd64.deb
# or for ARM64: sudo apt install ./mare_<version>_linux_arm64.deb
```

### Fedora / RHEL (`.rpm`)

Download the `.rpm` package matching your architecture (`amd64` or `arm64`) from [GitHub Releases](https://github.com/signalridge/mare/releases), verify its digest against `checksums.txt`, and install:

```sh
sudo dnf install ./mare_<version>_linux_amd64.rpm
# or for ARM64: sudo dnf install ./mare_<version>_linux_arm64.rpm
```

### Arch Linux (AUR)

```sh
yay -S mare-bin
# or: paru -S mare-bin
```

### Homebrew (Linuxbrew)

```sh
brew tap signalridge/tap
brew trust signalridge/tap
brew install signalridge/tap/mare
```

### Nix (Flake and NUR)

```sh
# Flake (run directly or install to your profile)
nix run github:signalridge/mare
nix profile install github:signalridge/mare#mare

# NUR prebuilt binary
nix profile install github:signalridge/nur-packages#mare
```

### Manual archive (`.tar.gz`)

Download `mare-$VERSION-$TARGET.tar.gz` and its `.sha256` file from [GitHub Releases](https://github.com/signalridge/mare/releases), verify the digest with `sha256sum`, and install:

```sh
VERSION=vX.Y.Z
TARGET=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl for ARM64
ARCHIVE="mare-$VERSION-$TARGET.tar.gz"
EXPECTED=$(awk 'NR==1 && length($1)==64 && $1 ~ /^[[:xdigit:]]+$/{print tolower($1)}' "$ARCHIVE.sha256")
ACTUAL=$(sha256sum "$ARCHIVE" | awk 'NR==1{print tolower($1)}')
if [ -n "$EXPECTED" ] && [ "$ACTUAL" = "$EXPECTED" ]; then
  mkdir -p ~/.local/bin &&
  tar xzf "$ARCHIVE" &&
  install -m 755 "mare-$VERSION-$TARGET/mare" ~/.local/bin/mare &&
  ~/.local/bin/mare --version
else
  echo "Checksum mismatch: expected $EXPECTED, got $ACTUAL" >&2
  false
fi
```

Make sure `~/.local/bin` (or the directory you chose) is on your `PATH`. On Linux, install `bubblewrap` through your distribution package manager if you want full namespace-based OS sandboxing (Landlock is also supported where the kernel enables it).

## Windows

### Quick install script (PowerShell)

Download the latest Windows release archive (`x86_64-pc-windows-msvc` or `aarch64-pc-windows-msvc`), verify its SHA-256 checksum against `checksums.txt`, install `mare.exe` into `%LOCALAPPDATA%\mare\bin` (override with `$env:MARE_INSTALL_DIR`), and add it to your User `PATH`:

```powershell
irm https://signalridge.github.io/mare/install.ps1 | iex
```

Set `$env:MARE_VERSION = "vX.Y.Z"` before running the command to pin a specific version.

### Scoop

```powershell
scoop bucket add signalridge https://github.com/signalridge/scoop-bucket
scoop install signalridge/mare
```

### Manual archive (`.zip`)

Download `mare-$v-$t.zip` and its `.sha256` file from [GitHub Releases](https://github.com/signalridge/mare/releases), verify the SHA-256 digest before extracting, then copy `mare-$v-$t\mare.exe` to a directory on your `PATH`:

```powershell
$ErrorActionPreference = "Stop"
$v = "vX.Y.Z"; $t = "x86_64-pc-windows-msvc"   # or aarch64-pc-windows-msvc
$archive = "mare-$v-$t.zip"
$expected = ([string](Get-Content "$archive.sha256" -Raw)).Trim().Split()[0].ToLower()
$actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
if ($expected -notmatch '^[0-9a-f]{64}$' -or $actual -ne $expected) {
  throw "Checksum mismatch: expected $expected, got $actual"
}
Expand-Archive $archive -DestinationPath .
```

Windows also provides an optional enhanced sandbox tier. `mare sandbox status` shows whether it is installed and runs a self-test; `mare sandbox setup` installs it and asks for elevation once.

## Container (Docker / GHCR)

Multi-architecture (`linux/amd64`, `linux/arm64`) images based on Ubuntu 24.04 (including `tini` as PID 1 init for zombie reaping, `git`, and `bubblewrap`) are published to GitHub Container Registry and signed with Cosign:

```sh
docker pull ghcr.io/signalridge/mare:latest
docker run --rm -it -v "$PWD:/workspace" -w /workspace ghcr.io/signalridge/mare:latest
```

## Release artifacts and verification

Releases are published on GitHub at [`signalridge/mare`](https://github.com/signalridge/mare/releases). Each published GitHub Release (`vX.Y.Z`) attaches:

- `.tar.gz` archives (macOS and Linux) and `.zip` archives (Windows) with per-archive `.sha256` files;
- `.deb` and `.rpm` packages for Linux (`amd64` and `arm64`);
- `checksums.txt`, its Cosign signature bundle (`checksums.txt.sigstore.json`), SPDX SBOMs (`*.sbom.spdx.json`), and SLSA provenance (`multiple.intoto.jsonl`).

Each archive unpacks into a single directory of the same base name containing the `mare` binary (`mare.exe` on Windows), a `types/` directory with Lua type definitions, `LICENSE`, `THIRD_PARTY_NOTICES.md`, `licenses/cordis-LICENSE`, `licenses/RUST-THIRD-PARTY-NOTICES.txt`, and `README.md`.

## Build from source

You need the Rust toolchain at version 1.100.0 or newer (`rust-version` in `Cargo.toml`; the project uses edition 2024) and a C compiler, because the Lua 5.5 runtime is compiled from vendored sources. `rust-toolchain.toml` pins the exact release that CI and the release builds use, so rustup installs and selects it automatically inside the checkout.

Install directly with Cargo:

```sh
cargo install --git https://github.com/signalridge/mare --locked --bin mare mare-cli
mare --version
```

Or clone the repository and build locally:

```sh
git clone https://github.com/signalridge/mare
cd mare
cargo build --release --locked -p mare-cli
./target/release/mare --version
```

The release workflow builds exactly this way, with `--locked` so `Cargo.lock` decides dependency versions. Copy `target/release/mare` somewhere on your `PATH`. The release profile enables LTO, so a release build takes noticeably longer than a debug build.

The repository also has a `justfile`:

| Command | What it does |
| --- | --- |
| `just run <args>` | `cargo run -- <args>`; debug builds read the built-in plugins from `crates/mare-plugin/preset` on disk, and `/reload` picks up edits |
| `just release` | `cargo build --release` |
| `just test` | Run workspace tests and doctests (requires cargo-nextest 0.9.138 or newer) |
| `just check` | Lint and test |

`mare --version` prints the version, the commit, the Git commit date, and the target.

## First setup

Create a default configuration:

```sh
mare init
```

`mare init` writes `~/.config/mare/init.lua` (an existing file is kept, never overwritten) and `~/.config/mare/.luarc.json`, which points LuaLS at the type definitions in `~/.local/share/mare/runtime/types`. Each line of output says whether a file was `created`, `wrote` or `kept`. If you edited `.luarc.json` yourself, mare leaves it alone.

Then check the installation:

```sh
mare doctor
```

`mare doctor` prints one line per check, with a check mark, a warning mark or a cross, and exits with `1` when any check fails. On a fresh machine two failures are expected: missing directories (mare creates them on first use) and no credentials. Create the directories and tighten file permissions with:

```sh
mare doctor --fix
```

`--fix` creates missing directories, sets modes (`0700` for directories, `0600` for `auth.json` and `.jsonl` files directly inside the sessions directory), and attempts to repair a corrupt package lock file. The doctor run itself still does its normal start-up work (such as preparing the session index and runtime files). Add `--json` to get the checks as one JSON document. [Troubleshooting](troubleshooting.md) lists what each check covers.

Next, log in to a provider as described in the [Quickstart](quickstart.md).

## Shell completion

`mare completion <shell>` prints a completion script for `bash`, `zsh`, `fish`, `elvish` or `powershell`:

```sh
mare completion zsh > ~/.zfunc/_mare          # zsh: ~/.zfunc must be in fpath
mare completion bash > ~/.local/share/bash-completion/completions/mare
mare completion fish > ~/.config/fish/completions/mare.fish
```

Restart the shell, or reload its completion system, to pick it up.

## Updating

mare does not replace its own binary. To update:

- If you installed via a package manager (`brew upgrade mare`, `scoop update mare`, `yay -S mare-bin`, `nix profile upgrade mare`), use that package manager.
- Otherwise, re-run the quick install script (`install.sh` / `install.ps1`) or download the newer archive, verify it, and overwrite the old `mare` binary.

Release builds of the TUI check GitHub releases for a newer version every 24 hours by default (`update-check.interval_hours` adjusts the interval; debug builds skip automatic checks) and print one line if there is one. The request carries no identifier beyond `mare/<version>`. Turn the check off in `init.lua`:

```lua
mare.setup { update_check = false }
```

When the version changes, mare re-extracts its built-in plugins and types into `~/.local/share/mare/runtime` the next time it starts. Packages declared in `init.lua` are updated separately with `mare plugin update`.

## Files mare uses

| Directory | Default | Contents |
| --- | --- | --- |
| Config | `~/.config/mare` | `init.lua`, `.luarc.json`, `auth.json` (credentials), `mare-lock.json` (package lock), `plugins/` (your plugins) |
| State | `~/.local/state/mare` | `state.json` (runtime choices), `trust.json`, `sessions/`, `log/`, `crashes/` |
| Data | `~/.local/share/mare` | `runtime/` (built-in plugins and types), `packages/` |
| Cache | `~/.cache/mare` | `luac/` (compiled Lua bytecode cache) |

`XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` move the corresponding directory (mare appends `mare`). Setting `MARE_HOME` puts `config`, `state`, `data` and `cache` under one directory instead, which is useful for trying a clean setup without touching your real one. `mare runtime` prints the runtime directory.

A project can also have a `.mare/` directory with its own `init.lua`, plugins, skills and prompts. These only load after you trust the project; see the [Quickstart](quickstart.md#trust-a-project).

## Uninstall

1. Optional: remove stored credentials with `mare logout`, which revokes tokens where the provider supports it. Without a provider name it works when exactly one provider is logged in; otherwise name one, as in `mare logout openai`.
2. Uninstall via your package manager (for example `brew uninstall mare` or `scoop uninstall mare`), or delete the `mare` binary directly.
3. Delete the directories above if you want no trace left. Deleting the state directory removes all saved sessions.

```sh
rm -rf ~/.config/mare ~/.local/state/mare ~/.local/share/mare ~/.cache/mare
```

Environment variables and keys you wrote into `init.lua` are not touched by `mare logout`; remove them yourself.
