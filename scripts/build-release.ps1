param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$ReleaseDirectory
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$packageLine = Select-String -Path (Join-Path $repoRoot "cli\Cargo.toml") -Pattern '^version = "([^\"]+)"' | Select-Object -First 1
if (-not $packageLine -or $packageLine.Matches[0].Groups[1].Value -ne $Version) {
    throw "mach: requested version $Version does not match cli version"
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($architecture -ne "X64") { throw "mach: unsupported cli release platform windows-$architecture" }
$targetDirectory = if ($env:MACH_RELEASE_TARGET_DIR) { $env:MACH_RELEASE_TARGET_DIR } else { Join-Path $env:TEMP "mach-release-target" }
$previousTarget = $env:CARGO_TARGET_DIR
$env:CARGO_TARGET_DIR = $targetDirectory

try {
    cargo build --locked --release --manifest-path (Join-Path $repoRoot "Cargo.toml") -p mach-cli
    if ($LASTEXITCODE -ne 0) { throw "mach: cli build failed" }

    New-Item -ItemType Directory -Force -Path $ReleaseDirectory | Out-Null
    $staging = Join-Path $env:TEMP ("mach-cli-" + [guid]::NewGuid())
    try {
        New-Item -ItemType Directory -Path $staging | Out-Null
        Copy-Item (Join-Path $targetDirectory "release\mach.exe") (Join-Path $staging "mach.exe")
        $archiveName = "mach-cli-windows-x86_64.zip"
        $archive = Join-Path $ReleaseDirectory $archiveName
        Compress-Archive -Path (Join-Path $staging "mach.exe") -DestinationPath $archive -Force
        $hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
        "$hash  $archiveName" | Set-Content "$archive.sha256"
    } finally {
        if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
    }
} finally {
    $env:CARGO_TARGET_DIR = $previousTarget
}
