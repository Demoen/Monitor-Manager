[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ExecutablePath,

    [string]$TimestampUrl = "https://timestamp.digicert.com"
)

$ErrorActionPreference = "Stop"

$certificateBase64 = $env:SIGNING_CERTIFICATE_BASE64
$pfxProtectionValue = $env:SIGNING_CERTIFICATE_PASSWORD
$hasCertificate = -not [string]::IsNullOrWhiteSpace($certificateBase64)
$hasProtectionValue = -not [string]::IsNullOrWhiteSpace($pfxProtectionValue)

if (-not $hasCertificate -and -not $hasProtectionValue) {
    Write-Output "Authenticode signing is not configured."
    return
}

if (-not $hasCertificate -or -not $hasProtectionValue) {
    throw "Both Windows signing secrets must be configured."
}

$executable = (Resolve-Path $ExecutablePath).Path
$certificatePath = Join-Path $env:RUNNER_TEMP "monitor-manager-signing-$([guid]::NewGuid().ToString('N')).pfx"
$importedCertificates = @()

try {
    [IO.File]::WriteAllBytes($certificatePath, [Convert]::FromBase64String($certificateBase64))
    $secureProtectionValue = ConvertTo-SecureString $pfxProtectionValue -AsPlainText -Force
    $importedCertificates = @(Import-PfxCertificate -FilePath $certificatePath -CertStoreLocation Cert:\CurrentUser\My -Password $secureProtectionValue)
    $signingCertificate = $importedCertificates | Where-Object HasPrivateKey | Select-Object -First 1
    if (-not $signingCertificate) {
        throw "The signing certificate does not contain a private key."
    }

    $signTool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Filter signtool.exe -Recurse -File |
        Where-Object FullName -Match '\\x64\\signtool\.exe$' |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if (-not $signTool) {
        throw "signtool.exe was not found."
    }

    & $signTool.FullName sign /sha1 $signingCertificate.Thumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 $executable
    if ($LASTEXITCODE -ne 0) {
        throw "Authenticode signing failed."
    }

    & $signTool.FullName verify /pa /v $executable
    if ($LASTEXITCODE -ne 0) {
        throw "Authenticode signature verification failed."
    }
} finally {
    foreach ($certificate in $importedCertificates) {
        $storePath = "Cert:\CurrentUser\My\$($certificate.Thumbprint)"
        if (Test-Path -LiteralPath $storePath) {
            Remove-Item -LiteralPath $storePath -Force
        }
    }

    if (Test-Path -LiteralPath $certificatePath) {
        Remove-Item -LiteralPath $certificatePath -Force
    }
}
