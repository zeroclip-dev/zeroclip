#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$brandDir = $PSScriptRoot
$svgPath = Join-Path $brandDir 'logo-mark.svg'
$icoPath = Join-Path $brandDir 'zeroclip.ico'
$sizes = @(16, 24, 32, 48, 64, 128, 256)

if (-not (Test-Path -LiteralPath $svgPath)) {
    throw "Source SVG not found: $svgPath"
}

function Find-Tool {
    param(
        [string]$Name,
        [string[]]$FallbackPatterns = @()
    )
    $cmd = Get-Command -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) {
        return $cmd.Source
    }
    foreach ($pattern in $FallbackPatterns) {
        $match = Get-Item -Path $pattern -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($match) {
            return $match.FullName
        }
    }
    return $null
}

function Write-PngsToIco {
    param(
        [string[]]$PngPaths,
        [int[]]$IconSizes,
        [string]$OutputPath
    )
    $count = $PngPaths.Count
    $imageBytes = New-Object System.Collections.Generic.List[byte[]]
    for ($i = 0; $i -lt $count; $i++) {
        $imageBytes.Add([System.IO.File]::ReadAllBytes($PngPaths[$i]))
    }

    $stream = [System.IO.File]::Open($OutputPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write)
    $writer = New-Object System.IO.BinaryWriter($stream)
    try {
        # ICONDIR header: reserved (0), type (1 = ICO), count
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$count)

        $offset = 6 + (16 * $count)
        for ($i = 0; $i -lt $count; $i++) {
            $dim = if ($IconSizes[$i] -ge 256) { [byte]0 } else { [byte]$IconSizes[$i] }
            $len = $imageBytes[$i].Length
            $writer.Write($dim)              # width
            $writer.Write($dim)              # height
            $writer.Write([byte]0)           # color count
            $writer.Write([byte]0)           # reserved
            $writer.Write([uint16]1)         # color planes
            $writer.Write([uint16]32)        # bits per pixel
            $writer.Write([uint32]$len)      # image byte size
            $writer.Write([uint32]$offset)   # image offset
            $offset += $len
        }

        for ($i = 0; $i -lt $count; $i++) {
            $writer.Write($imageBytes[$i])
        }
    }
    finally {
        $writer.Dispose()
        $stream.Dispose()
    }
}

$magick = Find-Tool -Name 'magick' -FallbackPatterns @(
    "$env:ProgramFiles\ImageMagick*\magick.exe",
    "${env:ProgramFiles(x86)}\ImageMagick*\magick.exe"
)
if ($magick) {
    $resizeSpec = ($sizes | Sort-Object -Descending) -join ','
    & $magick -background none $svgPath -define "icon:auto-resize=$resizeSpec" $icoPath
    if ($LASTEXITCODE -ne 0) {
        throw "magick failed with exit code $LASTEXITCODE"
    }
    Write-Output "Generated $icoPath using ImageMagick ($magick)."
    exit 0
}

$inkscape = Find-Tool -Name 'inkscape' -FallbackPatterns @(
    "$env:ProgramFiles\Inkscape\bin\inkscape.exe",
    "$env:ProgramFiles\Inkscape\inkscape.exe"
)
$rsvg = $null
if (-not $inkscape) {
    $rsvg = Find-Tool -Name 'rsvg-convert'
}

if (-not $inkscape -and -not $rsvg) {
    Write-Error 'No supported SVG rasterizer found (checked magick, inkscape, rsvg-convert). Install one and re-run this script.'
    exit 1
}

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("zeroclip-ico-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
try {
    $pngPaths = New-Object System.Collections.Generic.List[string]
    foreach ($size in $sizes) {
        $pngPath = Join-Path $tempDir "icon-$size.png"
        if ($inkscape) {
            & $inkscape $svgPath --export-type=png --export-filename=$pngPath --export-width=$size --export-height=$size | Out-Null
        }
        else {
            & $rsvg -w $size -h $size -o $pngPath $svgPath
        }
        if ($LASTEXITCODE -ne 0) {
            throw "SVG rasterizer failed at size ${size}px with exit code $LASTEXITCODE"
        }
        $pngPaths.Add($pngPath)
    }

    Write-PngsToIco -PngPaths $pngPaths.ToArray() -IconSizes $sizes -OutputPath $icoPath
    $usedTool = if ($inkscape) { "Inkscape ($inkscape)" } else { "rsvg-convert ($rsvg)" }
    Write-Output "Generated $icoPath using $usedTool."
}
finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -LiteralPath $tempDir -Recurse -Force
    }
}
