# 生成 VOC 图标（MSIX 各尺寸 + 窗口图标）。只需在图标设计改动后运行一次。
param([string]$OutDir = "")
Add-Type -AssemblyName System.Drawing
$ErrorActionPreference = "Stop"
if (-not $OutDir) { $OutDir = if ($PSScriptRoot) { Join-Path $PSScriptRoot "Assets" } else { Join-Path (Get-Location) "deploy\windows\Assets" } }
New-Item -ItemType Directory -Force $OutDir | Out-Null
# System.Drawing 需要绝对路径（.NET 的当前目录和 PowerShell 的不一定一样）
$out = (Resolve-Path $OutDir).Path

function New-VocIcon([int]$size, [string]$path, [bool]$plate = $true) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = "AntiAlias"
    $g.TextRenderingHint = "AntiAliasGridFit"
    $g.Clear([System.Drawing.Color]::Transparent)
    $s = $size / 256.0
    if ($plate) {
        # 圆角方块，紫色渐变
        $r = 56 * $s
        $rect = New-Object System.Drawing.RectangleF (8 * $s), (8 * $s), (240 * $s), (240 * $s)
        $gp = New-Object System.Drawing.Drawing2D.GraphicsPath
        $gp.AddArc($rect.X, $rect.Y, $r * 2, $r * 2, 180, 90)
        $gp.AddArc($rect.Right - $r * 2, $rect.Y, $r * 2, $r * 2, 270, 90)
        $gp.AddArc($rect.Right - $r * 2, $rect.Bottom - $r * 2, $r * 2, $r * 2, 0, 90)
        $gp.AddArc($rect.X, $rect.Bottom - $r * 2, $r * 2, $r * 2, 90, 90)
        $gp.CloseFigure()
        $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, ([System.Drawing.Color]::FromArgb(255, 0x8f, 0x82, 0xff)), ([System.Drawing.Color]::FromArgb(255, 0x5a, 0x4b, 0xe0)), 45.0
        $g.FillPath($brush, $gp)
    }
    $white = [System.Drawing.Color]::White
    # 「V」：两条粗圆头线
    $pen = New-Object System.Drawing.Pen $white, (30 * $s)
    $pen.StartCap = "Round"; $pen.EndCap = "Round"; $pen.LineJoin = "Round"
    [System.Drawing.PointF[]]$pts = @(
        (New-Object System.Drawing.PointF (62 * $s), (72 * $s)),
        (New-Object System.Drawing.PointF (112 * $s), (186 * $s)),
        (New-Object System.Drawing.PointF (162 * $s), (72 * $s))
    )
    $g.DrawLines($pen, $pts)
    # 两道声波
    $wave = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(230, 255, 255, 255)), (14 * $s)
    $wave.StartCap = "Round"; $wave.EndCap = "Round"
    $g.DrawArc($wave, (150 * $s), (92 * $s), (52 * $s), (72 * $s), -55, 110)
    $g.DrawArc($wave, (150 * $s), (66 * $s), (86 * $s), (124 * $s), -55, 110)
    $g.Dispose()
    # GDI+ 不支持超过 260 字符的路径：先存到短路径的临时文件再复制
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("voc-icon-" + [guid]::NewGuid().ToString("N") + ".png")
    $bmp.Save($tmp, [System.Drawing.Imaging.ImageFormat]::Png)
    [IO.File]::Copy($tmp, $path, $true)
    Remove-Item $tmp
    $bmp.Dispose()
}

New-VocIcon 256 (Join-Path $out "icon-256.png")
New-VocIcon 150 (Join-Path $out "Square150x150Logo.png")
New-VocIcon 44 (Join-Path $out "Square44x44Logo.png")
New-VocIcon 50 (Join-Path $out "StoreLogo.png")
# 任务栏「无底板」版本（Windows 会把它画在主题色上）
New-VocIcon 44 (Join-Path $out "Square44x44Logo.targetsize-44_altform-unplated.png") $false
Write-Host "图标已生成到 $out"
