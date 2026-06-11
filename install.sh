#!/bin/sh
# hifi installer: downloads the latest release binary and installs it on PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/toner-industries/hifi/main/install.sh | sh
#
# Files fetched with curl never get macOS's quarantine attribute, so this
# path never triggers the Gatekeeper "Apple could not verify…" dialog that
# a browser-downloaded binary does.
set -eu

REPO="toner-industries/hifi"
BIN_DIR="${HIFI_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -sm)" in
"Darwin arm64") TARGET="aarch64-apple-darwin" ;;
*)
    echo "error: no prebuilt binary for '$(uname -sm)' yet — build from source instead:" >&2
    echo "  git clone https://github.com/$REPO.git && cd hifi && cargo run --release" >&2
    exit 1
    ;;
esac

TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
if [ -z "$TAG" ]; then
    echo "error: could not determine the latest release of $REPO" >&2
    echo "(if the repo is private, use: gh release download --repo $REPO -p '*.tar.gz')" >&2
    exit 1
fi

URL="https://github.com/$REPO/releases/download/$TAG/hifi-$TAG-$TARGET.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading hifi $TAG ($TARGET)..."
curl -fsSL "$URL" -o "$TMP/hifi.tar.gz"
tar -xzf "$TMP/hifi.tar.gz" -C "$TMP"

mkdir -p "$BIN_DIR"
install -m 755 "$TMP/hifi" "$BIN_DIR/hifi"
echo "Installed $BIN_DIR/hifi"

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*)
    echo
    echo "note: $BIN_DIR is not on your PATH — add this line to your shell profile:"
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
    ;;
esac

echo
echo "hifi stores its login and state in the directory you run it from."
echo "Start it from one you'll keep using:"
echo "  mkdir -p ~/music && cd ~/music && hifi"
