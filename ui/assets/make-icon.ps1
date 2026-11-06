# Draws the Fast Explorer icon (a folder with a lightning bolt) and writes
#   icon-256.png, icon-32.png        (window / tab icons, used by app.slint)
#   fast-explorer.ico                (embedded in fex.exe by build.rs)
# Run from anywhere:  pwsh ui/assets/make-icon.ps1
Add-Type -AssemblyName System.Drawing
$out = $PSScriptRoot

function RoundRect($g, $x, $y, $w, $h, $r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90); $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90); $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure(); $p
}

function Draw([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'; $g.PixelOffsetMode = 'HighQuality'
    $g.ScaleTransform($size / 256.0, $size / 256.0)

    # Back of the folder with its tab.
    $back = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 214, 140, 30))
    $g.FillPath($back, (RoundRect $g 16 38 100 48 14))
    $g.FillPath($back, (RoundRect $g 16 62 224 160 18))

    # Front panel, lit from the top.
    $front = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
        [System.Drawing.Point]::new(0, 92), [System.Drawing.Point]::new(0, 228),
        [System.Drawing.Color]::FromArgb(255, 255, 214, 102), [System.Drawing.Color]::FromArgb(255, 246, 176, 52))
    $g.FillPath($front, (RoundRect $g 16 92 224 136 18))

    # Lightning bolt: blue with a white rim so it reads on the yellow at small sizes.
    $bolt = New-Object System.Drawing.Drawing2D.GraphicsPath
    $bolt.AddPolygon([System.Drawing.PointF[]]@(
        (New-Object System.Drawing.PointF 146, 100), (New-Object System.Drawing.PointF 94, 170),
        (New-Object System.Drawing.PointF 126, 170), (New-Object System.Drawing.PointF 110, 222),
        (New-Object System.Drawing.PointF 166, 146), (New-Object System.Drawing.PointF 134, 146),
        (New-Object System.Drawing.PointF 152, 100)))
    $rim = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), 12
    $rim.LineJoin = 'Round'
    $g.DrawPath($rim, $bolt)
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 21, 101, 192))), $bolt)
    $g.Dispose()
    $bmp
}

function PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); $ms.ToArray()
}

(Draw 256).Save((Join-Path $out "icon-256.png"), [System.Drawing.Imaging.ImageFormat]::Png)
(Draw 32).Save((Join-Path $out "icon-32.png"), [System.Drawing.Imaging.ImageFormat]::Png)

# .ico with PNG-compressed images (supported since Windows Vista).
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$images = [System.Collections.Generic.List[byte[]]]::new()
foreach ($s in $sizes) { $images.Add([byte[]](PngBytes (Draw $s))) }
$ico = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $ico
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i] % 256  # 256 is written as 0
    $w.Write([byte]$s); $w.Write([byte]$s); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$images[$i].Length); $w.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $w.Write($img) }
[System.IO.File]::WriteAllBytes((Join-Path $out "fast-explorer.ico"), $ico.ToArray())
"wrote icon-256.png, icon-32.png, fast-explorer.ico to $out"
