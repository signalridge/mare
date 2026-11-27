# 安装

请按你使用的操作系统选择对应安装方式，安装完成后运行 `mare init` 和 `mare doctor` 检查环境。

## 支持的平台

发布产物针对六个目标构建（`.github/workflows/release.yaml`）：

| 平台 | 目标 | 产物类型 |
| --- | --- | --- |
| macOS，Apple silicon | `aarch64-apple-darwin` | `.tar.gz`、Homebrew cask、Nix |
| macOS，Intel | `x86_64-apple-darwin` | `.tar.gz`、Homebrew cask、Nix |
| Linux，x86-64（glibc 2.39+） | `x86_64-unknown-linux-gnu` | `.tar.gz`、`.deb`、`.rpm`、Homebrew cask、AUR、Nix、GHCR 容器 |
| Linux，ARM64（glibc 2.39+） | `aarch64-unknown-linux-gnu` | `.tar.gz`、`.deb`、`.rpm`、Homebrew cask、AUR、Nix、GHCR 容器 |
| Windows，x86-64 | `x86_64-pc-windows-msvc` | `.zip`、Scoop 清单 |
| Windows，ARM64 | `aarch64-pc-windows-msvc` | `.zip`、Scoop 清单 |

mare 在所有平台上（包括 Windows）都把文件放在 XDG 风格的目录中（见[mare 使用的文件](#mare-使用的文件)）。

## macOS

### 一键安装脚本

自动下载最新的 macOS 压缩包（`aarch64-apple-darwin` 或 `x86_64-apple-darwin`）并对照 `checksums.txt` 校验 SHA-256：

```sh
curl -fsSL https://signalridge.github.io/mare/install.sh | sh
```

默认安装到 `~/.local/bin`，可通过 `MARE_INSTALL_DIR` 指定安装目录，或通过 `MARE_VERSION=vX.Y.Z` 指定版本。

### Homebrew cask

```sh
brew tap signalridge/tap
brew trust signalridge/tap
brew install --cask mare
```

### Nix（Flake 与 NUR）

```sh
# Flake（直接运行或安装到 profile）
nix run github:signalridge/mare
nix profile install github:signalridge/mare#mare

# NUR 预编译包
nix profile install github:signalridge/nur-packages#mare
```

### 手动下载压缩包（`.tar.gz`）

从 [GitHub Releases](https://github.com/signalridge/mare/releases) 下载 `mare-$VERSION-$TARGET.tar.gz` 及其 `.sha256` 文件，使用 `shasum -a 256` 校验后解压安装：

```sh
VERSION=vX.Y.Z
TARGET=aarch64-apple-darwin   # Intel Mac 请使用 x86_64-apple-darwin
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

## Linux

### 一键安装脚本

自动下载最新的 Linux 压缩包（`x86_64-unknown-linux-gnu` 或 `aarch64-unknown-linux-gnu`，要求 glibc 2.39+）并对照 `checksums.txt` 校验 SHA-256：

```sh
curl -fsSL https://signalridge.github.io/mare/install.sh | sh
```

默认安装到 `~/.local/bin`，可通过 `MARE_INSTALL_DIR` 指定安装目录，或通过 `MARE_VERSION=vX.Y.Z` 指定版本。

### Debian / Ubuntu（`.deb`）

从 [GitHub Releases](https://github.com/signalridge/mare/releases) 下载对应架构（`amd64` 或 `arm64`）的 `.deb` 包，对照 `checksums.txt` 校验后安装：

```sh
sudo apt install ./mare_<version>_linux_amd64.deb
# ARM64 请使用：sudo apt install ./mare_<version>_linux_arm64.deb
```

### Fedora / RHEL（`.rpm`）

从 [GitHub Releases](https://github.com/signalridge/mare/releases) 下载对应架构（`amd64` 或 `arm64`）的 `.rpm` 包，对照 `checksums.txt` 校验后安装：

```sh
sudo dnf install ./mare_<version>_linux_amd64.rpm
# ARM64 请使用：sudo dnf install ./mare_<version>_linux_arm64.rpm
```

### Arch Linux（AUR）

```sh
yay -S mare-bin
# 或：paru -S mare-bin
```

### Homebrew（Linuxbrew）

```sh
brew tap signalridge/tap
brew trust signalridge/tap
brew install --cask mare
```

### Nix（Flake 与 NUR）

```sh
# Flake（直接运行或安装到 profile）
nix run github:signalridge/mare
nix profile install github:signalridge/mare#mare

# NUR 预编译包
nix profile install github:signalridge/nur-packages#mare
```

### 手动下载压缩包（`.tar.gz`）

从 [GitHub Releases](https://github.com/signalridge/mare/releases) 下载 `mare-$VERSION-$TARGET.tar.gz` 及其 `.sha256` 文件，使用 `sha256sum` 校验后解压安装：

```sh
VERSION=vX.Y.Z
TARGET=x86_64-unknown-linux-gnu   # ARM64 请使用 aarch64-unknown-linux-gnu
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

确保 `~/.local/bin`（或你选择的目录）在 `PATH` 中。Linux 下如需完整的命名空间操作系统沙箱，建议通过发行版包管理器安装 `bubblewrap`（内核支持时也可启用 Landlock）。

## Windows

### 一键安装脚本（PowerShell）

自动下载最新 Windows 压缩包（`x86_64-pc-windows-msvc` 或 `aarch64-pc-windows-msvc`），对照 `checksums.txt` 校验 SHA-256，安装 `mare.exe` 到 `%LOCALAPPDATA%\mare\bin`（可通过 `$env:MARE_INSTALL_DIR` 覆盖），并自动写入用户 `PATH`：

```powershell
irm https://signalridge.github.io/mare/install.ps1 | iex
```

运行前设置 `$env:MARE_VERSION = "vX.Y.Z"` 可安装指定版本。

### Scoop

```powershell
scoop bucket add signalridge https://github.com/signalridge/scoop-bucket
scoop install signalridge/mare
```

### 手动下载压缩包（`.zip`）

从 [GitHub Releases](https://github.com/signalridge/mare/releases) 下载 `mare-$v-$t.zip` 及其 `.sha256` 文件，校验 SHA-256 后解压并把 `mare-$v-$t\mare.exe` 复制到 `PATH` 中的目录：

```powershell
$ErrorActionPreference = "Stop"
$v = "vX.Y.Z"; $t = "x86_64-pc-windows-msvc"   # ARM64 请使用 aarch64-pc-windows-msvc
$archive = "mare-$v-$t.zip"
$expected = ([string](Get-Content "$archive.sha256" -Raw)).Trim().Split()[0].ToLower()
$actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
if ($expected -notmatch '^[0-9a-f]{64}$' -or $actual -ne $expected) {
  throw "Checksum mismatch: expected $expected, got $actual"
}
Expand-Archive $archive -DestinationPath .
```

Windows 还有一个可选的增强沙箱层级。`mare sandbox status` 会显示它是否已安装并运行自检；`mare sandbox setup` 会安装它，并请求一次提权。

## 容器镜像（Docker / GHCR）

基于 Ubuntu 24.04 构建（内置 `git` 与 `bubblewrap`）、支持 `linux/amd64` 与 `linux/arm64` 多架构的容器镜像已发布至 GitHub Container Registry 并使用 Cosign 签名：

```sh
docker pull ghcr.io/signalridge/mare:latest
docker run --rm -it -v "$PWD:/workspace" -w /workspace ghcr.io/signalridge/mare:latest
```

## Release 产物与校验说明

发布包托管在 GitHub 的 [`signalridge/mare`](https://github.com/signalridge/mare/releases) 仓库。每个公开发布的 GitHub Release（`vX.Y.Z`）包含：

- 各目标的 `.tar.gz`（macOS / Linux）与 `.zip`（Windows）压缩包及对应 `.sha256` 文件；
- Linux 的 `.deb` 与 `.rpm` 安装包（`amd64` 与 `arm64`）；
- `checksums.txt`、Cosign 签名包（`checksums.txt.sigstore.json`）、SPDX SBOM（`*.sbom.spdx.json`）以及 SLSA 构建溯源证明（`multiple.intoto.jsonl`）。

每个压缩包里有一个与压缩包同名（不含扩展名）的目录，其中包含 `mare` 二进制文件（Windows 上为 `mare.exe`）、带有 Lua 类型定义的 `types/` 目录、`LICENSE`、`THIRD_PARTY_NOTICES.md`、`licenses/cordis-LICENSE`、`licenses/RUST-THIRD-PARTY-NOTICES.txt` 以及 `README.md`。

## 从源码构建

你需要 Rust 工具链 1.100.0 或更高版本（见 `Cargo.toml` 中的 `rust-version`；项目使用 edition 2024）和一个 C 编译器，因为 Lua 5.5 运行时是由内置的源码编译而来。`rust-toolchain.toml` 钉住了 CI 和发布构建所用的确切版本，在仓库目录内 rustup 会自动安装并选用它。

直接用 Cargo 安装：

```sh
cargo install --git https://github.com/signalridge/mare --locked --bin mare mare-cli
mare --version
```

或克隆仓库后本地构建：

```sh
git clone https://github.com/signalridge/mare
cd mare
cargo build --release --locked -p mare-cli
./target/release/mare --version
```

发布工作流正是这样构建的，并使用 `--locked`，由 `Cargo.lock` 决定依赖版本。把 `target/release/mare` 复制到 `PATH` 中的某处。release 配置启用了 LTO，所以 release 构建比 debug 构建慢得多。

仓库里还有一个 `justfile`：

| 命令 | 作用 |
| --- | --- |
| `just run <args>` | `cargo run -- <args>`；debug 构建会从磁盘上的 `crates/mare-plugin/preset` 读取内置插件，`/reload` 会应用你的修改 |
| `just release` | `cargo build --release` |
| `just test` | 运行工作区测试和 doctest（需要 cargo-nextest 0.9.138 或更新版本） |
| `just check` | 检查并测试 |

`mare --version` 会打印版本、提交、Git 提交日期和目标平台。

## 首次设置

创建默认配置：

```sh
mare init
```

`mare init` 会写入 `~/.config/mare/init.lua`（已有文件会保留，绝不覆盖）和 `~/.config/mare/.luarc.json`，后者让 LuaLS 指向 `~/.local/share/mare/runtime/types` 中的类型定义。输出的每一行都会说明文件是 `created`、`wrote` 还是 `kept`。如果你自己改过 `.luarc.json`，mare 不会动它。

然后检查安装：

```sh
mare doctor
```

`mare doctor` 每项检查输出一行，带有对勾、警告标记或叉号；只要有检查失败，就以 `1` 退出。在全新的机器上，有两项失败是正常的：目录缺失（mare 会在首次使用时创建）和没有凭据。用下面的命令创建目录并收紧文件权限：

```sh
mare doctor --fix
```

`--fix` 会创建缺失的目录、设置权限模式（目录为 `0700`，`auth.json` 和会话目录下直接存放的 `.jsonl` 文件为 `0600`），并尝试修复损坏的包锁定文件。doctor 运行本身仍会执行正常的启动准备（例如准备会话索引和运行时文件）。加上 `--json` 可以把检查结果作为一个 JSON 文档输出。每项检查涵盖的内容见[故障排查](troubleshooting.md)。

接下来，按[快速上手](quickstart.md)的说明登录 provider。

## Shell 补全

`mare completion <shell>` 会打印 `bash`、`zsh`、`fish`、`elvish` 或 `powershell` 的补全脚本：

```sh
mare completion zsh > ~/.zfunc/_mare          # zsh：~/.zfunc 必须在 fpath 中
mare completion bash > ~/.local/share/bash-completion/completions/mare
mare completion fish > ~/.config/fish/completions/mare.fish
```

重启 shell，或重新加载其补全系统，即可生效。

## 更新

mare 不会替换自己的二进制文件。要更新时：

- 如果你通过包管理器安装，请直接使用对应包管理器更新（例如 `brew upgrade --cask mare`、`scoop update mare`、`yay -Syu mare-bin`、`nix profile upgrade mare`）。
- 否则，重新运行一键安装脚本（`install.sh` / `install.ps1`），或按上文方式下载新压缩包校验后覆盖旧的 `mare` 二进制文件。

发布构建的 TUI 默认每 24 小时检查一次 GitHub 发布版本（`update-check.interval_hours` 可调整间隔；调试构建跳过自动检查），如有新版本就打印一行提示。请求中除 `mare/<version>` 外不带任何标识。可以在 `init.lua` 中关闭检查：

```lua
mare.setup { update_check = false }
```

版本变化后，mare 会在下次启动时把内置插件和类型重新解压到 `~/.local/share/mare/runtime`。在 `init.lua` 中声明的包用 `mare plugin update` 单独更新。

## mare 使用的文件

| 目录 | 默认位置 | 内容 |
| --- | --- | --- |
| 配置 | `~/.config/mare` | `init.lua`、`.luarc.json`、`auth.json`（凭据）、`mare-lock.json`（包锁定文件）、`plugins/`（你的插件） |
| 状态 | `~/.local/state/mare` | `state.json`（运行时选择）、`trust.json`、`sessions/`、`log/`、`crashes/` |
| 数据 | `~/.local/share/mare` | `runtime/`（内置插件和类型）、`packages/` |
| 缓存 | `~/.cache/mare` | `luac/`（编译后的 Lua 字节码缓存） |

`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、`XDG_DATA_HOME` 和 `XDG_CACHE_HOME` 可以移动对应的目录（mare 会在后面追加 `mare`）。设置 `MARE_HOME` 则会把 `config`、`state`、`data` 和 `cache` 都放到同一个目录下，适合在不碰真实环境的情况下试用干净的配置。`mare runtime` 会打印运行时目录。

项目也可以有自己的 `.mare/` 目录，里面放 `init.lua`、插件、技能和提示词。这些内容只有在你信任该项目后才会加载，见[快速上手](quickstart.md#信任项目)。

## 卸载

1. 可选：用 `mare logout` 删除已保存的凭据；provider 支持时会同时吊销令牌。不带 provider 名称时，只有恰好登录了一个 provider 才能执行；否则请指定名称，例如 `mare logout openai`。
2. 通过包管理器卸载（例如 `brew uninstall --cask mare` 或 `scoop uninstall mare`），或直接删除 `mare` 二进制文件。
3. 如果想不留痕迹，再删除上面列出的目录。删除状态目录会清除所有已保存的会话。

```sh
rm -rf ~/.config/mare ~/.local/state/mare ~/.local/share/mare ~/.cache/mare
```

`mare logout` 不会处理环境变量和你写进 `init.lua` 的密钥，请自行删除。
