#!/bin/sh
# Installs Sonora with SoundCloud support for the current user, whole: the app, its launcher
# entry and its icons. It needs no Sonora installed before it, and where a package manager
# installed one, this copy takes its place for this user, since ~/.local comes first. Nothing
# system-wide changes. Run it again to update, or with --uninstall to remove it again, which
# brings back the package manager's Sonora where there is one.
#
#   curl -fsSL https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.sh | sh -s -- --uninstall

set -eu

REPO="lvyrize/sonora-soundcloud-support"
ASSET="sonora-soundcloud-x86_64-linux.tar.gz"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
APP_DIR="$DATA/applications"
ICON_DIR="$DATA/icons/hicolor"
BIN="$BIN_DIR/sonora"
ENTRY="$APP_DIR/sonora.desktop"
SYSTEM_ENTRY="/usr/share/applications/sonora.desktop"
# Every icon this script puts in place, so --uninstall takes away exactly those.
ICONS="16x16 24x24 32x32 48x48 64x64 128x128 256x256 512x512"

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

refresh() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$ICON_DIR/index.theme" ]; then
        gtk-update-icon-cache -q -t "$ICON_DIR" >/dev/null 2>&1 || true
    fi
}

uninstall() {
    rm -f "$BIN" "$ENTRY" "$ICON_DIR/scalable/apps/sonora.svg"
    for size in $ICONS; do
        rm -f "$ICON_DIR/$size/apps/sonora.png"
    done
    refresh
    say "Removed Sonora with SoundCloud."
    if [ -f "$SYSTEM_ENTRY" ]; then
        say "Your normal Sonora is back."
    fi
    say "Your settings, accounts and playlists were left as they are."
    exit 0
}

[ "${1:-}" = "--uninstall" ] && uninstall

[ "$(uname -s)" = "Linux" ] || fail "this build is for Linux only"
[ "$(uname -m)" = "x86_64" ] || fail "this build is for x86_64 only"
command -v curl >/dev/null 2>&1 || fail "curl is needed"
command -v tar >/dev/null 2>&1 || fail "tar is needed"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

say "Downloading Sonora with SoundCloud..."
curl -fL --progress-bar -o "$TMP/$ASSET" \
    "https://github.com/$REPO/releases/latest/download/$ASSET"
tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -f "$TMP/sonora" ] || fail "the download holds no sonora binary"

mkdir -p "$BIN_DIR" "$APP_DIR"
install -m 755 "$TMP/sonora" "$BIN"

# The icons come with the download. An older download without them leans on a system Sonora's.
if [ -d "$TMP/icons/hicolor" ]; then
    if [ -f "$TMP/icons/hicolor/scalable/apps/sonora.svg" ]; then
        install -Dm644 "$TMP/icons/hicolor/scalable/apps/sonora.svg" \
            "$ICON_DIR/scalable/apps/sonora.svg"
    fi
    for size in $ICONS; do
        if [ -f "$TMP/icons/hicolor/$size/apps/sonora.png" ]; then
            install -Dm644 "$TMP/icons/hicolor/$size/apps/sonora.png" \
                "$ICON_DIR/$size/apps/sonora.png"
        fi
    done
fi

# The launcher entry is Sonora's own pointed at this binary, so the file types and links it
# opens stay the same. It comes with the download, else from a system Sonora, else it is
# written plain.
SOURCE_ENTRY=""
if [ -f "$TMP/sonora.desktop" ]; then
    SOURCE_ENTRY="$TMP/sonora.desktop"
elif [ -f "$SYSTEM_ENTRY" ]; then
    SOURCE_ENTRY="$SYSTEM_ENTRY"
fi
if [ -n "$SOURCE_ENTRY" ]; then
    sed -e "s|^Exec=sonora|Exec=$BIN|" -e "s|^TryExec=.*|TryExec=$BIN|" \
        "$SOURCE_ENTRY" > "$ENTRY"
else
    cat > "$ENTRY" <<EOF
[Desktop Entry]
Type=Application
Name=Sonora
GenericName=Music Player
Comment=A native music streaming client, with SoundCloud
Icon=sonora
Exec=$BIN %U
Terminal=false
Categories=AudioVideo;Audio;Player;
StartupWMClass=sonora
StartupNotify=true
EOF
fi
refresh

say "Installed to $BIN"

# Sonora links against a few system libraries every desktop has. Say which are missing rather
# than leaving the app to fail silently when it starts.
if command -v ldd >/dev/null 2>&1; then
    MISSING="$(ldd "$BIN" 2>/dev/null | awk '/not found/ { print $1 }' | tr '\n' ' ')"
    if [ -n "$MISSING" ]; then
        say "Warning: these libraries Sonora needs are missing: $MISSING"
        say "Install them with your package manager (for example alsa-lib, sqlite, vulkan-icd-loader," \
            "libxkbcommon-x11 and wayland), then start Sonora."
    fi
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "Note: $BIN_DIR is not on your PATH, so typing 'sonora' may not find it." \
        "Your app launcher starts it either way." ;;
esac
say "Quit Sonora if it is running, then open it again."
