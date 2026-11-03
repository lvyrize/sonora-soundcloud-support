#!/bin/sh
# Installs Sonora with SoundCloud support for the current user, in place of the Sonora your
# package manager installed. Nothing system-wide changes: the binary goes to ~/.local/bin and a
# launcher entry to ~/.local/share/applications, both of which take precedence over the system
# ones. Run it again to update, or with --uninstall to go back to the normal Sonora.
#
#   curl -fsSL https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/lvyrize/sonora-soundcloud-support/feat/soundcloud/scripts/install-soundcloud.sh | sh -s -- --uninstall

set -eu

REPO="lvyrize/sonora-soundcloud-support"
ASSET="sonora-soundcloud-x86_64-linux.tar.gz"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
BIN="$BIN_DIR/sonora"
ENTRY="$APP_DIR/sonora.desktop"
SYSTEM_ENTRY="/usr/share/applications/sonora.desktop"

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

refresh() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
}

uninstall() {
    rm -f "$BIN" "$ENTRY"
    refresh
    say "Removed Sonora with SoundCloud. Your normal Sonora is back."
    say "Your settings and accounts were left as they are."
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

# The launcher entry is the system one pointed at this binary, so the icon, file types and
# links all stay as they were. Without a system Sonora a plain entry is written instead.
if [ -f "$SYSTEM_ENTRY" ]; then
    sed -e "s|^Exec=sonora|Exec=$BIN|" -e "s|^TryExec=.*|TryExec=$BIN|" \
        "$SYSTEM_ENTRY" > "$ENTRY"
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
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "Note: $BIN_DIR is not on your PATH, so typing 'sonora' still starts the old one." \
        "Your app launcher uses the new one either way." ;;
esac
say "Quit Sonora if it is running, then open it again."
