[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ManifestPath,

    [Parameter(Mandatory)]
    [string]$OutputPath
)

$ErrorActionPreference = "Stop"

$metadataJson = & cargo metadata --locked --format-version 1 --manifest-path $ManifestPath
if ($LASTEXITCODE -ne 0) {
    throw "Cargo metadata failed."
}

$metadata = $metadataJson | ConvertFrom-Json
$rootPackage = @($metadata.packages | Where-Object id -EQ $metadata.resolve.root)
if ($rootPackage.Count -ne 1) {
    throw "Unable to identify the root Cargo package."
}

$references = @{}
foreach ($package in $metadata.packages) {
    $encodedName = [Uri]::EscapeDataString($package.name)
    $encodedVersion = [Uri]::EscapeDataString($package.version)
    $references[$package.id] = "pkg:cargo/$encodedName@$encodedVersion"
}

$components = @(
    foreach ($package in $metadata.packages | Where-Object id -NE $metadata.resolve.root | Sort-Object name, version) {
        $component = [ordered]@{
            type = "library"
            "bom-ref" = $references[$package.id]
            name = $package.name
            version = $package.version
            purl = $references[$package.id]
        }

        if (-not [string]::IsNullOrWhiteSpace($package.license)) {
            $component["licenses"] = @(
                [ordered]@{
                    expression = $package.license
                }
            )
        }

        $component
    }
)

$dependencies = @(
    foreach ($node in $metadata.resolve.nodes | Sort-Object id) {
        $dependencyReferences = @(
            $node.deps |
                ForEach-Object { $references[$_.pkg] } |
                Where-Object { $_ } |
                Sort-Object -Unique
        )

        [ordered]@{
            ref = $references[$node.id]
            dependsOn = $dependencyReferences
        }
    }
)

$root = $rootPackage[0]
$bom = [ordered]@{
    "`$schema" = "http://cyclonedx.org/schema/bom-1.5.schema.json"
    bomFormat = "CycloneDX"
    specVersion = "1.5"
    serialNumber = "urn:uuid:$([guid]::NewGuid())"
    version = 1
    metadata = [ordered]@{
        timestamp = [DateTimeOffset]::UtcNow.ToString("o")
        component = [ordered]@{
            type = "application"
            "bom-ref" = $references[$root.id]
            name = $root.name
            version = $root.version
            purl = $references[$root.id]
            licenses = @(
                [ordered]@{
                    expression = $root.license
                }
            )
        }
    }
    components = $components
    dependencies = $dependencies
}

$outputFullPath = [IO.Path]::GetFullPath($OutputPath)
$outputParent = Split-Path -Parent $outputFullPath
[void](New-Item -ItemType Directory -Path $outputParent -Force)
$bom | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $outputFullPath -Encoding utf8

Get-Item -LiteralPath $outputFullPath
