# Build setup for TT Spotify (Windows)
# Installs required build dependencies.
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

# The GUI is native Win32 (winsafe): no CMake, Ninja or toolkit downloads.

# LLVM/Clang (required by teamtalk-sys bindgen)
if (Get-Command clang -ErrorAction SilentlyContinue) {
    Info "LLVM/Clang already installed."
} else {
    Info "Installing LLVM (required for TeamTalk FFI bindings)..."
    winget install LLVM.LLVM --silent --accept-package-agreements --accept-source-agreements
    $env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
}

# Set LIBCLANG_PATH permanently (bindgen needs this to find libclang.dll)
$llvmBin = "C:\Program Files\LLVM\bin"
if (Test-Path $llvmBin) {
    $current = [Environment]::GetEnvironmentVariable("LIBCLANG_PATH", "User")
    if ($current -ne $llvmBin) {
        [Environment]::SetEnvironmentVariable("LIBCLANG_PATH", $llvmBin, "User")
        $env:LIBCLANG_PATH = $llvmBin
        Info "LIBCLANG_PATH set to $llvmBin"
    }
}

# Check for a C++ compiler (Visual Studio Build Tools, or the full IDE).
# Ask vswhere rather than searching a directory: Build Tools 2022 installs
# under Program Files (x86) while the 2022 IDE installs under Program Files,
# so looking in either one alone reports a missing compiler to half of people.
$vswhere = "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe"
$clExe = $null
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1
    if ($vsPath) { $clExe = $vsPath }
}
if (-not $clExe) {
    # No vswhere (it ships with 2017 and later): fall back to looking in both
    # roots for the compiler itself.
    foreach ($root in @("C:\Program Files\Microsoft Visual Studio", "C:\Program Files (x86)\Microsoft Visual Studio")) {
        if (Test-Path $root) {
            $found = Get-ChildItem $root -Recurse -Filter "cl.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($found) { $clExe = $found.FullName; break }
        }
    }
}
if ($clExe) {
    Info "C++ compiler found: $clExe"
} else {
    Warn "Visual Studio C++ compiler not found."
    Warn "Install Build Tools with 'Desktop development with C++' workload from:"
    Warn "  https://visualstudio.microsoft.com/visual-cpp-build-tools/"
    exit 1
}

Write-Host ""
Info "All dependencies installed."
Write-Host ""
