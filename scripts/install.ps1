#!/usr/bin/env pwsh
# bastion installer script for Windows (PowerShell)
#
# Usage:
#   irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex
#   $env:Version="0.1.0"; irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex
#   $env:BinDir="C:\Tools"; irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex
#
# Options (set via environment variables or script parameters):
#   Version      Specify a version (default: latest)
#   BinDir       Specify the installation directory (default: $env:LOCALAPPDATA\Programs\bastion)
#   Help         Show help message (set $env:Help="true")

param(
    [string]$Version = $env:Version,
    [string]$BinDir = $env:BinDir,
    [switch]$Help = ($env:Help -eq "true")
)

# GitHub repository configuration
$REPO = "attunehq/bastion"
$GITHUB_BASE = "https://github.com/$REPO"
$GITHUB_DOWNLOAD = "$GITHUB_BASE/releases/download"

function Write-Info {
    param([string]$Message)
    Write-Host $Message -ForegroundColor Green
}

function Write-Error-Message {
    param([string]$Message)
    Write-Host "Error: $Message" -ForegroundColor Red
    exit 1
}

function Write-Warning-Message {
    param([string]$Message)
    Write-Host "Warning: $Message" -ForegroundColor Yellow
}

function Show-Help {
    Write-Host @"
bastion installer for Windows

Usage:
  irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex
  `$env:Version="0.1.0"; irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex
  `$env:BinDir="C:\Tools"; irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex

Options (set via environment variables):
  Version      Specify a version (default: latest)
  BinDir       Specify the installation directory (default: `$env:LOCALAPPDATA\Programs\bastion)
  Help         Show this help message (set `$env:Help="true")

Examples:
  # Install latest version
  irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex

  # Install specific version
  `$env:Version="0.1.0"; irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex

  # Install to custom directory
  `$env:BinDir="C:\Tools"; irm https://raw.githubusercontent.com/attunehq/bastion/main/scripts/install.ps1 | iex
"@
    exit 0
}

function Get-LatestVersion {
    # Resolve the tag from the redirect target of /releases/latest rather than
    # the JSON API. api.github.com is tightly rate limited for unauthenticated
    # callers (60 requests/hour/IP), so it 403s from shared NATs and CI runners;
    # the github.com redirect has no such limit.
    $latestUrl = "$GITHUB_BASE/releases/latest"

    try {
        $response = Invoke-WebRequest -Uri $latestUrl -MaximumRedirection 5 -ErrorAction Stop
        $finalUrl = $response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri
    }
    catch {
        Write-Error-Message "Failed to resolve latest release from $latestUrl. Error: $_"
    }

    # The redirect lands on .../releases/tag/vX.Y.Z; take the segment after /tag/.
    if ($finalUrl -notmatch '/tag/') {
        Write-Error-Message "Could not parse version from latest release URL: $finalUrl"
    }
    return ($finalUrl -replace '.*/tag/', '') -replace '^v', ''
}

function Get-Platform {
    $arch = $env:PROCESSOR_ARCHITECTURE

    switch ($arch) {
        "AMD64" { return "x86_64-pc-windows-gnu" }
        "ARM64" { Write-Error-Message "Windows ARM64 is not currently supported. Please build from source or use x64 emulation." }
        default { Write-Error-Message "Unsupported architecture: $arch" }
    }
}

function Install-Binary {
    param(
        [string]$Platform,
        [string]$Version,
        [string]$InstallDir,
        [string]$TempDir
    )

    $Version = $Version -replace '^v', ''
    $archiveName = "bastion-$Platform.tar.gz"
    $tag = "v$Version"
    $downloadUrl = "$GITHUB_DOWNLOAD/$tag/$archiveName"
    $checksumsUrl = "$GITHUB_DOWNLOAD/$tag/checksums.txt"

    Write-Info "Downloading bastion $Version for $Platform..."

    # Create temporary directory
    $tempExtractDir = Join-Path $TempDir "bastion-install-$(Get-Random)"
    New-Item -ItemType Directory -Force -Path $tempExtractDir | Out-Null

    $archivePath = Join-Path $tempExtractDir $archiveName

    try {
        # Download archive
        Invoke-WebRequest -Uri $downloadUrl -OutFile $archivePath -ErrorAction Stop
    }
    catch {
        Write-Error-Message "Failed to download from $downloadUrl. Error: $_"
    }

    Write-Info "Verifying checksum..."

    # Download checksums
    try {
        $checksums = Invoke-RestMethod -Uri $checksumsUrl -ErrorAction Stop
    }
    catch {
        Write-Error-Message "Could not download checksums from $checksumsUrl. Error: $_"
    }

    # Calculate hash
    try {
        $hash = (Get-FileHash -Path $archivePath -Algorithm SHA256 -ErrorAction Stop).Hash.ToLower()
    }
    catch {
        Write-Error-Message "Could not calculate checksum for $archivePath. Error: $_"
    }

    # Find expected hash
    $expectedHash = ($checksums -split "`n" | Where-Object { $_ -match $archiveName } | ForEach-Object {
        ($_ -split '\s+')[0]
    } | Select-Object -First 1)

    if ([string]::IsNullOrWhiteSpace($expectedHash)) {
        Write-Error-Message "Could not find checksum for $archiveName in $checksumsUrl"
    }

    if ($hash -ne $expectedHash.ToLower()) {
        Write-Error-Message "Checksum verification failed!`nExpected: $expectedHash`nGot: $hash"
    }

    Write-Info "Checksum verified successfully"

    Write-Info "Extracting archive..."

    # Extract archive
    try {
        # Check if tar is available (Windows 10+)
        if (Get-Command tar -ErrorAction SilentlyContinue) {
            tar -xzf $archivePath -C $tempExtractDir
        }
        else {
            Write-Error-Message "tar command not found. Please install tar or upgrade to Windows 10+"
        }
    }
    catch {
        Write-Error-Message "Failed to extract archive: $_"
    }

    # Create installation directory
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    # Find and copy binary
    $extractedDir = Join-Path $tempExtractDir "bastion-$Platform"
    $binaryPath = Join-Path $extractedDir "bastion.exe"
    $targetPath = Join-Path $InstallDir "bastion.exe"

    if (-not (Test-Path $binaryPath)) {
        Write-Error-Message "Binary not found in archive at $binaryPath"
    }

    Copy-Item -Force $binaryPath $targetPath

    # Cleanup
    Remove-Item -Recurse -Force $tempExtractDir

    # Add to PATH if not already present
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$InstallDir*") {
        Write-Info "Adding $InstallDir to PATH..."

        # Add to PATH (remove trailing semicolon if present, then add directory with semicolon)
        $newPath = $userPath.TrimEnd(';') + ";$InstallDir"
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")

        # Update current session PATH
        $env:Path += ";$InstallDir"

        Write-Info "Added to PATH"
        Write-Host ""
        Write-Warning-Message "You may need to restart your PowerShell session for PATH changes to take effect in other terminals."
        Write-Host ""
    }

    # Display version
    $installedVersion = & $targetPath --version 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Info "Installed '$installedVersion' to '$targetPath'"
    }
    else {
        Write-Info "Installed to '$targetPath'"
    }

    Write-Host ""
    Write-Info "Installation complete!"
    Write-Host ""
    Write-Host "Run 'bastion --help' to get started"
}

# Main execution
function Main {
    if ($Help) {
        Show-Help
    }

    # Set default bin directory
    if ([string]::IsNullOrEmpty($BinDir)) {
        $BinDir = Join-Path $env:LOCALAPPDATA "Programs\bastion"
    }

    # Set default temp directory
    $TempDir = $env:TEMP

    # Detect platform
    $PLATFORM = Get-Platform
    Write-Info "Detected platform: $PLATFORM"

    # Get version
    if ([string]::IsNullOrEmpty($Version)) {
        $Version = Get-LatestVersion
        Write-Info "Installing latest version: $Version"
    }
    else {
        Write-Info "Installing version: $Version"
    }

    # Install
    Install-Binary -Platform $PLATFORM -Version $Version -InstallDir $BinDir -TempDir $TempDir
}

Main
