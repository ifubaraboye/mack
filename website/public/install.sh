#!/usr/bin/env sh
set -eu

# Installs Mack for Linux into ~/.local — no root, no package manager.
# Downloads the release tarball from https://releases.mack.sh, unpacks it as
# ~/.local/mack.app, links the binary onto PATH, and registers the desktop
# entry. docs/linux.md documents the equivalent manual steps.
#
#   curl -fsSL https://mack.sh/install.sh | sh
#
# Environment:
#   MACK_VERSION        install this version instead of the latest
#   MACK_BUNDLE_PATH    install a local tarball instead of downloading
#   MACK_RELEASES_URL   base URL to download from

usage() {
    cat <<'USAGE'
Install Mack for Linux into ~/.local.

Usage:
  curl -fsSL https://mack.sh/install.sh | sh
  curl -fsSL https://mack.sh/install.sh | sh -s -- --uninstall

Options:
  --uninstall   Remove Mack, leaving ~/.mack (projects and settings) alone
  --help        Show this help
USAGE
}

main() {
    app_dir="$HOME/.local/mack.app"
    bin_link="$HOME/.local/bin/mack"
    desktop_file="$HOME/.local/share/applications/sh.mack.desktop"
    releases="${MACK_RELEASES_URL:-https://releases.mack.sh}"

    case "${1:-}" in
        --uninstall) uninstall; return ;;
        --help | -h) usage; return ;;
        "") ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac

    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        echo "Mack for macOS ships as a signed .dmg that updates itself." >&2
        echo "Download it from https://mack.sh" >&2
        exit 1
    fi
    if [ "$platform" != "Linux" ]; then
        echo "Unsupported platform: $platform" >&2
        exit 1
    fi

    machine="$(uname -m)"
    case "$machine" in
        x86_64) target="x86_64-unknown-linux-gnu" ;;
        aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
        *)
            echo "Unsupported architecture: $machine" >&2
            echo "Build from source: https://github.com/ifubaraboye/mack" >&2
            exit 1
            ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        fetch() { command curl -fsSL "$1"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch() { wget -qO- "$1"; }
    else
        echo "Could not find 'curl' or 'wget' in your PATH." >&2
        exit 1
    fi

    temp="$(mktemp -d "${TMPDIR:-/tmp}/mack-XXXXXX")"
    staging="$app_dir.new"
    trap 'rm -rf -- "$temp" "$staging"' EXIT INT TERM

    archive="$temp/mack.tar.gz"
    if [ -n "${MACK_BUNDLE_PATH:-}" ]; then
        cp "$MACK_BUNDLE_PATH" "$archive"
    else
        version="${MACK_VERSION:-}"
        if [ -z "$version" ]; then
            if ! version="$(fetch "$releases/latest-linux.txt")"; then
                echo "Could not reach $releases/latest-linux.txt." >&2
                echo "Pass MACK_VERSION to install a specific version." >&2
                exit 1
            fi
            version="$(printf '%s' "$version" | tr -d '[:space:]')"
        fi
        if [ -z "$version" ]; then
            echo "No Mack version published for Linux yet." >&2
            exit 1
        fi
        echo "Downloading Mack $version for $machine"
        if ! fetch "$releases/mack-$version-$target.tar.gz" >"$archive"; then
            echo "Download failed: $releases/mack-$version-$target.tar.gz" >&2
            exit 1
        fi
    fi
    if ! tar -tzf "$archive" >/dev/null 2>&1; then
        echo "Downloaded file is not a readable tarball." >&2
        exit 1
    fi

    # Unpack beside the target and swap only once the contents check out, so a
    # truncated download cannot leave a working install in pieces. The tarball
    # holds one versioned top-level directory; stripping it keeps every install
    # at the same path.
    echo "Installing to $app_dir"
    rm -rf "$staging"
    mkdir -p "$staging" "$(dirname "$bin_link")" "$(dirname "$desktop_file")"
    tar -xzf "$archive" --strip-components=1 -C "$staging"

    # Mack resolves its daemon and self-update helper next to its own
    # executable, so all three must stay together in bin/. Linking only the
    # main binary onto PATH is safe — current_exe() resolves the symlink back
    # into mack.app.
    for binary in mack mack-daemon mack-updater; do
        if [ ! -x "$staging/bin/$binary" ]; then
            echo "Archive is missing bin/$binary." >&2
            exit 1
        fi
    done
    if [ "$(cat "$staging/share/mack/self-update-v1" 2>/dev/null || true)" != \
        "mack-self-update-v1" ]; then
        echo "Archive is missing its managed-install marker." >&2
        exit 1
    fi
    # Replace rather than merge: a file dropped from a later layout must not
    # survive the upgrade.
    rm -rf "$app_dir"
    mv "$staging" "$app_dir"
    ln -sf "$app_dir/bin/mack" "$bin_link"

    entry="$app_dir/share/applications/sh.mack.desktop"
    if [ -f "$entry" ]; then
        # The packaged entry is relocatable (bare Exec/Icon names). Pin both to
        # this install so the launcher works without PATH or icon-theme setup.
        sed -e "s|^Exec=mack$|Exec=$app_dir/bin/mack|" \
            -e "s|^Icon=sh.mack$|Icon=$app_dir/share/icons/hicolor/256x256/apps/sh.mack.png|" \
            "$entry" >"$desktop_file"
        if command -v update-desktop-database >/dev/null 2>&1; then
            update-desktop-database "$(dirname "$desktop_file")" 2>/dev/null || true
        fi
    fi

    # Mack is a desktop app and takes no arguments, so the launcher entry is
    # the way in. The PATH link is a convenience for starting it from a
    # terminal to watch its output.
    echo "Mack is installed."
    if [ -f "$desktop_file" ]; then
        echo "Open it from your applications menu."
    fi
    if [ "$(command -v mack || true)" = "$bin_link" ]; then
        echo "From a terminal: mack"
    else
        echo "From a terminal: $bin_link"
    fi
}

uninstall() {
    if [ ! -d "$app_dir" ] && [ ! -L "$bin_link" ]; then
        echo "Mack is not installed at $app_dir." >&2
        exit 1
    fi
    # Only reclaim the symlink and desktop entry this script created; a
    # distro package's copies of both belong to the package manager.
    if [ "$(readlink "$bin_link" 2>/dev/null || true)" = "$app_dir/bin/mack" ]; then
        rm -f "$bin_link"
    fi
    if [ -f "$desktop_file" ] && grep -qF "$app_dir/bin/mack" "$desktop_file"; then
        rm -f "$desktop_file"
    fi
    rm -rf "$app_dir"
    echo "Mack is uninstalled. Projects and settings remain in ~/.mack."
}

main "$@"
