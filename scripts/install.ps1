param(
    [string]$InstallDir = "$env:USERPROFILE\.mach\bin"
)

$ErrorActionPreference = "Stop"
$releaseBase = if ($env:MACH_RELEASES_URL) { $env:MACH_RELEASES_URL.TrimEnd('/') } else { "https://machinesatplay.com/releases" }
$version = (Invoke-WebRequest -Uri "$releaseBase/latest/version").Content.Trim()
if ($version -notmatch '^\d+\.\d+\.\d+$') {
    throw "mach: invalid release version"
}

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$platform = if ($architecture -eq "X64") { "windows-x86_64" } else { $null }
$temporaryDir = Join-Path ([System.IO.Path]::GetTempPath()) ("mach-install-" + [guid]::NewGuid())

try {
    New-Item -ItemType Directory -Path $temporaryDir | Out-Null
    $installed = $false
    if ($platform) {
        $archive = "mach-cli-$platform.zip"
        $archivePath = Join-Path $temporaryDir $archive
        $checksumPath = "$archivePath.sha256"
        try {
            Invoke-WebRequest -Uri "$releaseBase/v$version/$archive" -OutFile $archivePath
            Invoke-WebRequest -Uri "$releaseBase/v$version/$archive.sha256" -OutFile $checksumPath
            $downloaded = $true
        } catch {
            $downloaded = $false
        }
        if ($downloaded) {
            $expected = (Get-Content $checksumPath).Split()[0].ToLowerInvariant()
            $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
            if ($actual -ne $expected) { throw "mach: download checksum did not match" }
            $unpacked = Join-Path $temporaryDir "cli"
            Expand-Archive -Path $archivePath -DestinationPath $unpacked
            New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
            Copy-Item (Join-Path $unpacked "mach.exe") (Join-Path $InstallDir "mach.exe") -Force
            $installed = $true
        }
    }

    if (-not $installed) {
        throw "mach: no prebuilt CLI is available for windows-$architecture"
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathEntries = @($userPath -split ";" | Where-Object { $_ })
    if ($pathEntries -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable("Path", ((@($pathEntries) + $InstallDir) -join ";"), "User")
        Write-Host "mach: added $InstallDir to your user PATH. open a new terminal."
    }
    Write-Host "mach: installed $(Join-Path $InstallDir 'mach.exe')"
    if ($env:MACH_SKIP_SETUP -ne "1") {
        & (Join-Path $InstallDir "mach.exe") setup
        if ($LASTEXITCODE -ne 0) { throw "mach: setup failed" }
    }
} finally {
    if (Test-Path $temporaryDir) {
        Remove-Item -Recurse -Force $temporaryDir
    }
}
