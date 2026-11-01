# Draws Offhand's app icon (icons/source.png, then `pnpm tauri icon`) and the
# tray glyphs (icons/tray/*.png) from Geist ExtraBold. Needs the static TTF:
#   gh release download -R vercel/geist-font -p "geist-font-*.zip"
#   .\scripts\make-icons.ps1 -Font path\to\Geist-ExtraBold.ttf
param([Parameter(Mandatory)][string]$Font, [string]$Preview)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$root = Join-Path $PSScriptRoot '..\src-tauri\icons'
$fonts = New-Object System.Drawing.Text.PrivateFontCollection
$fonts.AddFontFile((Resolve-Path $Font))
$family = $fonts.Families[0]

function Color($hex, $a = 255) {
  $v = [Convert]::ToInt32($hex, 16)
  [System.Drawing.Color]::FromArgb($a, ($v -shr 16) -band 255, ($v -shr 8) -band 255, $v -band 255)
}
function RoundRect($x, $y, $w, $h, $r) {
  $p = New-Object System.Drawing.Drawing2D.GraphicsPath; $d = $r * 2
  $p.AddArc($x, $y, $d, $d, 180, 90); $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
  $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90); $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
  $p.CloseFigure(); $p
}
# The "o" outline, scaled so its ink is $height tall and centred on $cx, top at $top.
function Glyph($cx, $top, $height) {
  $p = New-Object System.Drawing.Drawing2D.GraphicsPath
  $p.AddString('o', $family, 0, 1000, (New-Object System.Drawing.PointF 0, 0), [System.Drawing.StringFormat]::GenericTypographic)
  $b = $p.GetBounds(); $k = $height / $b.Height
  $m = New-Object System.Drawing.Drawing2D.Matrix
  $m.Translate($cx, $top); $m.Scale($k, $k); $m.Translate(-($b.X + $b.Width / 2), -$b.Y)
  $p.Transform($m); $p
}
function New-Canvas($size) {
  $bmp = New-Object System.Drawing.Bitmap $size, $size
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = 'AntiAlias'; $g.PixelOffsetMode = 'HighQuality'; $g.Clear([System.Drawing.Color]::Transparent)
  @($bmp, $g)
}

# --- App icon, 1024 px ---
$bmp, $g = New-Canvas 1024
$tile = RoundRect 32 32 960 960 228
$g.FillPath((New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.Point 0, 32), (New-Object System.Drawing.Point 0, 992), (Color '2A2A30'), (Color '0A0A0B')), $tile)
$g.SetClip($tile)
$sheen = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.Point 0, 32), (New-Object System.Drawing.Point 0, 420), (Color 'FFFFFF' 34), (Color 'FFFFFF' 0)
$g.DrawPath((New-Object System.Drawing.Pen $sheen, 8), $tile)
$g.ResetClip()
$s = Glyph 512 196 480
$g.FillPath((New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.Point 0, 196), (New-Object System.Drawing.Point 0, 676), (Color 'FFFFFF'), (Color 'E4E2DC')), $s)
$bar = RoundRect 347 730 330 96 48
$g.FillPath((New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.Point 0, 730), (New-Object System.Drawing.Point 0, 826), (Color '5B5FFF'), (Color '2A2EE6')), $bar)
$bmp.Save((Join-Path $root 'source.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

# --- Tray glyphs, drawn at 128 px and scaled to 32 for clean edges ---
$tray = Join-Path $root 'tray'
New-Item -ItemType Directory -Force $tray | Out-Null
$themes = @{ dark = @('F3F2EE', '6C70FF'); light = @('0C0C0D', '3236F5') }
$states = @{ idle = 46; rec = 92; work = 68; off = 46 }
foreach ($t in $themes.Keys) {
  foreach ($st in $states.Keys) {
    $big, $g = New-Canvas 128
    $fg, $accent = $themes[$t]
    if ($st -eq 'off') { $fg = '8C8B86'; $accent = '8C8B86' }
    $g.FillPath((New-Object System.Drawing.SolidBrush (Color $fg)), (Glyph 64 6 84))
    $w = $states[$st]
    $g.FillPath((New-Object System.Drawing.SolidBrush (Color $accent)), (RoundRect (64 - $w / 2) 102 $w 20 10))
    $g.Dispose()
    $small = New-Object System.Drawing.Bitmap 32, 32
    $g2 = [System.Drawing.Graphics]::FromImage($small); $g2.InterpolationMode = 'HighQualityBicubic'; $g2.PixelOffsetMode = 'HighQuality'
    $g2.DrawImage($big, 0, 0, 32, 32); $g2.Dispose(); $big.Dispose()
    $small.Save((Join-Path $tray "$t-$st.png"), [System.Drawing.Imaging.ImageFormat]::Png); $small.Dispose()
  }
}

# --- Optional preview: the icon at taskbar sizes on dark and light bars ---
if ($Preview) {
  $src = [System.Drawing.Image]::FromFile((Join-Path $root 'source.png'))
  $sheet = New-Object System.Drawing.Bitmap 760, 400
  $g = [System.Drawing.Graphics]::FromImage($sheet); $g.SmoothingMode = 'AntiAlias'; $g.InterpolationMode = 'HighQualityBicubic'
  $g.Clear((Color 'E9E8E4'))
  $g.DrawImage($src, 30, 40, 240, 240)
  foreach ($row in @(@('202020', 60), @('F3F3F3', 220))) {
    $bg, $y = $row
    $g.FillRectangle((New-Object System.Drawing.SolidBrush (Color $bg)), 300, $y, 430, 120)
    $x = 320
    foreach ($size in 64, 48, 32, 24, 16) { $g.DrawImage($src, $x, $y + 60 - $size / 2, $size, $size); $x += $size + 24 }
    $theme = if ($bg -eq '202020') { 'dark' } else { 'light' }
    $x += 6
    foreach ($st in 'idle', 'rec', 'work') {
      $img = [System.Drawing.Image]::FromFile((Join-Path $tray "$theme-$st.png"))
      $g.DrawImage($img, $x, $y + 48, 24, 24); $img.Dispose(); $x += 34
    }
  }
  $sheet.Save($Preview, [System.Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $sheet.Dispose(); $src.Dispose()
}
