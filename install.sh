#!/usr/bin/env sh
# Builds csearch and installs it to ~/.local/bin (override with BIN_DIR=...).
set -eu

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found. Install Rust first: https://rustup.rs" >&2
    exit 1
fi

cargo build --release
mkdir -p "$BIN_DIR"
# Remove first: overwriting a binary in place makes macOS kill it
# (stale code-signature cache for the same inode).
rm -f "$BIN_DIR/csearch"
cp target/release/csearch "$BIN_DIR/csearch"
echo "Installed csearch to $BIN_DIR/csearch"

case ":$PATH:" in
    *":$BIN_DIR:"*) echo "Run: csearch" ;;
    *)
        echo
        echo "$BIN_DIR is not on your PATH. Add this to ~/.zshrc or ~/.bashrc:"
        echo "    export PATH=\"$BIN_DIR:\$PATH\""
        ;;
esac
