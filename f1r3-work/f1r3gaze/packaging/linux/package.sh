#!/usr/bin/env bash
# Build the Linux release artifacts from already-built binaries:
#   dist/f1r3gaze_<ver>_<arch>.deb
#   dist/F1R3Gaze-<ver>-<arch>.AppImage     (needs appimagetool on PATH or APPIMAGETOOL=)
#   dist/f1r3gaze-<ver>-linux-<arch>.tar.gz
# Usage: packaging/linux/package.sh <version> <bin dir holding f1r3gaze and f1r3c>
set -euo pipefail
VER="${1:?version}"; BIN="${2:?bin dir}"
HERE="$(cd "$(dirname "$0")" && pwd)"; ICONS="$HERE/../icons"
ARCH="$(uname -m)"; DEBARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
OUT="${DIST:-dist}"; mkdir -p "$OUT"; OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

stage() { # $1 = root prefix (e.g. $WORK/deb/usr)
  install -Dm755 "$BIN/f1r3gaze" "$1/bin/f1r3gaze"
  install -Dm755 "$BIN/f1r3c" "$1/bin/f1r3c"
  install -Dm644 "$HERE/f1r3gaze.desktop" "$1/share/applications/f1r3gaze.desktop"
  for s in 16 32 48 64 128 256 512; do
    install -Dm644 "$ICONS/$s.png" "$1/share/icons/hicolor/${s}x${s}/apps/f1r3gaze.png"
  done
  install -Dm644 "$ICONS/f1r3gaze.svg" "$1/share/icons/hicolor/scalable/apps/f1r3gaze.svg"
}

# --- .deb ------------------------------------------------------------------
D="$WORK/deb"; stage "$D/usr"
SIZE="$(du -sk "$D/usr" | cut -f1)"
mkdir -p "$D/DEBIAN"
cat > "$D/DEBIAN/control" <<CTRL
Package: f1r3gaze
Version: $VER
Section: web
Priority: optional
Architecture: $DEBARCH
Installed-Size: $SIZE
Depends: libc6 (>= 2.35), libxkbcommon0, libvulkan1 | libgl1
Recommends: libxkbcommon-x11-0, mesa-vulkan-drivers
Maintainer: F1R3FLY.io <engineering@f1r3fly.io>
Homepage: https://github.com/F1R3FLY-io/F1R3Gaze
Description: browser whose only execution mechanism is f1r3lang
 F1R3Gaze renders HTML and CSS with Blitz and runs page behaviour as
 f1r3lang on a native RSpace. It never executes JavaScript. Pages hold
 capabilities, not ambient authority; shard sites resolve and verify
 through the F1R3FLY shard bridge.
CTRL
cat > "$D/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
POST
chmod 755 "$D/DEBIAN/postinst"
dpkg-deb --root-owner-group --build "$D" "$OUT/f1r3gaze_${VER}_${DEBARCH}.deb" >/dev/null
echo "built $OUT/f1r3gaze_${VER}_${DEBARCH}.deb"

# --- tarball -----------------------------------------------------------------
T="$WORK/f1r3gaze-$VER"; stage "$T"
tar -C "$WORK" -czf "$OUT/f1r3gaze-$VER-linux-$ARCH.tar.gz" "f1r3gaze-$VER"
echo "built $OUT/f1r3gaze-$VER-linux-$ARCH.tar.gz"

# --- AppImage ----------------------------------------------------------------
TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [ -n "$TOOL" ]; then
  A="$WORK/F1R3Gaze.AppDir"; stage "$A/usr"
  cp "$HERE/f1r3gaze.desktop" "$A/f1r3gaze.desktop"
  cp "$ICONS/256.png" "$A/f1r3gaze.png"
  cat > "$A/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/f1r3gaze" "$@"
RUN
  chmod 755 "$A/AppRun"
  ARCH="$ARCH" "$TOOL" --no-appstream "$A" "$OUT/F1R3Gaze-$VER-$ARCH.AppImage" >/dev/null
  echo "built $OUT/F1R3Gaze-$VER-$ARCH.AppImage"
else
  echo "appimagetool not found: skipping the AppImage" >&2
fi
