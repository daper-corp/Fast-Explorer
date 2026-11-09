# Builds the release binaries and installer -> <target>\installer\FastExplorer-Setup-<version>.exe
# Honors CARGO_TARGET_DIR (e.g. to build while target\release\fex.exe is running).
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot)
cargo build --release --workspace
if ($LASTEXITCODE) { exit 1 }
$target = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR) } else { "$PWD\target" }
$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup)' }
& $iscc "/DAppVersion=$ver" "/DTarget=$target" installer\fex.iss
if ($LASTEXITCODE) { exit 1 }
