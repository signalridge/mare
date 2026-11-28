$ErrorActionPreference = "Stop"

[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Add-MarePathEntry {
  param(
    [AllowNull()]
    [AllowEmptyString()]
    [string]$CurrentPath,
    [Parameter(Mandatory = $true)]
    [string]$Entry
  )
  $PathEntries = @()
  if ($CurrentPath) {
    $PathEntries = @($CurrentPath -split ';' | Where-Object { $_ })
  }
  if ($PathEntries -notcontains $Entry) {
    return ((@($PathEntries) + @($Entry)) -join ';')
  }
  return $null
}

function Install-MareExecutable {
  param(
    [Parameter(Mandatory = $true)]
    [string]$SourcePath,
    [Parameter(Mandatory = $true)]
    [string]$InstallDir,
    [string]$FileName = "mare.exe"
  )
  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  $ResolvedInstallDir = (Resolve-Path -LiteralPath $InstallDir).ProviderPath

  # Best-effort sweep of backups left behind when an earlier upgrade replaced a running binary.
  foreach ($Stale in @(Get-ChildItem -LiteralPath $ResolvedInstallDir -Filter "$FileName.*.bak" -File -ErrorAction SilentlyContinue)) {
    Remove-Item -LiteralPath $Stale.FullName -Force -ErrorAction SilentlyContinue
  }

  $TargetPath = Join-Path $ResolvedInstallDir $FileName
  $Nonce = "$PID." + [guid]::NewGuid().ToString("N").Substring(0, 8)
  $StagedPath = Join-Path $ResolvedInstallDir "$FileName.$Nonce.new"
  $BackupPath = Join-Path $ResolvedInstallDir "$FileName.$Nonce.bak"

  # Stage inside the install directory so all renames stay on the same volume.
  Copy-Item -LiteralPath $SourcePath -Destination $StagedPath -Force

  $MovedOld = $false
  try {
    if (Test-Path -LiteralPath $TargetPath) {
      # Windows refuses overwriting a running executable, but allows renaming it on the same volume.
      [System.IO.File]::Move($TargetPath, $BackupPath)
      $MovedOld = $true
    }
    [System.IO.File]::Move($StagedPath, $TargetPath)
  } catch {
    Remove-Item -LiteralPath $StagedPath -Force -ErrorAction SilentlyContinue
    if ($MovedOld -and (Test-Path -LiteralPath $BackupPath) -and -not (Test-Path -LiteralPath $TargetPath)) {
      try {
        [System.IO.File]::Move($BackupPath, $TargetPath)
      } catch {
      }
    }
    throw
  }

  # Remove backup immediately when unlocked; otherwise a later install sweeps it after running sessions exit.
  if ($MovedOld) {
    Remove-Item -LiteralPath $BackupPath -Force -ErrorAction SilentlyContinue
  }
}

$Repo = "signalridge/mare"
$InstallDir = if ($env:MARE_INSTALL_DIR) { $env:MARE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "mare\bin" }

$Arch = $null
try {
  $RuntimeInfoType = [Type]::GetType("System.Runtime.InteropServices.RuntimeInformation, mscorlib")
  if (-not $RuntimeInfoType) {
    $RuntimeInfoType = [Type]::GetType("System.Runtime.InteropServices.RuntimeInformation, System.Runtime.InteropServices")
  }
  if ($RuntimeInfoType) {
    $Arch = [string]$RuntimeInfoType::OSArchitecture
  }
} catch {
  $Arch = $null
}
if (-not $Arch) {
  $RawArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  $Arch = switch -Regex ($RawArch) {
    "ARM64" { "Arm64" }
    "AMD64|x86_64" { "X64" }
    default { $RawArch }
  }
}

$Target = switch ($Arch) {
  "Arm64" { "aarch64-pc-windows-msvc" }
  "X64"   { "x86_64-pc-windows-msvc" }
  default { throw "Unsupported Windows architecture: $Arch" }
}

if ($env:MARE_VERSION) {
  $Tag = if ($env:MARE_VERSION.StartsWith("v")) { $env:MARE_VERSION } else { "v$($env:MARE_VERSION)" }
} else {
  $Req = [System.Net.HttpWebRequest]::Create("https://github.com/$Repo/releases/latest")
  $Req.AllowAutoRedirect = $true
  $Resp = $Req.GetResponse()
  try {
    $Tag = $Resp.ResponseUri.Segments[-1].TrimEnd("/")
  } finally {
    $Resp.Close()
  }
  if (-not $Tag -or $Tag -eq "latest") {
    throw "Failed to resolve latest release tag for $Repo"
  }
}

$Archive = "mare-$Tag-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mare-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

try {
  Write-Host "Installing mare $Tag ($Target)..."
  $ArchivePath = Join-Path $TempDir $Archive
  $ChecksumsPath = Join-Path $TempDir "checksums.txt"

  Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Archive" -OutFile $ArchivePath
  Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/checksums.txt" -OutFile $ChecksumsPath

  $Expected = $null
  foreach ($Line in Get-Content -Path $ChecksumsPath) {
    $Parts = $Line.Trim() -split '\s+', 2
    if ($Parts.Length -eq 2 -and $Parts[1].TrimStart("*") -eq $Archive) {
      $Expected = $Parts[0].ToLowerInvariant()
      break
    }
  }
  if (-not $Expected) {
    throw "Missing checksum for $Archive in checksums.txt"
  }

  $Actual = (Get-FileHash -Path $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($Actual -ne $Expected) {
    throw "Checksum mismatch for ${Archive}: expected=$Expected actual=$Actual"
  }

  Expand-Archive -Path $ArchivePath -DestinationPath $TempDir -Force
  Install-MareExecutable -SourcePath (Join-Path $TempDir "mare-$Tag-$Target\mare.exe") -InstallDir $InstallDir

  $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
  $NewPath = Add-MarePathEntry -CurrentPath $UserPath -Entry $InstallDir
  if ($null -ne $NewPath) {
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    if (@($env:Path -split ';' | Where-Object { $_ }) -notcontains $InstallDir) {
      $env:Path = if ($env:Path) { "$InstallDir;$env:Path" } else { $InstallDir }
    }
    Write-Host "Added $InstallDir to your User PATH (restart new terminals if needed)."
  }

  & (Join-Path $InstallDir "mare.exe") --version
} finally {
  Remove-Item -LiteralPath $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}
