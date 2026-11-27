# Self-test for install.ps1 PATH manipulation.
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

$FuncAst = $Ast.Find({
  param($Node)
  $Node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $Node.Name -eq "Add-MarePathEntry"
}, $true)
if (-not $FuncAst) {
  throw "install.ps1 does not define Add-MarePathEntry"
}

Invoke-Expression $FuncAst.Extent.Text

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

Write-Host "PASS: install.ps1 PATH self-test (all cases passed)"
