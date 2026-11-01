# Builds the release exe the right way and restarts the running Offhand on it.
#
# - Always through `pnpm tauri build`: plain `cargo build --release` leaves
#   the app window's pages out, and the window then tries localhost:1420.
# - The running exe is renamed first, so dictation keeps working while the
#   build runs.
# - The new copy is started through Explorer, so it never runs inside the
#   private file system of whatever app ran this script (see storage.rs).

# Below normal priority: the build, and all it starts, yields to a running game.
[System.Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'BelowNormal'

# Not 'Stop': PowerShell 5 turns pnpm's progress lines on stderr into errors.
# Each step checks its own result instead.
$root = Split-Path $PSScriptRoot -Parent
$exe = Join-Path $root 'src-tauri\target\release\offhand.exe'
$old = Join-Path $root 'src-tauri\target\release\offhand.old.exe'

if (Test-Path $exe) {
    Remove-Item $old -ErrorAction SilentlyContinue
    Rename-Item $exe $old -ErrorAction Stop
}

Push-Location $root
try {
    pnpm tauri build --no-bundle
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $exe)) {
        if (Test-Path $old) { Rename-Item $old $exe }
        throw 'build failed; the previous exe is back in place'
    }
} finally {
    Pop-Location
}

Get-Process offhand, offhand.old -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Start-Process (Join-Path $env:WINDIR 'explorer.exe') -ArgumentList "`"$exe`""
Start-Sleep -Seconds 4
Remove-Item $old -ErrorAction SilentlyContinue

$p = Get-Process offhand -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { throw 'Offhand did not start' }
$cpu = $p.TotalProcessorTime
Start-Sleep -Seconds 10
$p.Refresh()
'Offhand running: {0} ms CPU over 10 s idle, {1:N1} MB private' -f [int]($p.TotalProcessorTime - $cpu).TotalMilliseconds, ($p.PrivateMemorySize64 / 1MB)
