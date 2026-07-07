#!/bin/sh
# Install the latest tinycd release binary:
#
#   curl -fsSL https://raw.githubusercontent.com/maxz411/tinycd/main/install.sh | sh
#
# Environment:
#   TINYCD_INSTALL_DIR  install directory (default: ~/.local/bin)
#   TINYCD_INSTALL_URL  download base (default: the latest GitHub release)
set -eu

REPO="maxz411/tinycd"
BASE_URL="${TINYCD_INSTALL_URL:-https://github.com/$REPO/releases/latest/download}"
INSTALL_DIR="${TINYCD_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$1"; }
err() { printf 'install.sh: %s\n' "$1" >&2; exit 1; }

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        err "curl or wget is required"
    fi
}

case "$(uname -s)" in
    Linux) libc="unknown-linux-musl" ;;
    Darwin) libc="apple-darwin" ;;
    *) err "unsupported operating system $(uname -s); on Windows run install.ps1" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) err "no prebuilt binary for $(uname -m); build from source with cargo instead" ;;
esac
archive="tinycd-$arch-$libc.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $archive"
download "$BASE_URL/$archive" "$tmp/$archive"
download "$BASE_URL/$archive.sha256" "$tmp/$archive.sha256"

cd "$tmp"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$archive.sha256" >/dev/null
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$archive.sha256" >/dev/null
else
    say "warning: neither sha256sum nor shasum found; skipping checksum verification"
fi

tar -xzf "$archive"
mkdir -p "$INSTALL_DIR"
mv tinycd "$INSTALL_DIR/tinycd"
chmod +x "$INSTALL_DIR/tinycd"

say "installed $("$INSTALL_DIR/tinycd" --version) to $INSTALL_DIR/tinycd"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say "note: $INSTALL_DIR is not on PATH; add it to your shell profile:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
