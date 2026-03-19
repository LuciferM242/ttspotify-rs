# Build setup for TT Spotify (Windows)
# Installs required dependencies and builds both binaries.
# Run: powershell -ExecutionPolicy Bypass -File scripts\setup.ps1

$ErrorActionPreference = "Stop"

function Info($msg)  { Write-Host "[+] $msg" -ForegroundColor Green }
function Warn($msg)  { Write-Host "[!] $msg" -ForegroundColor Yellow }
function Error($msg) { Write-Host "[x] $msg" -ForegroundColor Red }

Write-Host ""
Write-Host "=============================="
Write-Host "  TT Spotify - Windows Setup"
Write-Host "=============================="
Write-Host ""

# Check winget
$hasWinget = Get-Command winget -ErrorAction SilentlyContinue
if (-not $hasWinget) {
    Error "winget not found. Please install App Installer from the Microsoft Store."
    exit 1
}

# Rust
if (Get-Command rustc -ErrorAction SilentlyContinue) {
    Info "Rust already installed: $(rustc --version)"
} else {
    Info "Installing Rust..."
    winget install Rustlang.Rustup --silent --accept-package-agreements --accept-source-agreements
    # Refresh PATH
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
    if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
        Warn "Rust installed but not in PATH. Please restart your terminal and run this script again."
        exit 0
    }
    Info "Rust installed: $(rustc --version)"
}

# CMake (required for wxDragon)
if (Get-Command cmake -ErrorAction SilentlyContinue) {
    Info "CMake already installed: $(cmake --version | Select-Object -First 1)"
} else {
    Info "Installing CMake (required for wxDragon GUI)..."
    winget install Kitware.CMake --silent --accept-package-agreements --accept-source-agreements
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
        Warn "CMake installed but not in PATH. Please restart your terminal and run this script again."
        exit 0
    }
}

# Ninja (required for wxDragon)
if (Get-Command ninja -ErrorAction SilentlyContinue) {
    Info "Ninja already installed."
} else {
    Info "Installing Ninja (required for wxDragon GUI)..."
    winget install Ninja-build.Ninja --silent --accept-package-agreements --accept-source-agreements
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
}

# LLVM/Clang (required by teamtalk-sys bindgen)
if (Get-Command clang -ErrorAction SilentlyContinue) {
    Info "LLVM/Clang already installed."
} else {
    Info "Installing LLVM (required for TeamTalk FFI bindings)..."
    winget install LLVM.LLVM --silent --accept-package-agreements --accept-source-agreements
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
}

# Check for C++ compiler (Visual Studio Build Tools)
$clExe = Get-ChildItem "C:\Program Files (x86)\Microsoft Visual Studio" -Recurse -Filter "cl.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
if ($clExe) {
    Info "C++ compiler found: $($clExe.FullName)"
} else {
    Warn "Visual Studio C++ compiler not found."
    Warn "Install Build Tools with 'Desktop development with C++' workload from:"
    Warn "  https://visualstudio.microsoft.com/visual-cpp-build-tools/"
    exit 1
}

Write-Host ""
Info "All dependencies installed."
Write-Host ""
