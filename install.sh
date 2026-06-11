#!/usr/bin/env bash
# clauden installer — builds and installs the `clauden` binary.
set -euo pipefail

REPO="https://github.com/skishore23/clauden"

bold() { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }
red() { printf '\033[31m%s\033[0m' "$1"; }

echo
echo "  $(bold '(• ◡ -)  clauden installer')"
echo

if ! command -v cargo >/dev/null 2>&1; then
  echo "  $(red '✗') Rust/cargo not found."
  echo "    Install Rust first: https://rustup.rs"
  exit 1
fi

echo "  › Building and installing via cargo (this may take a minute)…"
cargo install --git "$REPO" --force

echo
echo "  $(green '✓') Installed. Next steps:"
echo
echo "      clauden login      # add a Claude account (repeat per account)"
echo "      clauden list       # see your accounts"
echo "      clauden            # run the proxy + launch Claude Code"
echo

if ! command -v clauden >/dev/null 2>&1; then
  echo "  $(bold 'Note:') ~/.cargo/bin is not on your PATH. Add this to your shell rc:"
  echo "      export PATH=\"\$HOME/.cargo/bin:\$PATH\""
  echo
fi
