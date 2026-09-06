#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$APP_DIR/../.." && pwd)
. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_DIR="$MIX_RUSTUP_HOME"
CARGO_DIR="$MIX_CARGO_HOME"
RUST_VERSION=${MIX_RUST_TOOLCHAIN:-1.98.0}
CARGO_ABOUT_VERSION=0.9.0

if [ ! -x "$CARGO_DIR/bin/cargo" ]; then
  echo "Mix Rust toolchain is missing; run scripts/bootstrap-rust.sh first" >&2
  exit 2
fi

if [ -x "$CARGO_DIR/bin/cargo-about" ] && [ "$("$CARGO_DIR/bin/cargo-about" --version)" = "cargo-about $CARGO_ABOUT_VERSION" ]; then
  exit 0
fi

PATH="$CARGO_DIR/bin:$PATH" \
  RUSTUP_HOME="$RUSTUP_DIR" \
  CARGO_HOME="$CARGO_DIR" \
  RUSTUP_TOOLCHAIN="$RUST_VERSION" \
  "$CARGO_DIR/bin/cargo" install --locked --version "$CARGO_ABOUT_VERSION" --features cli cargo-about

test "$("$CARGO_DIR/bin/cargo-about" --version)" = "cargo-about $CARGO_ABOUT_VERSION"
