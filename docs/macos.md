# macOS

- macOS 13 or newer, Intel or Apple Silicon.
- **Using a release:** download `Rhea-v<version>-macos-arm64.dmg` (Apple Silicon) or `-x64.dmg` (Intel) from the GitHub release, drag **Rhea** to Applications, and the first time right-click it and choose **Open** (the build is ad hoc signed unless a Developer ID certificate was supplied). The accessibility engine (Agent Desktop) is bundled inside the app; no Node.js is needed.
- **Permissions:** switch Rhea on under System Settings > Privacy & Security > **Accessibility** (clicking, typing, key presses) and **Screen Recording** (only for screenshots). Without Accessibility, macOS drops synthetic input silently; Rhea checks and tells the model which pane to open. An ad hoc signed update is a new app identity to macOS, so the switch may need turning off and on again after updating, and the Keychain asks again: click **Always Allow**. A Developer ID signature (see [releases](releases.md)) makes both stick.
- **From source:** Xcode Command Line Tools (`xcode-select --install`), Rust, and for development Node.js 18+ with `npm install --global agent-desktop`.

## How it works on a Mac

Pointer, scroll and keyboard actions are posted natively as CoreGraphics events. The front app and window are read through System Events, apps are launched with `open -a`, and reading another app's accessibility tree and acting on its controls goes through Agent Desktop, which wraps the macOS Accessibility (AX) API. The shell tool defaults to `/bin/zsh -lc`. Rhea uses a macOS edition of its operating manual (`src/system_prompt_macos.md`) with Homebrew, `open`, `osascript`, `defaults`, `launchctl`, `pbcopy` and Cmd-based shortcut examples.

Icons are drawn from the bundled Phosphor font by family name, so they do not depend on which system fonts a Mac has.

## Packaging

`packaging/macos/package.sh <rust-target> <arm64|x64>` turns a release build into `Rhea.app`, a `.zip` and a `.dmg` under `dist/`. GitHub Actions does this on macOS runners for both architectures; see [releases](releases.md).
