#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

echo "=== ZenSight Flatpak Build ==="

# Check for required tools
if ! command -v flatpak-builder &> /dev/null; then
    echo "Error: flatpak-builder not found. Install it with:"
    echo "  sudo dnf install flatpak-builder"
    exit 1
fi

# Install SDK extension if needed
echo "Checking Flatpak SDK..."
flatpak install --user -y flathub org.freedesktop.Platform//24.08 org.freedesktop.Sdk//24.08 org.freedesktop.Sdk.Extension.rust-stable//24.08 || true

# Build the Flatpak
echo "Building Flatpak..."
cd flatpak
flatpak-builder --user --install --force-clean build-dir com.github.p13marc.ZenSight.yml

echo ""
echo "=== Build complete! ==="
echo "Run with: flatpak run com.github.p13marc.ZenSight"
