#!/bin/sh
# build-appimage.sh — stages an AppDir from the Wails-built Linux binary and
# invokes appimagetool to produce ClawdPanel-x86_64.AppImage in ./dist.
#
# Assumes:
#   * build/bin/clawdpanel exists (run `wails build -platform linux/amd64` first)
#   * appimagetool is on $PATH (download from https://github.com/AppImage/AppImageKit/releases)

set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/dist}"
APPDIR="$OUT_DIR/ClawdPanel.AppDir"

mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp "$ROOT/bin/clawdpanel" "$APPDIR/usr/bin/clawdpanel"
chmod +x "$APPDIR/usr/bin/clawdpanel"

cp "$ROOT/build/linux/clawdpanel.desktop" "$APPDIR/usr/share/applications/clawdpanel.desktop"
cp "$ROOT/build/linux/clawdpanel.desktop" "$APPDIR/clawdpanel.desktop"

cp "$ROOT/build/linux/icon.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/clawdpanel.png"
cp "$ROOT/build/linux/icon.png" "$APPDIR/clawdpanel.png"
ln -sf clawdpanel.png "$APPDIR/.DirIcon"

# Bundle GStreamer plugins
mkdir -p "$APPDIR/usr/lib/gstreamer-1.0"
if [ -d "/usr/lib/x86_64-linux-gnu/gstreamer-1.0" ]; then
  echo "Bundling GStreamer plugins..."
  cp /usr/lib/x86_64-linux-gnu/gstreamer-1.0/*.so "$APPDIR/usr/lib/gstreamer-1.0/"
fi

cp "$ROOT/build/linux/AppRun" "$APPDIR/AppRun"
chmod +x "$APPDIR/AppRun"

ARCH=x86_64 appimagetool "$APPDIR" "$OUT_DIR/ClawdPanel-x86_64.AppImage"

# Remove the staging tree so OUT_DIR only contains the AppImage. Without
# this the AppDir and all its bundled GStreamer plugins / AppRun / icon /
# .desktop survive into dist/, the CI artifact upload sweeps them all up,
# and the release-publish step flattens them into individual top-level
# release assets ("junk" alongside the real installers).
rm -rf "$APPDIR"

echo "Built $OUT_DIR/ClawdPanel-x86_64.AppImage"
