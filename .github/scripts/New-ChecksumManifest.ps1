[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$AssetDirectory,

    [Parameter(Mandatory)]
    [string]$OutputPath
)

$ErrorActionPreference = "Stop"

$assetRoot = (Resolve-Path $AssetDirectory).Path
$outputFullPath = [IO.Path]::GetFullPath($OutputPath)
$assets = @(
    Get-ChildItem -LiteralPath $assetRoot -File |
        Where-Object FullName -NE $outputFullPath |
        Sort-Object Name
)

if ($assets.Count -eq 0) {
    throw "No release assets were found in '$assetRoot'."
}

$lines = @(
    foreach ($asset in $assets) {
        $hash = (Get-FileHash -LiteralPath $asset.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $($asset.Name)"
    }
)

$lines | Set-Content -LiteralPath $outputFullPath -Encoding utf8
Get-Item -LiteralPath $outputFullPath
