#!/bin/sh
set -e

FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
  esac
done

# Configurable
INSTALL_DIR="${TIGHTBEAM_INSTALL_DIR:-$HOME/.local/bin}"
REPO="calebfaruki/tightbeam"

# Detect platform
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)  OS_NAME="linux" ;;
    darwin) OS_NAME="darwin" ;;
    *)      echo "tightbeam: unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64)   ARCH_NAME="amd64" ;;
    aarch64|arm64)   ARCH_NAME="arm64" ;;
    *)               echo "tightbeam: unsupported architecture: $ARCH"; exit 1 ;;
esac

ARTIFACT="tightbeam-daemon-${OS_NAME}-${ARCH_NAME}"

# Get latest release tag
if command -v curl >/dev/null 2>&1; then
    LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
    DOWNLOAD="curl -fsSL -o"
elif command -v wget >/dev/null 2>&1; then
    LATEST=$(wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
    DOWNLOAD="wget -qO"
else
    echo "tightbeam: curl or wget required"
    exit 1
fi

if [ -z "$LATEST" ]; then
    echo "tightbeam: could not determine latest release"
    exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST}/${ARTIFACT}"

# Download
echo "tightbeam: downloading ${ARTIFACT} (${LATEST})..."
mkdir -p "$INSTALL_DIR"
$DOWNLOAD "$INSTALL_DIR/tightbeam-daemon" "$URL"
chmod +x "$INSTALL_DIR/tightbeam-daemon"

# Ad-hoc sign on macOS (unsigned binaries get Killed: 9)
case "$OS_NAME" in
    darwin) codesign -s - "$INSTALL_DIR/tightbeam-daemon" ;;
esac

# Verify
if [ "$FORCE" = "0" ] && ! "$INSTALL_DIR/tightbeam-daemon" version >/dev/null 2>&1; then
    echo "tightbeam: download failed or binary is incompatible"
    exit 1
fi

if [ "$FORCE" = "1" ]; then
    echo "tightbeam: force-installed to $INSTALL_DIR/tightbeam-daemon"
else
    echo "tightbeam: installed to $INSTALL_DIR/tightbeam-daemon"
fi

# Check PATH
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "tightbeam: warning — $INSTALL_DIR is not in your PATH"
        echo "tightbeam: add this to your shell profile:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

# Run init
echo ""
"$INSTALL_DIR/tightbeam-daemon" init
