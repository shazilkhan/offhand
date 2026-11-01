# End-to-end check: hold the hotkey over a throwaway text box and confirm the
# transcript is pasted and the clipboard restored. Mic audio is swapped for a
# TTS clip via OFFHAND_TEST_WAV. It takes keyboard focus for ~10 s, so don't
# run it while using the PC. Aborts without sending keys if focus is lost.
param([int]$HoldMs = 1200)
$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot '..\src-tauri\target\release\offhand.exe'
$work = Join-Path $env:TEMP 'offhand-e2e'
New-Item -ItemType Directory -Force $work | Out-Null
$out = Join-Path $work 'out.txt'
$wav = Join-Path $work 'speech.wav'
Remove-Item $out -ErrorAction SilentlyContinue
if (-not (Test-Path $wav)) {
  Add-Type -AssemblyName System.Speech
  $tts = New-Object System.Speech.Synthesis.SpeechSynthesizer
  $fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo 16000, ([System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen), ([System.Speech.AudioFormat.AudioChannel]::Mono)
  $tts.SetOutputToWaveFile($wav, $fmt)
  $tts.Speak('Thanks for sending the posting over. Can we find thirty minutes on Thursday to talk it through?')
  $tts.Dispose()
}

Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public class W {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  public static string Title(IntPtr h) { var s = new StringBuilder(256); GetWindowText(h, s, 256); return s.ToString(); }
  public static void Focus(IntPtr target) {
    uint pid; var fgThread = GetWindowThreadProcessId(GetForegroundWindow(), out pid);
    var me = GetCurrentThreadId();
    AttachThreadInput(me, fgThread, true); SetForegroundWindow(target); AttachThreadInput(me, fgThread, false);
  }
}
"@

$previous = [W]::GetForegroundWindow()

Get-Process offhand -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$env:OFFHAND_TEST_WAV = $wav
Start-Process $exe
Remove-Item Env:OFFHAND_TEST_WAV
Start-Sleep -Seconds 2

Set-Clipboard -Value 'ORIGINAL CLIPBOARD 123'
$target = Start-Process powershell -ArgumentList '-NoProfile', '-STA', '-File', (Join-Path $PSScriptRoot 'e2e-target.ps1'), $out, 8000 -PassThru
$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 40 -and $hwnd -eq [IntPtr]::Zero; $i++) {
  Start-Sleep -Milliseconds 100
  $target.Refresh(); $hwnd = $target.MainWindowHandle
}
[W]::Focus($hwnd)
Start-Sleep -Milliseconds 400
$fg = [W]::GetForegroundWindow()
if ($fg -ne $hwnd) {
  "ABORT: test window is not focused (foreground is '$([W]::Title($fg))'); no keys sent"
  $target | Stop-Process -Force
} else {
  # Hold Ctrl+Win, then release Ctrl first, like a person would.
  [W]::keybd_event(0xA2, 0, 0, [UIntPtr]::Zero)
  [W]::keybd_event(0x5B, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds $HoldMs
  [W]::keybd_event(0xA2, 0, 2, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 80
  [W]::keybd_event(0x5B, 0, 2, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 600
  "foreground after release: $([W]::Title([W]::GetForegroundWindow()))"
  $target.WaitForExit(15000) | Out-Null
  "pasted text: [$(Get-Content $out -Raw -ErrorAction SilentlyContinue)]"
  Start-Sleep -Milliseconds 300
  "clipboard now: [$(Get-Clipboard -Raw)]"
}
if ($previous -ne [IntPtr]::Zero) { [W]::Focus($previous) }
"--- log"
Get-Content "$env:LOCALAPPDATA\Offhand\offhand.log" -Tail 6
# Leave a normal instance running, not the test one.
Get-Process offhand -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Process $exe
