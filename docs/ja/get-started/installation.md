# インストール

ご利用の OS に合った方法を選んで mare をインストールし、その後 `mare init` と `mare doctor` を実行してセットアップを確認してください。

## 対応プラットフォーム

リリース成果物は 6 つのターゲット向けにビルドされます (`.github/workflows/release.yaml`)。

| プラットフォーム | ターゲット | 配布形式 |
| --- | --- | --- |
| macOS、Apple シリコン | `aarch64-apple-darwin` | `.tar.gz`、Homebrew cask、Nix |
| macOS、Intel | `x86_64-apple-darwin` | `.tar.gz`、Homebrew cask、Nix |
| Linux、x86-64 (静的 musl) | `x86_64-unknown-linux-musl` | `.tar.gz`、`.deb`、`.rpm`、Homebrew cask、AUR、Nix、GHCR コンテナ |
| Linux、ARM64 (静的 musl) | `aarch64-unknown-linux-musl` | `.tar.gz`、`.deb`、`.rpm`、Homebrew cask、AUR、Nix、GHCR コンテナ |
| Windows、x86-64 | `x86_64-pc-windows-msvc` | `.zip`、Scoop マニフェスト |
| Windows、ARM64 | `aarch64-pc-windows-msvc` | `.zip`、Scoop マニフェスト |

mare は Windows を含むすべてのプラットフォームで、XDG 形式のディレクトリにファイルを置きます ([mare が使うファイル](#mare-が使うファイル) を参照)。

## macOS

### ワンラインインストーラー

最新の macOS アーカイブ (`aarch64-apple-darwin` または `x86_64-apple-darwin`) をダウンロードし、`checksums.txt` に基づく SHA-256 検証を行って導入します:

```sh
curl -fsSL https://signalridge.github.io/mare/install.sh | sh
```

既定では `~/.local/bin` にインストールされます。配置先は `MARE_INSTALL_DIR`、バージョン固定は `MARE_VERSION=vX.Y.Z` で指定できます。

### Homebrew cask

```sh
brew tap signalridge/tap
brew trust signalridge/tap
brew install --cask mare
```

### Nix (Flake と NUR)

```sh
# Flake (直接実行、またはプロファイルへ導入)
nix run github:signalridge/mare
nix profile install github:signalridge/mare#mare

# NUR ビルド済みバイナリ
nix profile install github:signalridge/nur-packages#mare
```

### 手動アーカイブ (`.tar.gz`)

[GitHub Releases](https://github.com/signalridge/mare/releases) から `mare-$VERSION-$TARGET.tar.gz` と `.sha256` ファイルを取得し、`shasum -a 256` で検証して配置します:

```sh
VERSION=vX.Y.Z
TARGET=aarch64-apple-darwin   # Intel Mac の場合は x86_64-apple-darwin
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

### ワンラインインストーラー

最新の Linux アーカイブ (`x86_64-unknown-linux-musl` または `aarch64-unknown-linux-musl`、静的リンク) をダウンロードし、`checksums.txt` に基づく SHA-256 検証を行って導入します:

```sh
curl -fsSL https://signalridge.github.io/mare/install.sh | sh
```

既定では `~/.local/bin` にインストールされます。配置先は `MARE_INSTALL_DIR`、バージョン固定は `MARE_VERSION=vX.Y.Z` で指定できます。

### Debian / Ubuntu (`.deb`)

[GitHub Releases](https://github.com/signalridge/mare/releases) からアーキテクチャ (`amd64` または `arm64`) に合う `.deb` パッケージをダウンロードし、`checksums.txt` を確認してからインストールします:

```sh
sudo apt install ./mare_<version>_linux_amd64.deb
# ARM64 の場合: sudo apt install ./mare_<version>_linux_arm64.deb
```

### Fedora / RHEL (`.rpm`)

[GitHub Releases](https://github.com/signalridge/mare/releases) からアーキテクチャ (`amd64` または `arm64`) に合う `.rpm` パッケージをダウンロードし、`checksums.txt` を確認してからインストールします:

```sh
sudo dnf install ./mare_<version>_linux_amd64.rpm
# ARM64 の場合: sudo dnf install ./mare_<version>_linux_arm64.rpm
```

### Arch Linux (AUR)

```sh
yay -S mare-bin
# または: paru -S mare-bin
```

### Homebrew (Linuxbrew)

```sh
brew tap signalridge/tap
brew trust signalridge/tap
brew install --cask mare
```

### Nix (Flake と NUR)

```sh
# Flake (直接実行、またはプロファイルへ導入)
nix run github:signalridge/mare
nix profile install github:signalridge/mare#mare

# NUR ビルド済みバイナリ
nix profile install github:signalridge/nur-packages#mare
```

### 手動アーカイブ (`.tar.gz`)

[GitHub Releases](https://github.com/signalridge/mare/releases) から `mare-$VERSION-$TARGET.tar.gz` と `.sha256` ファイルを取得し、`sha256sum` で検証して配置します:

```sh
VERSION=vX.Y.Z
TARGET=x86_64-unknown-linux-musl   # ARM64 の場合は aarch64-unknown-linux-musl
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

`~/.local/bin` (または選んだディレクトリ) が `PATH` に入っていることを確認してください。Linux で完全な名前空間ベースの OS サンドボックスを使うには、ディストリビューションのパッケージマネージャーで `bubblewrap` を導入してください (カーネルが対応していれば Landlock も利用されます)。

## Windows

### ワンラインインストーラー (PowerShell)

最新の Windows アーカイブ (`x86_64-pc-windows-msvc` または `aarch64-pc-windows-msvc`) をダウンロードし、`checksums.txt` で SHA-256 検証を行ったうえ `%LOCALAPPDATA%\mare\bin` (`$env:MARE_INSTALL_DIR` で変更可能) に配置してユーザー `PATH` へ自動追加します:

```powershell
irm https://signalridge.github.io/mare/install.ps1 | iex
```

バージョンを固定する場合は実行前に `$env:MARE_VERSION = "vX.Y.Z"` を設定してください。

### Scoop

```powershell
scoop bucket add signalridge https://github.com/signalridge/scoop-bucket
scoop install signalridge/mare
```

### 手動アーカイブ (`.zip`)

[GitHub Releases](https://github.com/signalridge/mare/releases) から `mare-$v-$t.zip` と `.sha256` ファイルをダウンロードし、SHA-256 を検証してから展開し、`mare-$v-$t\mare.exe` を `PATH` の通ったディレクトリへコピーしてください:

```powershell
$ErrorActionPreference = "Stop"
$v = "vX.Y.Z"; $t = "x86_64-pc-windows-msvc"   # ARM64 の場合は aarch64-pc-windows-msvc
$archive = "mare-$v-$t.zip"
$expected = ([string](Get-Content "$archive.sha256" -Raw)).Trim().Split()[0].ToLower()
$actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
if ($expected -notmatch '^[0-9a-f]{64}$' -or $actual -ne $expected) {
  throw "Checksum mismatch: expected $expected, got $actual"
}
Expand-Archive $archive -DestinationPath .
```

Windows には、オプションの拡張サンドボックス層もあります。`mare sandbox status` でインストール済みかどうかを確認してセルフテストを実行でき、`mare sandbox setup` でインストールします。昇格は 1 回だけ求められます。

## コンテナ (Docker / GHCR)

Ubuntu 24.04 ベースで `git` と `bubblewrap` を同梱したマルチアーキテクチャ (`linux/amd64`、`linux/arm64`) イメージを GitHub Container Registry へ公開し、Cosign で署名しています:

```sh
docker pull ghcr.io/signalridge/mare:latest
docker run --rm -it -v "$PWD:/workspace" -w /workspace ghcr.io/signalridge/mare:latest
```

## リリース成果物と検証

リリースは GitHub の [`signalridge/mare`](https://github.com/signalridge/mare/releases) で公開されます。公開済みの各 GitHub Release (`vX.Y.Z`) には次のファイルが添付されます:

- 各ターゲット向けの `.tar.gz` (macOS・Linux) および `.zip` (Windows) アーカイブと `.sha256` ファイル
- Linux 向けの `.deb` / `.rpm` パッケージ (`amd64` / `arm64`)
- `checksums.txt`、Cosign 署名バンドル (`checksums.txt.sigstore.json`)、SPDX SBOM (`*.sbom.spdx.json`)、および SLSA provenance (`multiple.intoto.jsonl`)

各アーカイブには、アーカイブと同じ名前 (拡張子なし) のディレクトリが 1 つ入っています。その中に `mare` バイナリ (Windows では `mare.exe`)、Lua 型定義の `types/` ディレクトリ、`LICENSE`、`THIRD_PARTY_NOTICES.md`、`licenses/cordis-LICENSE`、`licenses/RUST-THIRD-PARTY-NOTICES.txt`、および `README.md` が含まれています。

## ソースからビルドする

Rust ツールチェーンのバージョン 1.100.0 以上 (`Cargo.toml` の `rust-version`。プロジェクトは edition 2024 を使用) と C コンパイラーが必要です。Lua 5.5 ランタイムは同梱のソースからコンパイルされるためです。`rust-toolchain.toml` が CI とリリースビルドで使う正確なバージョンを固定しているため、チェックアウト内では rustup がそれを自動でインストールして選択します。

Cargo から直接インストール：

```sh
cargo install --git https://github.com/signalridge/mare --locked --bin mare mare-cli
mare --version
```

またはリポジトリをクローンしてローカルでビルド：

```sh
git clone https://github.com/signalridge/mare
cd mare
cargo build --release --locked -p mare-cli
./target/release/mare --version
```

リリースワークフローもまったく同じ方法でビルドしており、`--locked` により依存関係のバージョンは `Cargo.lock` で決まります。`target/release/mare` を `PATH` の通った場所にコピーしてください。リリースプロファイルでは LTO が有効なので、リリースビルドはデバッグビルドよりかなり時間がかかります。

リポジトリには `justfile` もあります。

| コマンド | 内容 |
| --- | --- |
| `just run <args>` | `cargo run -- <args>`。デバッグビルドは組み込みプラグインを `crates/mare-plugin/preset` からディスク上で読み込み、`/reload` で編集内容が反映されます |
| `just release` | `cargo build --release` |
| `just test` | ワークスペースのテストと doctest を実行します (cargo-nextest 0.9.138 以降が必要) |
| `just check` | リントとテスト |

`mare --version` は、バージョン、コミット、Git コミット日、ターゲットを表示します。

## 初期セットアップ

既定の設定を作成します。

```sh
mare init
```

`mare init` は `~/.config/mare/init.lua` (既存のファイルは上書きせず保持されます) と `~/.config/mare/.luarc.json` を書き出します。後者は LuaLS を `~/.local/share/mare/runtime/types` の型定義に向けるものです。出力の各行は、ファイルが `created`、`wrote`、`kept` のどれになったかを示します。`.luarc.json` を自分で編集した場合、mare はそのファイルに触れません。

続けて、インストールを確認します。

```sh
mare doctor
```

`mare doctor` はチェックごとに 1 行を、チェックマーク、警告マーク、バツ印のいずれかとともに表示し、失敗したチェックが 1 つでもあれば終了コード `1` で終わります。まっさらなマシンでは、ディレクトリがない (mare は初回使用時に作成します) ことと、認証情報がないことの 2 つの失敗が想定されます。ディレクトリを作成してファイルの権限を絞るには、次を実行します。

```sh
mare doctor --fix
```

`--fix` は、足りないディレクトリの作成、モードの設定 (ディレクトリは `0700`、`auth.json` とセッションディレクトリ直下の `.jsonl` ファイルは `0600`)、さらに壊れたパッケージロックファイルの修復を試みます。doctor の実行自体は通常の起動時処理 (セッションインデックスやランタイムファイルの準備など) も行います。`--json` を付けると、チェック結果を 1 つの JSON ドキュメントとして取得できます。各チェックの対象は [トラブルシューティング](troubleshooting.md) にまとめています。

次は、[クイックスタート](quickstart.md) に従ってプロバイダーにログインします。

## シェル補完

`mare completion <shell>` は、`bash`、`zsh`、`fish`、`elvish`、`powershell` の補完スクリプトを出力します。

```sh
mare completion zsh > ~/.zfunc/_mare          # zsh: ~/.zfunc が fpath に含まれている必要があります
mare completion bash > ~/.local/share/bash-completion/completions/mare
mare completion fish > ~/.config/fish/completions/mare.fish
```

反映するには、シェルを再起動するか、補完システムを再読み込みしてください。

## アップデート

mare は自分自身のバイナリを置き換えません。アップデート方法:

- パッケージマネージャーから導入した場合は、それぞれの更新コマンド (`brew upgrade --cask mare`、`scoop update mare`、`yay -Syu mare-bin`、`nix profile upgrade mare` など) を使います。
- それ以外の場合は、ワンラインインストーラー (`install.sh` / `install.ps1`) を再実行するか、新しいアーカイブをダウンロード・検証して上記のとおり古い `mare` を上書きしてください。

リリースビルドの TUI は、既定で 24 時間ごとに GitHub のリリースを確認し (`update-check.interval_hours` で間隔を変更可能、デバッグビルドは自動確認をスキップ)、新しいバージョンがあれば 1 行表示します。リクエストには `mare/<version>` 以外の識別情報は含まれません。確認を止めるには `init.lua` に次を書きます。

```lua
mare.setup { update_check = false }
```

バージョンが変わると、mare は次の起動時に組み込みプラグインと型定義を `~/.local/share/mare/runtime` へ展開し直します。`init.lua` で宣言したパッケージは、`mare plugin update` で別に更新します。

## mare が使うファイル

| ディレクトリ | 既定値 | 内容 |
| --- | --- | --- |
| 設定 | `~/.config/mare` | `init.lua`、`.luarc.json`、`auth.json` (認証情報)、`mare-lock.json` (パッケージのロック)、`plugins/` (あなたのプラグイン) |
| 状態 | `~/.local/state/mare` | `state.json` (実行時の選択)、`trust.json`、`sessions/`、`log/`、`crashes/` |
| データ | `~/.local/share/mare` | `runtime/` (組み込みプラグインと型定義)、`packages/` |
| キャッシュ | `~/.cache/mare` | `luac/` (コンパイル済み Lua バイトコードのキャッシュ) |

`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、`XDG_DATA_HOME`、`XDG_CACHE_HOME` で、対応するディレクトリを移動できます (mare が末尾に `mare` を付けます)。`MARE_HOME` を設定すると、`config`、`state`、`data`、`cache` を 1 つのディレクトリの下にまとめます。普段の環境に触れずにまっさらな状態を試すのに便利です。`mare runtime` はランタイムディレクトリを表示します。

プロジェクトには、独自の `init.lua`、プラグイン、スキル、プロンプトを置く `.mare/` ディレクトリも持てます。これらはプロジェクトを信頼した後にだけ読み込まれます。[クイックスタート](quickstart.md#プロジェクトを信頼する) を参照してください。

## アンインストール

1. 任意: `mare logout` で保存済みの認証情報を削除します。プロバイダーが対応していればトークンも失効させます。プロバイダー名を省略した場合、ログイン中のプロバイダーがちょうど 1 つのときだけ動作します。それ以外は `mare logout openai` のように名前を指定してください。
2. パッケージマネージャーから導入した場合はアンインストールコマンド (`brew uninstall --cask mare` や `scoop uninstall mare` など) を実行し、手動導入の場合は `mare` バイナリを削除します。
3. 痕跡を残したくなければ、上記のディレクトリを削除します。状態ディレクトリを削除すると、保存済みのセッションはすべて消えます。

```sh
rm -rf ~/.config/mare ~/.local/state/mare ~/.local/share/mare ~/.cache/mare
```

`init.lua` に書いた環境変数やキーは `mare logout` では削除されません。自分で削除してください。
