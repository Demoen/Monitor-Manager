[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ManifestPath,

    [Parameter(Mandatory)]
    [string]$Tag
)

$ErrorActionPreference = "Stop"

$metadataJson = & cargo metadata --locked --format-version 1 --manifest-path $ManifestPath
if ($LASTEXITCODE -ne 0) {
    throw "Cargo metadata failed."
}

$metadata = $metadataJson | ConvertFrom-Json
$package = @($metadata.packages | Where-Object id -EQ $metadata.resolve.root)
if ($package.Count -ne 1) {
    throw "Unable to identify the root Cargo package."
}

$expectedTag = "v$($package[0].version)"
if ($Tag -cne $expectedTag) {
    throw "Tag '$Tag' does not match Cargo package version '$($package[0].version)'. Expected '$expectedTag'."
}

$package[0].version
