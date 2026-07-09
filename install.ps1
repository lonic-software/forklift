# forklift installer for Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/lonic-software/forklift/main/install.ps1 | iex
#
# Choose what to install (default is the CLI). Set the component before piping:
#   $env:FORKLIFT_COMPONENT="server"; irm .../install.ps1 | iex   # the forklift-server head
#   $env:FORKLIFT_COMPONENT="all";    irm .../install.ps1 | iex   # both heads
#
# Environment overrides:
#   FORKLIFT_COMPONENT    cli (default) | server | all
#   FORKLIFT_VERSION      install a specific tag, e.g. v0.1.0 (default: latest release)
#   FORKLIFT_INSTALL_DIR  where to put the binaries (default: %LOCALAPPDATA%\Programs\forklift)
#   FORKLIFT_REPO         GitHub repo slug          (default: lonic-software/forklift)
#   FORKLIFT_NO_ALIAS     skip creating the `fl` short alias next to the forklift binary
param([string]$Component = $env:FORKLIFT_COMPONENT)

$ErrorActionPreference = "Stop"

$Repo = if ($env:FORKLIFT_REPO) { $env:FORKLIFT_REPO } else { "lonic-software/forklift" }
$Version = if ($env:FORKLIFT_VERSION) { $env:FORKLIFT_VERSION } else { "latest" }
$InstallDir = if ($env:FORKLIFT_INSTALL_DIR) { $env:FORKLIFT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\forklift" }
if (-not $Component) { $Component = "cli" }

$Binaries = switch ($Component) {
    { $_ -in "cli", "forklift" }           { @("forklift"); break }
    { $_ -in "server", "forklift-server" } { @("forklift-server"); break }
    { $_ -in "all", "both" }               { @("forklift", "forklift-server"); break }
    default { throw "unknown component '$Component' (want: cli | server | all)" }
}

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") {
    throw "no Windows ARM build yet - build from source: cargo install --path crates/forklift"
}

$Target = "x86_64-pc-windows-msvc"
$Base = if ($Version -eq "latest") {
    "https://github.com/$Repo/releases/latest/download"
} else {
    "https://github.com/$Repo/releases/download/$Version"
}

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $Tmp | Out-Null
try {
    # Fetch checksums once (best effort); reused to verify every head.
    $Checksums = $null
    try {
        Invoke-WebRequest "$Base/checksums.txt" -OutFile (Join-Path $Tmp "checksums.txt")
        $Checksums = Get-Content (Join-Path $Tmp "checksums.txt")
    } catch [System.Net.WebException] {
        Write-Host "warning: could not fetch checksums.txt; skipping verification"
    }

    New-Item -ItemType Directory -Force $InstallDir | Out-Null

    foreach ($Name in $Binaries) {
        $Asset = "$Name-$Target.zip"
        Write-Host "downloading $Base/$Asset"
        Invoke-WebRequest "$Base/$Asset" -OutFile (Join-Path $Tmp $Asset)

        if ($Checksums) {
            $line = $Checksums | Where-Object { $_ -match [regex]::Escape($Asset) }
            if (-not $line) { throw "$Asset missing from checksums.txt" }
            $expected = ($line -split '\s+')[0]
            $actual = (Get-FileHash (Join-Path $Tmp $Asset) -Algorithm SHA256).Hash.ToLower()
            if ($expected -ne $actual) { throw "checksum verification FAILED for $Asset - refusing to install" }
            Write-Host "  checksum ok"
        }

        Expand-Archive (Join-Path $Tmp $Asset) -DestinationPath $Tmp -Force
        Copy-Item (Join-Path $Tmp "$Name.exe") (Join-Path $InstallDir "$Name.exe") -Force
        Write-Host "installed $InstallDir\$Name.exe"

        # The `fl` short alias (DESIGN.html §5.0 F): every install method converges on the
        # same `forklift alias install`, so this is the one place the behavior lives. Default
        # on, opt out with FORKLIFT_NO_ALIAS=1; best effort (never fails the install over it).
        if ($Name -eq "forklift" -and -not $env:FORKLIFT_NO_ALIAS) {
            # $ErrorActionPreference = "Stop" only turns PowerShell-native errors into
            # exceptions - it does not touch an external process's exit code, so the
            # try/catch alone never fires on failure here. Check $LASTEXITCODE ourselves,
            # and keep both: the catch still covers the process failing to start at all.
            try {
                & (Join-Path $InstallDir "forklift.exe") alias install | Out-Null
                if ($LASTEXITCODE -ne 0) {
                    Write-Host "warning: could not create the `"fl`" alias (exit code $LASTEXITCODE); `"forklift alias install`" retries it"
                }
            } catch {
                Write-Host "warning: could not create the `"fl`" alias ($_); `"forklift alias install`" retries it"
            }
        }
    }

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
        Write-Host "added $InstallDir to your user PATH (restart your terminal)"
    }
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
