#!/bin/sh
# build-appimage.sh — stages an AppDir from the release Linux binary and
# invokes appimagetool to produce ClawdPanel-x86_64.AppImage in ./dist.
#
# Rust/Slint rework (S12): the binary is the native Slint + winit (X11) app.
# The CascadiaMono font is already compiled into the binary by slint-build, but
# we also bundle it under usr/share/fonts (+ XDG_DATA_DIRS in AppRun) as a
# fontconfig fallback. GStreamer plugins are bundled for the media slice.
#
# Assumes:
#   * ./bin/clawdpanel exists (CI copies target/release/clawdpanel there)
#   * appimagetool is on $PATH (download from https://github.com/AppImage/AppImageKit/releases)

set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/dist}"
APPDIR="$OUT_DIR/ClawdPanel.AppDir"

mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"
mkdir -p "$APPDIR/usr/share/fonts"

cp "$ROOT/bin/clawdpanel" "$APPDIR/usr/bin/clawdpanel"
chmod +x "$APPDIR/usr/bin/clawdpanel"

cp "$ROOT/build/linux/clawdpanel.desktop" "$APPDIR/usr/share/applications/clawdpanel.desktop"
cp "$ROOT/build/linux/clawdpanel.desktop" "$APPDIR/clawdpanel.desktop"

cp "$ROOT/build/linux/icon.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/clawdpanel.png"
cp "$ROOT/build/linux/icon.png" "$APPDIR/clawdpanel.png"
ln -sf clawdpanel.png "$APPDIR/.DirIcon"

# Bundle the UI font as a fontconfig fallback (AppRun adds usr/share to
# XDG_DATA_DIRS so fontconfig scans usr/share/fonts).
if [ -f "$ROOT/crates/ui/fonts/CascadiaMono.ttf" ]; then
  echo "Bundling CascadiaMono.ttf..."
  cp "$ROOT/crates/ui/fonts/CascadiaMono.ttf" "$APPDIR/usr/share/fonts/CascadiaMono.ttf"
fi

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
