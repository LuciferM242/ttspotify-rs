#!/usr/bin/env bash
# Build setup for tt-spotify-bot (Linux)
# Installs the build dependencies, plus curl (rustup is fetched with it, and
# build-essential does not bring it) and libpulse and ALSA, which the TeamTalk
# SDK loads when the bot starts - building without them succeeds and then
# fails at the first run.

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[+]${NC} $1"; }
warn()  { echo -e "${YELLOW}[!]${NC} $1"; }
error() { echo -e "${RED}[x]${NC} $1"; }

# Detect package manager
if command -v apt-get &>/dev/null; then
    PM="apt"
elif command -v dnf &>/dev/null; then
    PM="dnf"
elif command -v pacman &>/dev/null; then
    PM="pacman"
else
    PM="unknown"
fi

info "Detected package manager: $PM"

# Install system dependencies
install_deps() {
    info "Installing system dependencies..."
    case $PM in
        apt)
            sudo apt-get update
            # Ubuntu 24.04 renamed ALSA's library package; libasound2 is only a
            # virtual name there, which apt refuses to install.
            ALSA=libasound2
            apt-cache show libasound2t64 &>/dev/null && ALSA=libasound2t64
            sudo apt-get install -y build-essential pkg-config libssl-dev libclang-dev curl libpulse0 "$ALSA"
            ;;
        dnf)
            sudo dnf install -y gcc pkg-config openssl-devel clang-devel curl pulseaudio-libs alsa-lib
            ;;
        pacman)
            sudo pacman -S --needed --noconfirm base-devel pkg-config openssl clang curl libpulse alsa-lib
            ;;
        *)
            warn "Unknown package manager. Please install manually:"
            warn "  - C compiler (gcc/clang)"
            warn "  - pkg-config"
            warn "  - OpenSSL development headers (libssl-dev / openssl-devel)"
            warn "  - libclang development headers (libclang-dev / clang-devel)"
            warn "  - curl (rustup is fetched with it)"
            warn "  - libpulse (libpulse0 / pulseaudio-libs), which the TeamTalk SDK loads at runtime"
            warn "  - ALSA (libasound2 / libasound2t64 / alsa-lib), which the TeamTalk SDK also loads"
            ;;
    esac
}

# Install Rust via rustup
install_rust() {
    if command -v rustc &>/dev/null; then
        info "Rust already installed: $(rustc --version)"
    else
        info "Installing Rust via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
        info "Rust installed: $(rustc --version)"
    fi
}

echo ""
echo "============================="
echo "  TT Spotify Bot - Setup"
echo "============================="
echo ""

install_deps
install_rust

echo ""
info "All dependencies installed."
echo ""
