# Self-test for install.ps1 PATH manipulation and in-use executable replacement.
# Runs on PowerShell 5.1+ and PowerShell 7+ (pwsh) without downloading anything or touching User PATH.
$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$InstallPs1 = Join-Path $RepoRoot "install.ps1"
$DocsInstallPs1 = Join-Path $RepoRoot "docs-site/public/install.ps1"

$RootBytes = [System.IO.File]::ReadAllBytes($InstallPs1)
$DocsBytes = [System.IO.File]::ReadAllBytes($DocsInstallPs1)
if ($RootBytes.Length -ne $DocsBytes.Length -or ([System.BitConverter]::ToString($RootBytes) -ne [System.BitConverter]::ToString($DocsBytes))) {
  throw "docs-site/public/install.ps1 differs from install.ps1"
}

$Tokens = $null
$ParseErrors = $null
$Ast = [System.Management.Automation.Language.Parser]::ParseFile($InstallPs1, [ref]$Tokens, [ref]$ParseErrors)
if ($ParseErrors -and $ParseErrors.Count -gt 0) {
  throw "Parse errors in install.ps1: $($ParseErrors | ForEach-Object { $_.Message } | Out-String)"
}

foreach ($FnName in @("Add-MarePathEntry", "Install-MareExecutable")) {
  $FuncAst = $Ast.Find({
    param($Node)
    $Node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $Node.Name -eq $FnName
  }, $true)
  if (-not $FuncAst) {
    throw "install.ps1 does not define $FnName"
  }
  Invoke-Expression $FuncAst.Extent.Text
}

function Assert-Equal {
  param($Actual, $Expected, [string]$Label)
  if ($Actual -cne $Expected) {
    throw "FAIL [$Label]: expected '$Expected' got '$Actual'"
  }
}

$Dir = "C:\Users\me\AppData\Local\mare\bin"

# 1. Empty / null / semicolon-only PATH
Assert-Equal (Add-MarePathEntry -CurrentPath "" -Entry $Dir) $Dir "empty PATH"
Assert-Equal (Add-MarePathEntry -CurrentPath $null -Entry $Dir) $Dir "null PATH"
Assert-Equal (Add-MarePathEntry -CurrentPath ";;" -Entry $Dir) $Dir "semicolon-only PATH"

# 2. Single-entry PATH (regression for scalar string concatenation bug)
Assert-Equal (Add-MarePathEntry -CurrentPath "C:\Tools" -Entry $Dir) "C:\Tools;$Dir" "single-entry PATH"
Assert-Equal (Add-MarePathEntry -CurrentPath "C:\Tools;" -Entry $Dir) "C:\Tools;$Dir" "single-entry PATH with trailing semicolon"

# 3. Multi-entry PATH
Assert-Equal (Add-MarePathEntry -CurrentPath "C:\Tools;C:\Bin" -Entry $Dir) "C:\Tools;C:\Bin;$Dir" "multi-entry PATH"

# 4. Already present (exact and case-insensitive) -> returns $null (no change needed)
if ($null -ne (Add-MarePathEntry -CurrentPath $Dir -Entry $Dir)) {
  throw "FAIL [single exact duplicate]: expected null"
}
if ($null -ne (Add-MarePathEntry -CurrentPath "C:\Tools;c:\users\me\appdata\local\mare\bin" -Entry $Dir)) {
  throw "FAIL [case-insensitive duplicate]: expected null"
}

# 5. Executable replacement: fresh install, in-use upgrade, deferred .bak sweep, and failure preservation
$TmpRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("mare-ps1-test-" + [guid]::NewGuid().ToString("N"))
$TestInstallDir = Join-Path $TmpRoot "bin"
$SrcDir = Join-Path $TmpRoot "src"
New-Item -ItemType Directory -Force -Path $SrcDir | Out-Null

try {
  $SrcV1 = Join-Path $SrcDir "mare-v1.exe"
  $SrcV2 = Join-Path $SrcDir "mare-v2.exe"
  $SrcV3 = Join-Path $SrcDir "mare-v3.exe"
  $SrcV4 = Join-Path $SrcDir "mare-v4.exe"
  [System.IO.File]::WriteAllText($SrcV1, "mare-v1")
  [System.IO.File]::WriteAllText($SrcV2, "mare-v2")
  [System.IO.File]::WriteAllText($SrcV3, "mare-v3")
  [System.IO.File]::WriteAllText($SrcV4, "mare-v4")

  $TargetExe = Join-Path $TestInstallDir "mare.exe"

  # Fresh install
  Install-MareExecutable -SourcePath $SrcV1 -InstallDir $TestInstallDir
  Assert-Equal ([System.IO.File]::ReadAllText($TargetExe)) "mare-v1" "fresh install"

  # Stale .bak left by an earlier run is cleaned on the next install
  $StaleBak = Join-Path $TestInstallDir "mare.exe.99999.deadbeef.bak"
  [System.IO.File]::WriteAllText($StaleBak, "stale")
  Install-MareExecutable -SourcePath $SrcV2 -InstallDir $TestInstallDir
  Assert-Equal ([System.IO.File]::ReadAllText($TargetExe)) "mare-v2" "normal upgrade"
  if (Test-Path -LiteralPath $StaleBak) {
    throw "FAIL [stale bak cleanup]: expected stale .bak to be removed"
  }

  # In-use upgrade: hold target open with Read + Delete sharing (matches Windows running-exe image lock:
  # overwrites fail with sharing violation, while same-volume rename succeeds).
  $OpenShare = [System.IO.FileShare]::Read -bor [System.IO.FileShare]::Delete
  $HeldStream = [System.IO.File]::Open($TargetExe, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $OpenShare)
  try {
    if ($env:OS -eq "Windows_NT") {
      $CopyFailed = $false
      try {
        Copy-Item -LiteralPath $SrcV3 -Destination $TargetExe -Force
      } catch {
        $CopyFailed = $true
      }
      if (-not $CopyFailed) {
        throw "FAIL [in-use repro]: expected direct Copy-Item onto held exe to fail on Windows"
      }
    }

    Install-MareExecutable -SourcePath $SrcV3 -InstallDir $TestInstallDir

    # Open handle keeps reading v2; target path now serves v3 for the next launch.
    $Reader = New-Object System.IO.StreamReader($HeldStream)
    Assert-Equal ($Reader.ReadToEnd()) "mare-v2" "in-use open handle keeps old binary"
    Assert-Equal ([System.IO.File]::ReadAllText($TargetExe)) "mare-v3" "target path updated while old binary open"
  } finally {
    $HeldStream.Dispose()
  }

  # Subsequent install after running session exits sweeps any leftover .bak files.
  Install-MareExecutable -SourcePath $SrcV4 -InstallDir $TestInstallDir
  Assert-Equal ([System.IO.File]::ReadAllText($TargetExe)) "mare-v4" "post-release upgrade"
  $LeftoverBaks = @(Get-ChildItem -LiteralPath $TestInstallDir -Filter "mare.exe.*.bak" -File -ErrorAction SilentlyContinue)
  if ($LeftoverBaks.Count -ne 0) {
    throw "FAIL [deferred bak cleanup]: leftover backups found: $($LeftoverBaks.Name -join ', ')"
  }

  # Failure preservation: a missing source or non-renameable lock keeps the existing install working.
  $MissingFailed = $false
  try {
    Install-MareExecutable -SourcePath (Join-Path $SrcDir "does-not-exist.exe") -InstallDir $TestInstallDir
  } catch {
    $MissingFailed = $true
  }
  if (-not $MissingFailed) {
    throw "FAIL [missing source]: expected Install-MareExecutable to throw"
  }
  Assert-Equal ([System.IO.File]::ReadAllText($TargetExe)) "mare-v4" "existing install preserved on missing source"

  if ($env:OS -eq "Windows_NT") {
    $ExclusiveStream = [System.IO.File]::Open($TargetExe, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::None)
    $LockFailed = $false
    try {
      try {
        Install-MareExecutable -SourcePath $SrcV1 -InstallDir $TestInstallDir
      } catch {
        $LockFailed = $true
      }
    } finally {
      $ExclusiveStream.Dispose()
    }
    if (-not $LockFailed) {
      throw "FAIL [exclusive lock]: expected Install-MareExecutable to throw when rename is blocked"
    }
    Assert-Equal ([System.IO.File]::ReadAllText($TargetExe)) "mare-v4" "existing install preserved on failed replace"
  }

  $LeftoverNew = @(Get-ChildItem -LiteralPath $TestInstallDir -Filter "mare.exe.*.new" -File -ErrorAction SilentlyContinue)
  if ($LeftoverNew.Count -ne 0) {
    throw "FAIL [staged cleanup]: leftover .new files found: $($LeftoverNew.Name -join ', ')"
  }
} finally {
  Remove-Item -LiteralPath $TmpRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "PASS: install.ps1 self-test (PATH + in-use replacement passed)"
