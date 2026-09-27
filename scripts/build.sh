#!/usr/bin/env bash
# Build the Linux and Windows release binaries.
# One binary per platform: on Windows it is the tray GUI, on Linux the CLI bot.
# The Windows build runs only under WSL, where powershell.exe reaches Windows.

set -e

cd "$(dirname "$0")/.."

# The oldest Rust that builds the locked dependencies, from Cargo.toml.
need=$(grep -m1 '^rust-version' Cargo.toml | cut -d'"' -f2)
have=$(rustc --version 2>/dev/null | awk '{print $2}')
if [ -z "$have" ]; then
    echo "Rust is not installed. Run ./scripts/setup.sh, which installs it."
    exit 1
fi
if [ -n "$need" ] && [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | head -n1)" != "$need" ]; then
    echo "Rust $have is too old; this needs $need or newer. Run ./scripts/setup.sh, which updates it."
    exit 1
fi

echo "Building Linux release..."
cargo build --release --quiet --bin tt-spotify-bot

# Under WSL, and only when Windows has its own Rust to build with.
windows=no
if command -v powershell.exe &>/dev/null; then
    if powershell.exe -NoProfile -Command "Get-Command cargo" &>/dev/null; then
        windows=yes
    else
        echo "Skipping the Windows build: Windows has no Rust. Run scripts\setup.ps1 there first."
    fi
fi

if [ "$windows" = yes ]; then
    echo "Building Windows release..."
    if ! powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "cargo build --release --quiet --bin tt-spotify-bot"; then
        echo "The Windows build failed. If the tray is running from target\release, close it and run this again."
        exit 1
    fi
fi

echo ""
echo "Done. Binaries:"
echo "  Linux:   target/release/tt-spotify-bot"
if [ "$windows" = yes ]; then
    echo "  Windows: target/release/tt-spotify-bot.exe"
fi
