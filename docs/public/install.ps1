#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Installs Rift from checksummed GitHub Release archive.

.PARAMETER Version
    Installs exact vX.Y.Z release. Defaults to RIFT_VERSION or latest.

.PARAMETER Help
    Prints command usage.

.EXAMPLE
    irm https://volar.sh/rift/install.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((irm https://volar.sh/rift/install.ps1))) -Version v1.2.3
#>

[CmdletBinding()]
param(
    [string]$Version = $(if ($env:RIFT_VERSION) { $env:RIFT_VERSION } else { "latest" }),
    [switch]$Help
)

if ($Help) {
    Write-Host @'
Usage: install.ps1 [-Version vX.Y.Z]

Options:
  -Version vX.Y.Z  Install exact release instead of latest.
  -Help             Print this help.

Environment:
  RIFT_VERSION      Exact release used when -Version is absent.
  RIFT_INSTALL_DIR  Destination directory for Rift binary.
'@
    return
}

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Repository = if ($env:RIFT_REPOSITORY) { $env:RIFT_REPOSITORY } else { "volarized/rift" }
$GitHubApi = if ($env:RIFT_GITHUB_API) { $env:RIFT_GITHUB_API } else { "https://api.github.com" }
$DownloadBase = if ($env:RIFT_DOWNLOAD_BASE) { $env:RIFT_DOWNLOAD_BASE } else { "https://github.com/$Repository/releases/download" }
$RunningOnWindows = $PSVersionTable.PSEdition -eq "Desktop" -or [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)
$RunningOnMacOS = -not $RunningOnWindows -and [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)
$RunningOnLinux = -not $RunningOnWindows -and [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Linux)
$InstallDir = if ($env:RIFT_INSTALL_DIR) {
    $env:RIFT_INSTALL_DIR
} elseif ($RunningOnWindows) {
    Join-Path $env:LOCALAPPDATA "Rift/bin"
} else {
    Join-Path $HOME ".rift/bin"
}

function Stop-Install {
    param([string]$Message)
    throw "error: $Message"
}

function Assert-HttpsUri {
    param([uri]$Uri)
    if ($Uri.Scheme -ne "https") {
        Stop-Install "refusing non-HTTPS URL: $Uri"
    }
}

function Invoke-Fetch {
    param(
        [uri]$Uri,
        [string]$OutFile
    )

    Assert-HttpsUri $Uri
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $handler.SslProtocols = [System.Security.Authentication.SslProtocols]::Tls12
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(60)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd("rift-installer")
    $response = $null
    $input = $null
    $output = $null
    try {
        $current = $Uri
        for ($redirects = 0; ; $redirects++) {
            Assert-HttpsUri $current
            $response = $client.GetAsync($current, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            $status = [int]$response.StatusCode
            if ($status -lt 300 -or $status -ge 400) { break }
            if ($redirects -ge 5) { Stop-Install "download exceeded five redirects: $Uri" }
            $location = $response.Headers.Location
            if (-not $location) { Stop-Install "redirect has no location: $current" }
            $next = if ($location.IsAbsoluteUri) { $location } else { [uri]::new($current, $location) }
            Assert-HttpsUri $next
            $response.Dispose()
            $response = $null
            $current = $next
        }
        if (-not $response.IsSuccessStatusCode) {
            Stop-Install "download failed with HTTP $([int]$response.StatusCode): $Uri"
        }
        $input = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $output = [System.IO.File]::Create($OutFile)
        $input.CopyTo($output)
    } finally {
        if ($output) { $output.Dispose() }
        if ($input) { $input.Dispose() }
        if ($response) { $response.Dispose() }
        $client.Dispose()
        $handler.Dispose()
    }
}

function Test-Version {
    param([string]$Value)
    return $Value -match '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

function Resolve-Version {
    param([string]$Requested)

    if ($Requested -ne "latest") {
        if (-not (Test-Version $Requested)) {
            Stop-Install "version must match vX.Y.Z: $Requested"
        }
        return $Requested
    }

    $temporary = [System.IO.Path]::GetTempFileName()
    try {
        Invoke-Fetch -Uri "$($GitHubApi.TrimEnd('/'))/repos/$Repository/releases/latest" -OutFile $temporary
        $release = Get-Content -Raw $temporary | ConvertFrom-Json
        $tag = [string]$release.tag_name
        if (-not (Test-Version $tag)) {
            Stop-Install "latest release returned invalid tag: $(if ($tag) { $tag } else { 'missing' })"
        }
        return $tag
    } finally {
        Remove-Item $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Get-Target {
    $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    $arch = switch ($architecture) {
        "X64" { "x86_64" }
        "Arm64" { "aarch64" }
        default { Stop-Install "unsupported architecture: $architecture" }
    }

    $platform = if ($RunningOnWindows) {
        "pc-windows-msvc"
    } elseif ($RunningOnMacOS) {
        "apple-darwin"
    } elseif ($RunningOnLinux) {
        "unknown-linux-gnu"
    } else {
        Stop-Install "unsupported operating system"
    }
    return "$arch-$platform"
}

function Confirm-Checksum {
    param(
        [string]$Archive,
        [string]$Manifest
    )

    $name = [System.IO.Path]::GetFileName($Archive)
    $pattern = "^(?<hash>[0-9a-fA-F]{64})  $([regex]::Escape($name))$"
    $entries = @(Get-Content $Manifest | Where-Object { $_ -match $pattern })
    if ($entries.Count -ne 1) {
        Stop-Install "checksum manifest has no unique entry for $name"
    }
    $null = $entries[0] -match $pattern
    $expected = $Matches.hash.ToLowerInvariant()
    $actual = (Get-FileHash -Path $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        Stop-Install "checksum mismatch for $name"
    }
}

function Expand-CheckedArchive {
    param(
        [string]$Archive,
        [string]$Root,
        [string]$Destination
    )

    if ($RunningOnWindows) {
        Expand-Archive -Path $Archive -DestinationPath $Destination
        $files = @(
            Get-ChildItem -Path $Destination -File -Recurse |
                ForEach-Object {
                    $_.FullName.Substring($Destination.Length + 1).Replace('\', '/')
                } |
                Sort-Object
        )
        $expected = @("$Root/LICENSE.md", "$Root/README.md", "$Root/rift.exe") | Sort-Object
        if (Compare-Object $files $expected) {
            Stop-Install "archive contains unexpected files"
        }
        return Join-Path $Destination "$Root/rift.exe"
    }

    $listing = @(& tar -tzf $Archive)
    $expected = @("$Root/rift", "$Root/README.md", "$Root/LICENSE.md")
    if (Compare-Object $listing $expected -SyncWindow 0) {
        Stop-Install "archive contains unexpected files"
    }
    & tar -xzf $Archive -C $Destination "$Root/rift"
    if ($LASTEXITCODE -ne 0) {
        Stop-Install "could not extract Rift binary"
    }
    return Join-Path $Destination "$Root/rift"
}

function Main {
    $resolvedVersion = Resolve-Version $Version
    $target = Get-Target
    $root = "rift-$resolvedVersion-$target"
    $extension = if ($RunningOnWindows) { "zip" } else { "tar.gz" }
    $archiveName = "$root.$extension"
    $checksumName = "rift-$resolvedVersion-checksums.sha256"
    $base = "$($DownloadBase.TrimEnd('/'))/$resolvedVersion"
    $workDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    $candidate = $null

    New-Item -ItemType Directory -Path $workDir | Out-Null
    try {
        $archive = Join-Path $workDir $archiveName
        $manifest = Join-Path $workDir $checksumName
        Invoke-Fetch -Uri "$base/$archiveName" -OutFile $archive
        Invoke-Fetch -Uri "$base/$checksumName" -OutFile $manifest
        Confirm-Checksum -Archive $archive -Manifest $manifest

        $extracted = Join-Path $workDir "extracted"
        New-Item -ItemType Directory -Path $extracted | Out-Null
        $binary = Expand-CheckedArchive -Archive $archive -Root $root -Destination $extracted

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        $installedName = if ($RunningOnWindows) { "rift.exe" } else { "rift" }
        $destination = Join-Path $InstallDir $installedName
        $candidate = Join-Path $InstallDir ".rift.$PID"
        Copy-Item -Path $binary -Destination $candidate -Force
        if (-not $RunningOnWindows) { & chmod 0755 $candidate }
        Move-Item -Path $candidate -Destination $destination -Force
        $candidate = $null

        Write-Host "Installed Rift $resolvedVersion to $destination"
        if (($env:PATH -split [System.IO.Path]::PathSeparator) -notcontains $InstallDir) {
            Write-Host "Add $InstallDir to PATH."
        }
    } finally {
        if ($candidate) { Remove-Item $candidate -Force -ErrorAction SilentlyContinue }
        Remove-Item $workDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    Main
}
