# Builds the installer people download.
#
# - The exe needs a few files a clean PC may not have: DirectML.dll and the
#   Visual C++ runtime. They're copied into target\redist and installed next
#   to the exe (tauri.installer.conf.json).
# - It builds into its own folder (--target), so a running Offhand and the
#   exe scripts/release-local.ps1 made are left alone.

# Below normal priority: the build, and all it starts, yields to a running game.
[System.Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'BelowNormal'

# Not 'Stop': PowerShell 5 turns pnpm's progress lines on stderr into errors.
# Each step checks its own result instead.
$root = Split-Path $PSScriptRoot -Parent
$target = Join-Path $root 'src-tauri\target'
$redist = Join-Path $target 'redist'
New-Item -ItemType Directory -Force $redist | Out-Null

$crt = Get-ChildItem 'C:\Program Files*\Microsoft Visual Studio\2022\*\VC\Redist\MSVC\*\x64\Microsoft.VC143.CRT' -Directory |
    Sort-Object FullName | Select-Object -Last 1
if (-not $crt) { throw 'no Visual C++ runtime found; install the Visual Studio Build Tools' }
foreach ($dll in 'msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
    Copy-Item (Join-Path $crt.FullName $dll) $redist -Force -ErrorAction Stop
}
$dml = Join-Path $target 'release\DirectML.dll'
if (-not (Test-Path $dml)) { throw 'no DirectML.dll yet; run scripts/release-local.ps1 once first' }
Copy-Item $dml $redist -Force -ErrorAction Stop

Push-Location $root
try {
    pnpm tauri build --target x86_64-pc-windows-msvc --config src-tauri/tauri.installer.conf.json
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
} finally {
    Pop-Location
}

$setup = Get-ChildItem (Join-Path $target 'x86_64-pc-windows-msvc\release\bundle\nsis\*-setup.exe') |
    Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $setup) { throw 'no installer was made' }
'{0}: {1:N1} MB, SHA-256 {2}' -f $setup.FullName, ($setup.Length / 1MB), (Get-FileHash $setup.FullName).Hash
