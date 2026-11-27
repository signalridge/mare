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
  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  Copy-Item -Path (Join-Path $TempDir "mare-$Tag-$Target\mare.exe") -Destination (Join-Path $InstallDir "mare.exe") -Force

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
