# The hosted image name does not prove the runner's window station. Query the
# process executing the tests; child processes inherit its station.
$ErrorActionPreference = 'Stop'
# Nonzero status is expected on the unsupported-station path; inspect it explicitly.
$PSNativeCommandUseErrorActionPreference = $false
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
public static class MareCiStation {
    [DllImport("user32.dll", SetLastError=true)]
    static extern IntPtr GetProcessWindowStation();
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool GetUserObjectInformationW(IntPtr handle, int index, StringBuilder name, uint bytes, out uint needed);
    public static string Name() {
        var station = GetProcessWindowStation();
        if (station == IntPtr.Zero) throw new Win32Exception();
        var name = new StringBuilder(1024);
        uint needed;
        if (!GetUserObjectInformationW(station, 2, name, (uint)(name.Capacity * 2), out needed))
            throw new Win32Exception();
        return name.ToString();
    }
}
'@
$station = [MareCiStation]::Name()
Write-Host "Actual CI process window station: $station"
# Remove inherited selectors so the observed station alone selects the test path.
Remove-Item Env:MARE_REQUIRE_ENHANCED_SANDBOX -ErrorAction SilentlyContinue
Remove-Item Env:MARE_EXPECT_ENHANCED_UNAVAILABLE -ErrorAction SilentlyContinue
$interactive = $station -ieq 'WinSta0'
if ($interactive) {
    $env:MARE_REQUIRE_ENHANCED_SANDBOX = '1'
} else {
    $env:MARE_EXPECT_ENHANCED_UNAVAILABLE = $station
}
$prevMareHome = $env:MARE_HOME
$tempMareHome = Join-Path ([System.IO.Path]::GetTempPath()) ("mare-sandbox-ci-" + [guid]::NewGuid().ToString("N"))
$null = New-Item -ItemType Directory -Path $tempMareHome -Force
$env:MARE_HOME = $tempMareHome
cargo nextest list -p mare-cli --profile ci -E 'test(/suite::win_sandbox::enhanced_/)' | Out-Null
if ($LASTEXITCODE -ne 0) { throw "mare build failed ($LASTEXITCODE)" }
try {
    ./target/debug/mare.exe sandbox setup
    if ($LASTEXITCODE -ne 0) { throw "enhanced sandbox setup failed ($LASTEXITCODE)" }
    $status = ./target/debug/mare.exe sandbox status --require-full 2>&1 | Out-String
    $statusExit = $LASTEXITCODE
    Write-Host $status
    if ($statusExit -eq 0) { throw 'Partial per-call network isolation incorrectly satisfied --require-full' }
    if ($interactive -and $status -notmatch 'self-test: passed') {
        throw "enhanced sandbox self-test failed in WinSta0: $status"
    }
    if (!$interactive -and $status -notmatch 'enhanced sandbox requires WinSta0; parent window station is ') {
        throw "unsupported station $station lacked the explicit unavailable diagnosis: $status"
    }
    # Required selectors make missing setup fail on either path. The test checks
    # the exact unavailable diagnostic and no payload execution outside WinSta0.
    $env:MARE_TEST_EXPECT_BACKEND = 'windows-acl'
    cargo nextest run -p mare-cli --profile ci --no-capture -E 'test(/suite::win_sandbox::enhanced_/)'
    if ($LASTEXITCODE -ne 0) { throw "enhanced sandbox tests failed ($LASTEXITCODE)" }
    if ($interactive) {
        $result = "Enhanced sandbox pipe/ConPTY confinement tested in actual station: $station."
    } else {
        $result = "Enhanced sandbox UNAVAILABLE in actual station: $station. Verified explicit refusal for pipe/ConPTY; enhanced confinement was NOT RUN. Run it separately in the user's PVE interactive WinSta0 session."
    }
    Write-Host $result
    if ($env:GITHUB_STEP_SUMMARY) { Add-Content -Path $env:GITHUB_STEP_SUMMARY -Value $result }
} finally {
    ./target/debug/mare.exe sandbox setup --remove
    $removeExit = $LASTEXITCODE
    if ($null -eq $prevMareHome) {
        Remove-Item Env:MARE_HOME -ErrorAction SilentlyContinue
    } else {
        $env:MARE_HOME = $prevMareHome
    }
    Remove-Item -Path $tempMareHome -Recurse -Force -ErrorAction SilentlyContinue
    if ($removeExit -ne 0) { throw "enhanced sandbox cleanup failed ($removeExit)" }
}
