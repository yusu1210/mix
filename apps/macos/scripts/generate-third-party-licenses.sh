#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$APP_DIR/../.." && pwd)
. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_DIR="$MIX_RUSTUP_HOME"
CARGO_DIR="$MIX_CARGO_HOME"
RUST_VERSION=${MIX_RUST_TOOLCHAIN:-1.98.0}
OUTPUT="$APP_DIR/src-tauri/resources/THIRD-PARTY-LICENSES.html"

if [ ! -x "$CARGO_DIR/bin/cargo" ]; then
  echo "Mix Rust toolchain is missing; run scripts/bootstrap-rust.sh first" >&2
  exit 2
fi
sh "$SCRIPT_DIR/bootstrap-cargo-about.sh"

case "${MIX_BUILD_ARCH:-$(uname -m)}" in
  arm64) TARGET_TRIPLE=aarch64-apple-darwin ;;
  x86_64) TARGET_TRIPLE=x86_64-apple-darwin ;;
  *) echo "unsupported license target architecture" >&2; exit 3 ;;
esac

mkdir -p "$(dirname "$OUTPUT")"
PATH="$CARGO_DIR/bin:$PATH" \
RUSTUP_HOME="$RUSTUP_DIR" \
CARGO_HOME="$CARGO_DIR" \
RUSTUP_TOOLCHAIN="$RUST_VERSION" \
  "$CARGO_DIR/bin/cargo-about" generate \
    --manifest-path "$APP_DIR/src-tauri/Cargo.toml" \
    --config "$APP_DIR/src-tauri/about.toml" \
    --target "$TARGET_TRIPLE" \
    --locked --fail \
    "$APP_DIR/src-tauri/about.hbs" > "$OUTPUT"

test -s "$OUTPUT"
printf '%s\n' "$OUTPUT"
