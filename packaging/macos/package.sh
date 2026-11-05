#!/usr/bin/env bash
# Build JARVUS.app, a .zip and a .dmg for one macOS architecture from a finished release build.
#
#   packaging/macos/package.sh <rust-target> <arm64|x64> [version]
#
# Needs (all present on GitHub's macOS runners): cargo output in target/<rust-target>/release,
# sips, iconutil, codesign, hdiutil, ditto, and npm (to fetch the Agent Desktop binaries).
#
# Optional signing and notarization, used only when these environment variables are set:
#   MACOS_CERTIFICATE (base64 .p12) + MACOS_CERTIFICATE_PASSWORD + MACOS_SIGN_IDENTITY
#   APPLE_ID + APPLE_TEAM_ID + APPLE_APP_PASSWORD   (notarize the .dmg)
# Without them the app is signed ad hoc: it runs, but macOS asks the user to right-click > Open the
# first time, and the Accessibility permission has to be granted again after each update.
set -euo pipefail

target="${1:?usage: package.sh <rust-target> <arm64|x64> [version]}"
arch="${2:?usage: package.sh <rust-target> <arm64|x64> [version]}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
version="${3:-$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' Cargo.toml | head -n 1)}"
test -n "$version" || { echo "could not read the version from Cargo.toml" >&2; exit 1; }
agent_desktop_version="${AGENT_DESKTOP_VERSION:-1.2.0}"

binary="target/${target}/release/jarvus"
test -x "$binary" || { echo "missing $binary - run cargo build --release --locked --bin jarvus --target $target first" >&2; exit 1; }

name="JARVUS-v${version}-macos-${arch}"
dist="$root/dist"
app="$dist/stage/JARVUS.app"
rm -rf "$dist/stage" "$dist/$name.zip" "$dist/$name.dmg" "$dist/jarvus.iconset"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$dist"

echo "==> app bundle"
cp "$binary" "$app/Contents/MacOS/jarvus"
chmod +x "$app/Contents/MacOS/jarvus"
sed "s/@VERSION@/${version}/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp packaging/macos/THIRD_PARTY.txt "$app/Contents/Resources/THIRD_PARTY.txt"
cp README.md "$app/Contents/Resources/README.md"
cp assets/jarvus-mark.png "$app/Contents/Resources/jarvus-mark.png"

echo "==> icon"
iconset="$dist/jarvus.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" assets/jarvus-icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" assets/jarvus-icon-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/jarvus.icns"
rm -rf "$iconset"

echo "==> Agent Desktop ${agent_desktop_version} (accessibility engine)"
tmp="$(mktemp -d)"
(cd "$tmp" && npm pack "agent-desktop@${agent_desktop_version}" --silent >/dev/null && tar -xzf agent-desktop-*.tgz)
cp "$tmp/package/bin/agent-desktop-darwin-${arch}" "$app/Contents/Resources/agent-desktop"
cp "$tmp/package/bin/agent-desktop-macos-helper-darwin-${arch}" "$app/Contents/Resources/agent-desktop-macos-helper"
chmod +x "$app/Contents/Resources/agent-desktop" "$app/Contents/Resources/agent-desktop-macos-helper"
rm -rf "$tmp"
# The runner matches the target architecture, so the bundled CLI can be exercised here.
"$app/Contents/Resources/agent-desktop" version

echo "==> sign"
identity="-"
keychain=""
if [[ -n "${MACOS_CERTIFICATE:-}" && -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
  keychain="$RUNNER_TEMP/jarvus-signing.keychain-db"
  security create-keychain -p "ci" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "ci" "$keychain"
  echo "$MACOS_CERTIFICATE" | base64 --decode > "$RUNNER_TEMP/certificate.p12"
  security import "$RUNNER_TEMP/certificate.p12" -P "${MACOS_CERTIFICATE_PASSWORD:-}" -A -t cert -f pkcs12 -k "$keychain"
  security set-key-partition-list -S apple-tool:,apple: -k "ci" "$keychain" >/dev/null
  security list-keychains -d user -s "$keychain" login.keychain
  identity="$MACOS_SIGN_IDENTITY"
fi
for helper in "$app/Contents/Resources/agent-desktop" "$app/Contents/Resources/agent-desktop-macos-helper"; do
  codesign --force --options runtime --timestamp=none --sign "$identity" "$helper" || codesign --force --sign "$identity" "$helper"
done
codesign --force --deep --options runtime --entitlements packaging/macos/entitlements.plist --sign "$identity" "$app" \
  || codesign --force --deep --sign "$identity" "$app"
codesign --verify --deep --strict "$app"
codesign -dv "$app" 2>&1 | head -n 6

echo "==> zip and dmg"
ditto -c -k --keepParent "$app" "$dist/$name.zip"
mkdir -p "$dist/stage/dmg"
cp -R "$app" "$dist/stage/dmg/JARVUS.app"
ln -s /Applications "$dist/stage/dmg/Applications"
hdiutil create -volname "JARVUS ${version}" -srcfolder "$dist/stage/dmg" -ov -format UDZO "$dist/$name.dmg" >/dev/null

if [[ -n "${APPLE_ID:-}" && -n "${APPLE_TEAM_ID:-}" && -n "${APPLE_APP_PASSWORD:-}" && "$identity" != "-" ]]; then
  echo "==> notarize"
  xcrun notarytool submit "$dist/$name.dmg" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" --wait
  xcrun stapler staple "$dist/$name.dmg"
fi

rm -rf "$dist/stage"
[[ -n "$keychain" ]] && security delete-keychain "$keychain" || true
ls -lh "$dist/$name.zip" "$dist/$name.dmg"
