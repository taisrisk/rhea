<#
.SYNOPSIS
  Build Rhea for Windows and publish it: one command from change to release.

.DESCRIPTION
  1. bumps the version in Cargo.toml / Cargo.lock (patch by default; every build is a new version),
  2. builds the release exe,
  3. commits everything with the message "Release vX.Y.Z", tags it and pushes the commit and tag,
  4. creates (or reuses) the GitHub release vX.Y.Z and uploads dist\Rhea-vX.Y.Z-windows-x64.exe.

  Pushing the tag also starts the "macOS build" workflow on GitHub, which builds both Mac versions
  and attaches the .dmg/.zip files to the same release. The exe you upload is the installer: when it
  is run it installs itself to %LOCALAPPDATA%\Programs\Rhea, replacing an older install.

  Needs git, cargo and the GitHub CLI signed in (gh auth login).

.EXAMPLE
  .\build.ps1                 # patch bump, build, commit, tag, push, release
  .\build.ps1 -Bump minor     # 0.3.4 -> 0.4.0
  .\build.ps1 -NoPush         # only bump and build (nothing is committed or published)
#>
[CmdletBinding()]
param(
    [ValidateSet("patch", "minor", "major")]
    [string]$Bump = "patch",
    [switch]$NoPush
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

function Invoke-Checked {
    param([Parameter(Mandatory)][string]$Command, [string[]]$Arguments = @())
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Command $($Arguments -join ' ') failed with exit code $LASTEXITCODE." }
}

if ($env:OS -ne "Windows_NT") { throw "build.ps1 builds the Windows exe. Run it from Windows PowerShell." }
foreach ($tool in @("git", "cargo") + $(if ($NoPush) { @() } else { @("gh") })) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool was not found on PATH." }
}
if (-not $NoPush) { Invoke-Checked "gh" @("auth", "status") }

# ---- version ---------------------------------------------------------------------------
$manifestPath = Join-Path $PSScriptRoot "Cargo.toml"
$lockPath = Join-Path $PSScriptRoot "Cargo.lock"
$manifest = [IO.File]::ReadAllText($manifestPath)
$match = [regex]::Match($manifest, '(?m)^version\s*=\s*"(?<v>\d+\.\d+\.\d+)"\s*$')
if (-not $match.Success) { throw "Could not read the version from Cargo.toml." }
$parts = @($match.Groups["v"].Value.Split('.') | ForEach-Object { [int]$_ })
switch ($Bump) {
    "major" { $parts[0]++; $parts[1] = 0; $parts[2] = 0 }
    "minor" { $parts[1]++; $parts[2] = 0 }
    "patch" { $parts[2]++ }
}
$version = "$($parts[0]).$($parts[1]).$($parts[2])"
$tag = "v$version"
if (-not $NoPush) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & git rev-parse -q --verify "refs/tags/$tag" *> $null
    $tagExists = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = $previous
    if ($tagExists) { throw "The tag $tag already exists. Pick a larger -Bump or delete the tag." }
}
$utf8 = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText($manifestPath, $manifest.Remove($match.Index, $match.Length).Insert($match.Index, "version = `"$version`""), $utf8)
$lock = [IO.File]::ReadAllText($lockPath)
$lockPattern = '(?m)(\[\[package\]\]\r?\nname = "rhea"\r?\nversion = ")\d+\.\d+\.\d+(")'
if (-not [regex]::IsMatch($lock, $lockPattern)) { throw "Could not find Rhea's entry in Cargo.lock." }
[IO.File]::WriteAllText($lockPath, [regex]::new($lockPattern).Replace($lock, "`${1}$version`${2}", 1), $utf8)
Write-Host "Version $version" -ForegroundColor Green

# ---- build -----------------------------------------------------------------------------
Invoke-Checked "cargo" @("build", "--release", "--locked", "--bin", "rhea")
# Ask cargo where the build went: CARGO_TARGET_DIR or a .cargo/config.toml can move it.
$metadata = (& cargo metadata --format-version 1 --no-deps --locked) -join "`n" | ConvertFrom-Json
$targetRoot = $metadata.target_directory
$built = Join-Path $targetRoot "release\rhea.exe"
if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { throw "Build finished but $built does not exist." }
$dist = Join-Path $PSScriptRoot "dist"
New-Item -ItemType Directory -Path $dist -Force | Out-Null
$asset = Join-Path $dist "Rhea-$tag-windows-x64.exe"
Copy-Item -LiteralPath $built -Destination $asset -Force
Write-Host "Built $asset" -ForegroundColor Green

if ($NoPush) { Write-Host "Not publishing (-NoPush). Version files were changed; commit them yourself or revert."; return }

# ---- publish ---------------------------------------------------------------------------
Invoke-Checked "git" @("add", "-A")
& git diff --cached --stat
Invoke-Checked "git" @("commit", "-m", "Release $tag")
Invoke-Checked "git" @("tag", "-a", $tag, "-m", "Rhea $version")
$branch = (& git rev-parse --abbrev-ref HEAD).Trim()
Invoke-Checked "git" @("push", "origin", $branch)
Invoke-Checked "git" @("push", "origin", $tag)   # starts the macOS build on GitHub

# gh prints "release not found" on stderr; Windows PowerShell would turn that into a terminating
# error under "Stop", so these probing calls run with errors set aside and the exit code is read.
function Test-Release {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try { & gh release view $tag *> $null; return ($LASTEXITCODE -eq 0) } finally { $ErrorActionPreference = $previous }
}
if (-not (Test-Release)) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try { & gh release create $tag --title "Rhea $tag" --generate-notes *> $null } finally { $ErrorActionPreference = $previous }
    # the macOS workflow may have created it a moment earlier
    if (-not (Test-Release)) { throw "Could not create the release $tag." }
}
Invoke-Checked "gh" @("release", "upload", $tag, $asset, "--clobber")
Write-Host "Published $tag with the Windows installer. The macOS files are added by GitHub Actions when its build finishes." -ForegroundColor Green
