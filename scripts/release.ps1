[CmdletBinding()]
param(
    [ValidateSet("patch", "minor", "major")]
    [string]$Bump,
    [switch]$Upload
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repoRoot "Cargo.toml"
$lockPath = Join-Path $repoRoot "Cargo.lock"
$targetRoot = if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    Join-Path $repoRoot "target"
} elseif ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
    $env:CARGO_TARGET_DIR
} else {
    Join-Path $repoRoot $env:CARGO_TARGET_DIR
}

function Invoke-Checked {
    param([Parameter(Mandatory)][string]$Command, [string[]]$Arguments = @())
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command failed with exit code $LASTEXITCODE."
    }
}

function Get-VersionState {
    $manifest = [System.IO.File]::ReadAllText($manifestPath)
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
    if (-not $match.Success) {
        throw "Could not read a stable x.y.z package version from Cargo.toml."
    }
    return @{ Manifest = $manifest; Version = $match.Groups["version"].Value; Match = $match }
}

if ($env:OS -ne "Windows_NT") {
    throw "This command builds the Windows executable locally. Run it from Windows PowerShell."
}
if (-not $Bump -and -not $Upload) {
    throw "Choose -Bump patch|minor|major to update the package version, or -Upload to upload a previously tagged build."
}
if ($Upload -and $Bump) {
    throw "Bump and upload are separate steps. Review and commit the version bump, create and push its tag, then run this command with -Upload."
}

Push-Location $repoRoot
try {
    $versionState = Get-VersionState
    $version = $versionState.Version

    if ($Bump) {
        $parts = @($version.Split('.') | ForEach-Object { [int]$_ })
        switch ($Bump) {
            "major" { $parts[0]++; $parts[1] = 0; $parts[2] = 0 }
            "minor" { $parts[1]++; $parts[2] = 0 }
            "patch" { $parts[2]++ }
        }
        $nextVersion = "$($parts[0]).$($parts[1]).$($parts[2])"
        $updatedManifest = $versionState.Manifest.Substring(0, $versionState.Match.Index) +
            $versionState.Manifest.Substring($versionState.Match.Index, $versionState.Match.Length).Replace($version, $nextVersion) +
            $versionState.Manifest.Substring($versionState.Match.Index + $versionState.Match.Length)

        $lock = [System.IO.File]::ReadAllText($lockPath)
        $lockPattern = '(?m)(\[\[package\]\]\r?\nname = "jarvus"\r?\nversion = ")\d+\.\d+\.\d+("\r?\n)'
        $lockMatch = [regex]::Match($lock, $lockPattern)
        if (-not $lockMatch.Success) {
            throw "Could not locate JARVUS's package entry in Cargo.lock; no version files were changed."
        }
        $updatedLock = ([regex]::new($lockPattern)).Replace($lock, "`${1}$nextVersion`${2}", 1)

        # Write temporary files before replacing either version file.
        # Cargo.lock's root-package version is metadata; dependency versions stay locked.
        $manifestTemp = "$manifestPath.tmp"
        $lockTemp = "$lockPath.tmp"
        [System.IO.File]::WriteAllText($manifestTemp, $updatedManifest, [System.Text.UTF8Encoding]::new($false))
        [System.IO.File]::WriteAllText($lockTemp, $updatedLock, [System.Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $manifestTemp -Destination $manifestPath -Force
        Move-Item -LiteralPath $lockTemp -Destination $lockPath -Force
        $version = $nextVersion
        Write-Host "Updated Cargo.toml and Cargo.lock to JARVUS $version. Review and commit the version bump before creating a release tag."
    }

    if ($Upload) {
        if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
            throw "GitHub CLI (gh) is required for -Upload. Install it and sign in with gh auth login."
        }
        $tag = "v$version"
        $origin = & git remote get-url origin 2>$null
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace(($origin -join ""))) {
            throw "No GitHub origin is configured. Add the repository remote before using -Upload."
        }
        if (($origin -join "") -notmatch '^https://github\.com/|^git@github\.com:') {
            throw "The origin remote is not a GitHub URL; refusing to upload a GitHub Release asset."
        }
        Invoke-Checked "gh" @("auth", "status")
        $headTag = & git describe --tags --exact-match HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or ($headTag -join "").Trim() -ne $tag) {
            throw "HEAD must be tagged $tag before upload. This script does not create or push Git tags."
        }
        Invoke-Checked "gh" @("release", "view", $tag)
    }

    Invoke-Checked "cargo" @("build", "--release", "--locked", "--bin", "jarvus")

    $exePath = Join-Path $targetRoot "release\jarvus.exe"
    if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
        throw "Build succeeded but the expected Windows executable was not created: $exePath"
    }
    $distPath = Join-Path $repoRoot "dist"
    New-Item -ItemType Directory -Path $distPath -Force | Out-Null
    $assetPath = Join-Path $distPath "JARVUS-v$version-windows-x64.exe"
    Copy-Item -LiteralPath $exePath -Destination $assetPath -Force
    Write-Host "Built $assetPath"

    if ($Upload) {
        Invoke-Checked "gh" @("release", "upload", "v$version", $assetPath, "--clobber")
        Write-Host "Uploaded the Windows executable to the existing GitHub release v$version."
    }
}
finally {
    Pop-Location
}
