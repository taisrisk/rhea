# Releases and GitHub builds

The Windows build is made on a PC; the macOS builds come from GitHub Actions and end up on the same release.

## One command: `build.ps1`

```powershell
.\build.ps1                # patch bump (0.3.4 -> 0.3.5), build, commit, tag, push, release
.\build.ps1 -Bump minor    # 0.3.4 -> 0.4.0
.\build.ps1 -NoPush        # only bump and build
```

It needs git, cargo and the GitHub CLI signed in (`gh auth login`). In order it:

1. bumps the version in `Cargo.toml` and `Cargo.lock` (every build gets a new version),
2. builds `target\release\rhea.exe` and copies it to `dist\Rhea-Setup-vX.Y.Z-windows-x64.exe`,
3. commits everything as "Release vX.Y.Z", tags `vX.Y.Z` and pushes the commit and the tag,
4. creates the GitHub release (or reuses it) and uploads the exe.

The exe is the installer ([installer](installer.md)). `dist/` is git-ignored.

## The macOS workflow

`.github/workflows/macos.yml` builds, tests and packages Rhea for Apple Silicon and Intel on macOS runners.

- Pushing the tag (step 3 above) starts it; when both builds pass it attaches `Rhea-vX.Y.Z-macos-arm64.dmg`/`.zip` and `-x64` to that tag's release, creating the release itself if the Windows upload has not yet.
- Pushes and pull requests without a tag build and test only; the files stay as workflow artifacts for 14 days.
- It can be started by hand from the Actions tab; give it an existing release tag to attach the builds there.

A green run proves the code compiles and the tests pass on a Mac; it does not replace an interactive accessibility and permission check.

## Signing (macOS)

For a Developer ID signed and notarized build, add these repository secrets: `MACOS_CERTIFICATE` (base64 `.p12`), `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`. Without them the app is ad hoc signed: it runs, but macOS asks to right-click > Open the first time, and permissions and Keychain approvals must be granted again after each update.
