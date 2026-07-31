[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Version,

    [Parameter(Mandatory)]
    [string]$ExecutablePath,

    [Parameter(Mandatory)]
    [string]$PdbPath,

    [Parameter(Mandatory)]
    [string]$OutputDirectory
)

$ErrorActionPreference = "Stop"

$executable = Get-Item -LiteralPath $ExecutablePath
$pdb = Get-Item -LiteralPath $PdbPath
$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$readme = Get-Item -LiteralPath (Join-Path $repositoryRoot "README.md")
$license = Get-Item -LiteralPath (Join-Path $repositoryRoot "LICENSE")
$icon = Get-Item -LiteralPath (Join-Path $repositoryRoot "icon.png")
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
[void](New-Item -ItemType Directory -Path $outputRoot -Force)

$packageName = "MonitorManager-v$Version-windows-x86_64"
$packagePath = Join-Path $outputRoot "$packageName.zip"
$symbolsPath = Join-Path $outputRoot "$packageName-symbols.zip"
$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) "monitor-manager-release-$([guid]::NewGuid().ToString('N'))"
$packageRoot = Join-Path $temporaryRoot $packageName
$symbolsRoot = Join-Path $temporaryRoot "$packageName-symbols"

try {
    [void](New-Item -ItemType Directory -Path $packageRoot -Force)
    [void](New-Item -ItemType Directory -Path $symbolsRoot -Force)

    Copy-Item -LiteralPath $executable.FullName -Destination (Join-Path $packageRoot "MonitorManager.exe")
    Copy-Item -LiteralPath $readme.FullName -Destination $packageRoot
    Copy-Item -LiteralPath $license.FullName -Destination $packageRoot
    Copy-Item -LiteralPath $icon.FullName -Destination $packageRoot
    Copy-Item -LiteralPath $pdb.FullName -Destination (Join-Path $symbolsRoot "MonitorManager.pdb")

    Compress-Archive -Path (Join-Path $packageRoot "*") -DestinationPath $packagePath -CompressionLevel Optimal -Force
    Compress-Archive -Path (Join-Path $symbolsRoot "*") -DestinationPath $symbolsPath -CompressionLevel Optimal -Force
} finally {
    if (Test-Path -LiteralPath $temporaryRoot) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force
    }
}

Get-Item -LiteralPath $packagePath, $symbolsPath
