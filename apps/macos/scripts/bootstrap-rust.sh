#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$APP_DIR/../.." && pwd)
. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_DIR="$MIX_RUSTUP_HOME"
CARGO_DIR="$MIX_CARGO_HOME"
RUSTUP_VERSION=1.29.1
RUST_VERSION=${MIX_RUST_TOOLCHAIN:-1.98.0}
HOST_ARCH=$(uname -m)
BUILD_ARCH=${MIX_BUILD_ARCH:-$(uname -m)}

if [ "$(uname -s)" != Darwin ]; then
  echo "Mix desktop Rust bootstrap currently supports macOS only" >&2
  exit 3
fi

case "$HOST_ARCH" in
  arm64)
    RUSTUP_TARGET=aarch64-apple-darwin
    RUSTUP_SHA256=ec1b9233e7f72990ecd8e62063fa7f6c3dfc2bec8e97f88bff165f9100ac696a
    ;;
  x86_64)
    RUSTUP_TARGET=x86_64-apple-darwin
    RUSTUP_SHA256=259e2b84274434085163fe8d556510571772cda2aa6d87ca6aa664f57bc644e3
    ;;
  *) echo "unsupported macOS host architecture: $HOST_ARCH" >&2; exit 3 ;;
esac

case "$BUILD_ARCH" in
  arm64) TARGET_TRIPLE=aarch64-apple-darwin ;;
  x86_64) TARGET_TRIPLE=x86_64-apple-darwin ;;
  *) echo "unsupported Rust target architecture: $BUILD_ARCH" >&2; exit 3 ;;
esac

mkdir -p "$MIX_TOOL_ROOT"
INSTALLED_RUSTUP_VERSION=
if [ -x "$CARGO_DIR/bin/rustup" ]; then
  INSTALLED_RUSTUP_VERSION=$(RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" \
    "$CARGO_DIR/bin/rustup" --version 2>/dev/null | sed -n 's/^rustup \([^ ]*\).*/\1/p')
fi
if [ "$INSTALLED_RUSTUP_VERSION" != "$RUSTUP_VERSION" ]; then
  INSTALLER=$(mktemp "$MIX_TOOL_ROOT/.rustup-init.XXXXXX")
  cleanup() { rm -f "$INSTALLER"; }
  trap cleanup EXIT INT TERM
  /usr/bin/curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$RUSTUP_TARGET/rustup-init" \
    --output "$INSTALLER"
  ACTUAL_SHA256=$(/usr/bin/shasum -a 256 "$INSTALLER" | awk '{print $1}')
  if [ "$ACTUAL_SHA256" != "$RUSTUP_SHA256" ]; then
    echo "rustup-init checksum mismatch for $RUSTUP_TARGET" >&2
    exit 4
  fi
  chmod 700 "$INSTALLER"
  RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" "$INSTALLER" \
    -y --no-modify-path --profile minimal --default-host "$RUSTUP_TARGET" \
    --default-toolchain "$RUST_VERSION"
fi

RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" "$CARGO_DIR/bin/rustup" set auto-self-update disable
RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" "$CARGO_DIR/bin/rustup" toolchain install "$RUST_VERSION" --profile minimal
RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" "$CARGO_DIR/bin/rustup" default "$RUST_VERSION"
RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" "$CARGO_DIR/bin/rustup" target add "$TARGET_TRIPLE" --toolchain "$RUST_VERSION"
INSTALLED_RUSTUP_VERSION=$(RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" \
  "$CARGO_DIR/bin/rustup" --version 2>/dev/null | sed -n 's/^rustup \([^ ]*\).*/\1/p')
if [ "$INSTALLED_RUSTUP_VERSION" != "$RUSTUP_VERSION" ]; then
  echo "workspace rustup version mismatch: expected $RUSTUP_VERSION, got $INSTALLED_RUSTUP_VERSION" >&2
  exit 4
fi
RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" RUSTUP_TOOLCHAIN="$RUST_VERSION" "$CARGO_DIR/bin/rustc" --version
RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" RUSTUP_TOOLCHAIN="$RUST_VERSION" "$CARGO_DIR/bin/cargo" --version
