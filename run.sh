#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

SCRCPY_VERSION="v5.0.1"
SCRCPY_URL="https://github.com/Genymobile/scrcpy/releases/download/${SCRCPY_VERSION}/scrcpy-linux-x86_64-${SCRCPY_VERSION}.tar.gz"

if [[ ! -x "$SCRIPT_DIR/vendor/scrcpy/scrcpy" ]]; then
    echo "Downloading bundled scrcpy ${SCRCPY_VERSION}..."
    mkdir -p "$SCRIPT_DIR/vendor"
    curl -L -s "$SCRCPY_URL" | tar -xz -C "$SCRIPT_DIR/vendor"
    rm -rf "$SCRIPT_DIR/vendor/scrcpy"
    mv "$SCRIPT_DIR/vendor/scrcpy-linux-x86_64-${SCRCPY_VERSION}" "$SCRIPT_DIR/vendor/scrcpy"
fi

if [[ ! -x "$SCRIPT_DIR/target/release/screenr" ]]; then
    echo "Building Screenr (release)..."
    cargo build --release
fi

exec "$SCRIPT_DIR/target/release/screenr" "$@"
