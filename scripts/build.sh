#!/usr/bin/env bash
# Build the Linux and Windows release binaries.
# One binary per platform: on Windows it is the tray GUI, on Linux the CLI bot.
# Run from the project root.

set -e

echo "Building Linux release..."
cargo build --release --quiet --bin tt-spotify-bot

echo "Building Windows release..."
powershell.exe -ExecutionPolicy Bypass -Command "cargo build --release --quiet --bin tt-spotify-bot"

echo ""
echo "Done. Binaries:"
echo "  Linux:   target/release/tt-spotify-bot"
echo "  Windows: target/release/tt-spotify-bot.exe"
