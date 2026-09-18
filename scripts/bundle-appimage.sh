#!/usr/bin/env bash
#
# Builds a Linux AppImage from the current source tree (including uncommitted
# work — no clean checkout). Reuses the same release binaries as
# scripts/bundle-linux.sh, then wraps the install prefix in an AppDir and
# drives it through linuxdeploy + appimagetool.
#
# Usage:
#   ./scripts/bundle-appimage.sh
#
# Environment overrides:
#   MACK_APPIMAGE_PROFILE   cargo profile to bundle (default: release)
#   MACK_LINUXDEPLOY_URL    linuxdeploy AppImage URL (arch default below)
#   MACK_APPIMAGETOOL_URL   appimagetool AppImage URL (arch default below)
#
# Output:
#   target/<profile>/Mack-<version>-<arch>.AppImage (+ .sha256 sidecar)
#
# Notes:
# - The AppImage intentionally omits share/mack/self-update-v1, so the in-app
#   updater (src/updater/linux.rs) stays dormant: a squashfs mount cannot be
#   swapped in place. Updates mean downloading a new AppImage.
# - System graphics drivers, Mesa, and xdg-desktop-portal stay on the host;
#   linuxdeploy bundles the loader/client libraries (Wayland, X11, xkbcommon,
#   fontconfig, Vulkan loader) only.
# - Built on Ubuntu 22.04 in CI, so the glibc baseline stays 2.35 like the
#   tarball releases.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

profile="${MACK_APPIMAGE_PROFILE:-release}"
target_dir="${CARGO_TARGET_DIR:-target}"
# The workspace package keeps its historical `waku` name; the product
# binaries ship under the Mack name.
version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"waku","version":"\([^"]*\)".*/\1/p')"
if [ -z "${version:-}" ]; then
  echo "Could not determine version from Cargo.toml" >&2
  exit 1
fi
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
case "$target_triple" in
  x86_64-unknown-linux-gnu) arch="x86_64" ;;
  aarch64-unknown-linux-gnu) arch="aarch64" ;;
  *)
    echo "Unsupported host for AppImage: $target_triple" >&2
    exit 1
    ;;
esac

out="$target_dir/$profile/Mack-${version}-${arch}.AppImage"

cargo build --locked "--$profile" \
  --package waku --bin mack --bin mack-updater --bin mack_js_repl \
  --package waku-daemon --bin mack-daemon \
  --package waku-computer-use --bin mack_computer_use

# Assemble the install prefix first (same content as bundle-linux.sh, minus
# the managed-install marker which must NOT ship in a read-only AppImage).
staging="$(mktemp -d)"
trap 'rm -rf -- "$staging"' EXIT
prefix="$staging/prefix"
mkdir -p "$prefix"
bun scripts/cua-driver.ts bundle "$prefix/bin" "$prefix/share/mack" "$profile"
install -Dm755 "$target_dir/$profile/mack" "$prefix/bin/mack"
install -Dm755 "$target_dir/$profile/mack-updater" "$prefix/bin/mack-updater"
install -Dm755 "$target_dir/$profile/mack-daemon" "$prefix/bin/mack-daemon"
install -Dm644 resources/linux/sh.mack.desktop \
  "$prefix/share/applications/sh.mack.desktop"
install -Dm644 website/public/app-icon.png \
  "$prefix/share/icons/hicolor/256x256/apps/sh.mack.png"
install -Dm644 LICENSE "$prefix/share/licenses/mack/LICENSE"

# Rearrange into an AppDir: binaries under usr/, matching the layout the
# updater and daemon resolution already expect (current_exe -> .../usr/bin).
appdir="$staging/AppDir"
mkdir -p "$appdir/usr"
cp -a "$prefix/bin" "$appdir/usr/bin"
cp -a "$prefix/share" "$appdir/usr/share"

cat >"$appdir/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/mack" "$@"
APPRUN
chmod +x "$appdir/AppRun"

# Fetch the packagers once into the shared cache (override via env to pin).
tooldir="$root/.mack-cache/appimage"
mkdir -p "$tooldir"
linuxdeploy_default="https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${arch}.AppImage"
appimagetool_default="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${arch}.AppImage"
linuxdeploy_bin="$tooldir/linuxdeploy-${arch}.AppImage"
appimagetool_bin="$tooldir/appimagetool-${arch}.AppImage"
linuxdeploy_url="${MACK_LINUXDEPLOY_URL:-$linuxdeploy_default}"
appimagetool_url="${MACK_APPIMAGETOOL_URL:-$appimagetool_default}"
if [ ! -x "$linuxdeploy_bin" ]; then
  curl -fsSL --retry 3 -o "$linuxdeploy_bin" "$linuxdeploy_url"
  chmod +x "$linuxdeploy_bin"
fi
if [ ! -x "$appimagetool_bin" ]; then
  curl -fsSL --retry 3 -o "$appimagetool_bin" "$appimagetool_url"
  chmod +x "$appimagetool_bin"
fi

# Bundle shared-library dependencies. The *.AppImage packagers run with
# --appimage-extract-and-run so no FUSE device is needed (CI runners).
"$linuxdeploy_bin" --appimage-extract-and-run \
  --appdir "$appdir" \
  --executable "$appdir/usr/bin/mack" \
  --desktop-file "$root/resources/linux/sh.mack.desktop" \
  --icon-file "$root/website/public/app-icon.png"

# appimagetool requires the top-level desktop file + icon to match Icon=.
test -f "$appdir/sh.mack.desktop"
test -f "$appdir/sh.mack.png" || test -f "$appdir/.DirIcon"
if [ ! -e "$appdir/.DirIcon" ]; then
  ln -sf sh.mack.png "$appdir/.DirIcon"
fi

mkdir -p "$(dirname "$out")"
rm -f "$out"
ARCH="$arch" "$appimagetool_bin" --appimage-extract-and-run \
  --comp zstd --no-appstream "$appdir" "$out"
chmod +x "$out"
sha256sum "$out" | sed "s| .*|  $(basename "$out")|" >"$out.sha256"

printf 'Created %s\n' "$out"
