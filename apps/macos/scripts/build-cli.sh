#!/bin/sh
set -eu

TARGET_TRIPLE=${1:?Rust target triple is required}
case "$TARGET_TRIPLE" in
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) echo "unsupported Mix CLI target: $TARGET_TRIPLE" >&2; exit 2 ;;
esac

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$APP_DIR/../.." && pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/mix-target}
case "$TARGET_DIR" in
  /*) ;;
  *) echo "CARGO_TARGET_DIR must be an absolute path" >&2; exit 3 ;;
esac
export CARGO_TARGET_DIR="$TARGET_DIR"
mkdir -p "$TARGET_DIR"
touch "$TARGET_DIR/.mix-target-owned"
. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_DIR="$MIX_RUSTUP_HOME"
CARGO_DIR="$MIX_CARGO_HOME"
RUST_VERSION=${MIX_RUST_TOOLCHAIN:-1.98.0}

if [ ! -x "$CARGO_DIR/bin/cargo" ]; then
  echo "Mix Rust toolchain is missing; run scripts/bootstrap-rust.sh first" >&2
  exit 2
fi

export RUSTUP_HOME="$RUSTUP_DIR"
export CARGO_HOME="$CARGO_DIR"
export RUSTUP_TOOLCHAIN="$RUST_VERSION"
. "$SCRIPT_DIR/configure-rust-release.sh"

"$CARGO_DIR/bin/cargo" build --release --locked --package mix-cli --target "$TARGET_TRIPLE"
sh "$SCRIPT_DIR/strip-macos.sh" "$TARGET_DIR/$TARGET_TRIPLE/release/mix"
printf 'built Mix CLI: %s\n' "$TARGET_DIR/$TARGET_TRIPLE/release/mix"
