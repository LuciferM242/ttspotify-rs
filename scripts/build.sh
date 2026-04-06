#!/usr/bin/env bash
# Build both Linux and Windows release binaries.
# Run from the project root.

set -e

echo "Building Linux release..."
cargo build --release

echo ""
echo "Building Windows release (via PowerShell)..."
powershell.exe -ExecutionPolicy Bypass -Command "cargo build --release"

echo ""
echo "Done. Binaries:"
echo "  Linux:   target/release/tt-spotify-bot"
echo "  Windows: target/release/tt-spotify-bot.exe"
echo "  Windows: target/release/tt-spotify-win.exe"
