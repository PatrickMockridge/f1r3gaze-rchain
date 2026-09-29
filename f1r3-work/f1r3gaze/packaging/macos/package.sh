#!/usr/bin/env bash
# Build dist/F1R3Gaze-<ver>-macos-universal.dmg: a universal app bundle,
# signed with the hardened runtime, in a signed, notarised, stapled DMG.
# Usage: packaging/macos/package.sh <version> <arm64 bin dir> <x86_64 bin dir>
# Signing env (all optional; without them the DMG is unsigned):
#   MACOS_CERT_P12      base64 of a "Developer ID Application" .p12
#   MACOS_CERT_PASSWORD its password
#   MACOS_SIGN_IDENTITY e.g. "Developer ID Application: F1R3FLY.io (TEAMID)"
# Notarisation env (optional): APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD
set -euo pipefail
VER="${1:?version}"; ARM="${2:?arm64 bin dir}"; X86="${3:?x86_64 bin dir}"
HERE="$(cd "$(dirname "$0")" && pwd)"; ICONS="$HERE/../icons"
OUT="${DIST:-dist}"; mkdir -p "$OUT"; OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"; [ -n "${KC:-}" ] && security delete-keychain "$KC" 2>/dev/null || true' EXIT

APP="$WORK/stage/F1R3Gaze.app"; C="$APP/Contents"
mkdir -p "$C/MacOS" "$C/Resources"
lipo -create "$ARM/f1r3gaze" "$X86/f1r3gaze" -output "$C/MacOS/f1r3gaze"
lipo -create "$ARM/f1r3c" "$X86/f1r3c" -output "$C/MacOS/f1r3c"
sed "s/__VERSION__/$VER/g" "$HERE/Info.plist" > "$C/Info.plist"

IS="$WORK/f1r3gaze.iconset"; mkdir -p "$IS"
for s in 16 32 128 256 512; do
  cp "$ICONS/$s.png" "$IS/icon_${s}x${s}.png"
  cp "$ICONS/$((s * 2)).png" "$IS/icon_${s}x${s}@2x.png" 2>/dev/null || cp "$ICONS/1024.png" "$IS/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$IS" -o "$C/Resources/f1r3gaze.icns"

SIGN=""
if [ -n "${MACOS_CERT_P12:-}" ]; then
  KC="$WORK/build.keychain-db"; KP="$(uuidgen)"
  security create-keychain -p "$KP" "$KC"
  security set-keychain-settings -lut 21600 "$KC"
  security unlock-keychain -p "$KP" "$KC"
  echo "$MACOS_CERT_P12" | base64 --decode > "$WORK/cert.p12"
  security import "$WORK/cert.p12" -k "$KC" -P "${MACOS_CERT_PASSWORD:-}" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple: -s -k "$KP" "$KC" >/dev/null
  security list-keychains -d user -s "$KC" $(security list-keychains -d user | tr -d '"')
  SIGN="${MACOS_SIGN_IDENTITY:?MACOS_SIGN_IDENTITY must name the certificate}"
  cs() { codesign --force --timestamp --options runtime --entitlements "$HERE/entitlements.plist" --sign "$SIGN" "$@"; }
  cs "$C/MacOS/f1r3c"
  cs "$C/MacOS/f1r3gaze"
  cs "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
else
  echo "MACOS_CERT_P12 not set: the app is unsigned" >&2
fi

ln -s /Applications "$WORK/stage/Applications"
DMG="$OUT/F1R3Gaze-$VER-macos-universal.dmg"
hdiutil create -volname "F1R3Gaze $VER" -srcfolder "$WORK/stage" -ov -format UDZO "$DMG" >/dev/null
[ -n "$SIGN" ] && codesign --force --timestamp --sign "$SIGN" "$DMG"

if [ -n "$SIGN" ] && [ -n "${APPLE_ID:-}" ]; then
  xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" --wait
  xcrun stapler staple "$DMG"
  spctl --assess --type open --context context:primary-signature --verbose "$DMG"
else
  echo "notarisation skipped (needs signing and APPLE_ID)" >&2
fi
echo "built $DMG"
