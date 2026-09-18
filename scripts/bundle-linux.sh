#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-target}"
version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"mack","version":"\([^"]*\)".*/\1/p')"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
package="mack-${version}-${target_triple}"
archive="$target_dir/release/$package.tar.gz"
staging="$(mktemp -d)"
trap 'rm -rf -- "$staging"' EXIT

cargo build --locked --release \
  --package waku --bin mack --bin mack-updater --bin mack_js_repl \
  --package waku-daemon --bin mack-daemon \
  --package waku-computer-use --bin mack_computer_use

package_dir="$staging/$package"
bun scripts/cua-driver.ts bundle "$package_dir/bin" "$package_dir/share/mack" release
install -Dm755 "$target_dir/release/mack" "$package_dir/bin/mack"
install -Dm755 "$target_dir/release/mack-updater" "$package_dir/bin/mack-updater"
install -Dm755 "$target_dir/release/mack-daemon" "$package_dir/bin/mack-daemon"
install -Dm644 resources/linux/sh.mack.desktop \
  "$package_dir/share/applications/sh.mack.desktop"
install -Dm644 resources/linux/self-update-v1 \
  "$package_dir/share/mack/self-update-v1"
install -Dm644 website/public/app-icon.png \
  "$package_dir/share/icons/hicolor/256x256/apps/sh.mack.png"
install -Dm644 LICENSE "$package_dir/share/licenses/mack/LICENSE"

mkdir -p "$(dirname "$archive")"
tar -C "$staging" -czf "$archive" "$package"
printf 'Created %s\n' "$archive"
