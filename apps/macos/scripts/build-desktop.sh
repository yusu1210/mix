#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$APP_DIR/../.." && pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/mix-target}
case "$TARGET_DIR" in
  /*) ;;
  *) echo "CARGO_TARGET_DIR must be an absolute path" >&2; exit 3 ;;
esac
LOCAL_OUTPUT_ROOT=${MIX_LOCAL_OUTPUT_ROOT:-$PROJECT_DIR/.build}
case "$LOCAL_OUTPUT_ROOT" in
  /*) ;;
  *) echo "MIX_LOCAL_OUTPUT_ROOT must be an absolute path" >&2; exit 3 ;;
esac
if [ -L "$LOCAL_OUTPUT_ROOT" ]; then
  echo "MIX_LOCAL_OUTPUT_ROOT must not be a symbolic link" >&2
  exit 3
fi
mkdir -p "$LOCAL_OUTPUT_ROOT"
LOCAL_OUTPUT_ROOT=$(CDPATH='' cd -- "$LOCAL_OUTPUT_ROOT" && pwd)
export CARGO_TARGET_DIR="$TARGET_DIR"
mkdir -p "$TARGET_DIR"
touch "$TARGET_DIR/.mix-target-owned"
AVAILABLE_KIB=$(df -Pk "$TARGET_DIR" | awk 'END { print $4 }')
MINIMUM_KIB=4194304
case "$AVAILABLE_KIB" in
  ''|*[!0-9]*) echo "cannot determine available build disk space" >&2; exit 2 ;;
esac
if [ "$AVAILABLE_KIB" -lt "$MINIMUM_KIB" ]; then
  echo "desktop release build requires at least 4 GiB free in $TARGET_DIR" >&2
  exit 2
fi
. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_DIR="$MIX_RUSTUP_HOME"
CARGO_DIR="$MIX_CARGO_HOME"
RUST_VERSION=${MIX_RUST_TOOLCHAIN:-1.98.0}
HOST_ARCH=$(uname -m)
BUILD_ARCH=${MIX_BUILD_ARCH:-$HOST_ARCH}

case "$BUILD_ARCH" in
  arm64) TARGET_TRIPLE=aarch64-apple-darwin; RELEASE_ARCH=aarch64 ;;
  x86_64) TARGET_TRIPLE=x86_64-apple-darwin; RELEASE_ARCH=x86_64 ;;
  *) echo "unsupported release architecture: $BUILD_ARCH" >&2; exit 3 ;;
esac

if [ "$BUILD_ARCH" != "$HOST_ARCH" ]; then
  if [ "$HOST_ARCH:$BUILD_ARCH" != "arm64:x86_64" ]; then
    echo "unsupported desktop cross-build: $HOST_ARCH -> $BUILD_ARCH" >&2
    exit 3
  fi
  /usr/bin/arch -x86_64 /usr/bin/true
fi
export MIX_BUILD_ARCH

if [ ! -x "$CARGO_DIR/bin/cargo" ]; then
  echo "Mix Rust toolchain is missing; run scripts/bootstrap-rust.sh first" >&2
  exit 2
fi

export RUSTUP_HOME="$RUSTUP_DIR"
export CARGO_HOME="$CARGO_DIR"
export RUSTUP_TOOLCHAIN="$RUST_VERSION"
export PATH="$CARGO_DIR/bin:$PATH"
. "$SCRIPT_DIR/configure-rust-release.sh"

sh "$PROJECT_DIR/scripts/verify-node.sh"

UPDATER_CONFIG=
UPDATER_TEMP_DIR=
cleanup() {
  if [ -n "$UPDATER_TEMP_DIR" ] && [ -d "$UPDATER_TEMP_DIR" ]; then
    rm -rf "$UPDATER_TEMP_DIR"
  fi
}
trap cleanup EXIT INT TERM

if [ -n "${MIX_CODESIGN_IDENTITY:-}" ]; then
  export APPLE_SIGNING_IDENTITY="$MIX_CODESIGN_IDENTITY"
fi

cd "$APP_DIR"
npm run build
sh scripts/generate-third-party-licenses.sh
test -s src-tauri/resources/THIRD-PARTY-LICENSES.html
case "${MIX_UPDATER_ENABLED:-0}" in
  0)
    if [ -n "${MIX_UPDATER_PUBKEY:-}" ] || [ -n "${MIX_UPDATE_ENDPOINT:-}" ] || [ -n "${MIX_RELEASE_PAGE_URL:-}" ]; then
      echo "updater values were provided while MIX_UPDATER_ENABLED is not 1" >&2
      exit 3
    fi
    if [ "$BUILD_ARCH" = "$HOST_ARCH" ]; then
      npm run tauri build -- --bundles app
    else
      npm run tauri build -- --target "$TARGET_TRIPLE" --bundles app
    fi
    ;;
  1)
    : "${TAURI_SIGNING_PRIVATE_KEY:?TAURI_SIGNING_PRIVATE_KEY is required for updater artifacts}"
    : "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:?TAURI_SIGNING_PRIVATE_KEY_PASSWORD is required for updater artifacts}"
    : "${MIX_UPDATER_PUBKEY:?MIX_UPDATER_PUBKEY is required for update checks}"
    : "${MIX_UPDATE_ENDPOINT:?MIX_UPDATE_ENDPOINT is required for update checks}"
    : "${MIX_RELEASE_PAGE_URL:?MIX_RELEASE_PAGE_URL is required for manual update downloads}"
    RELEASE_TOOL=${MIX_RELEASE_TOOL:-$TARGET_DIR/release/mix-release}
    if [ ! -x "$RELEASE_TOOL" ]; then
      "$CARGO_DIR/bin/cargo" build --locked --release --package mix-release
    fi
    if [ ! -x "$RELEASE_TOOL" ]; then
      echo "Mix release tool is missing: $RELEASE_TOOL" >&2
      exit 2
    fi
    UPDATER_TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mix-updater-config.XXXXXX")
    UPDATER_CONFIG="$UPDATER_TEMP_DIR/config.json"
    "$RELEASE_TOOL" updater config "$UPDATER_CONFIG"
    if [ "$BUILD_ARCH" = "$HOST_ARCH" ]; then
      npm run tauri build -- --bundles app --config "$UPDATER_CONFIG"
    else
      npm run tauri build -- --target "$TARGET_TRIPLE" --bundles app --config "$UPDATER_CONFIG"
    fi
    ;;
  *)
    echo "MIX_UPDATER_ENABLED must be 0 or 1" >&2
    exit 3
    ;;
esac

if [ "$BUILD_ARCH" = "$HOST_ARCH" ]; then
  BUNDLE_DIR="$TARGET_DIR/release/bundle"
else
  BUNDLE_DIR="$TARGET_DIR/$TARGET_TRIPLE/release/bundle"
fi
APP_BUNDLE="$BUNDLE_DIR/macos/mix.app"
if [ "${MIX_UPDATER_ENABLED:-0}" = 0 ]; then
  # A local build must not leave a previously generated, differently keyed
  # updater archive beside the current release candidate.
  rm -f "$BUNDLE_DIR/macos/mix.app.tar.gz" "$BUNDLE_DIR/macos/mix.app.tar.gz.sig"
fi
sh scripts/strip-macos.sh "$APP_BUNDLE/Contents/MacOS/mix-macos"
sh scripts/sign-macos.sh "$APP_BUNDLE"
if [ "${MIX_UPDATER_ENABLED:-0}" = 1 ]; then
  # Tauri creates its first updater archive before our final whole-bundle
  # signature. Recreate it from the completed app so the installed update has
  # the same valid bundle signature and resources as the DMG.
  sh scripts/build-macos-updater.sh "$APP_BUNDLE" "$BUNDLE_DIR/macos/mix.app.tar.gz"
fi
VERSION=$(node -e 'const fs=require("fs"); console.log(JSON.parse(fs.readFileSync(process.argv[1],"utf8")).version)' "$APP_DIR/package.json")
DMG_PATH="$BUNDLE_DIR/dmg/mix_${VERSION}_${RELEASE_ARCH}.dmg"
sh scripts/build-dmg.sh "$APP_BUNDLE" "$DMG_PATH" mix

if [ "$BUILD_ARCH" = "$HOST_ARCH" ]; then
  LOCAL_APP_DIR="$LOCAL_OUTPUT_ROOT/local-app"
else
  LOCAL_APP_DIR="$LOCAL_OUTPUT_ROOT/local-app-$BUILD_ARCH"
fi
LOCAL_ARTIFACT_DIR="$LOCAL_OUTPUT_ROOT/local-artifacts"
mkdir -p "$LOCAL_APP_DIR" "$LOCAL_ARTIFACT_DIR"
rm -rf "$LOCAL_APP_DIR/mix.app"
ditto --norsrc "$APP_BUNDLE" "$LOCAL_APP_DIR/mix.app"
ditto --norsrc "$DMG_PATH" "$LOCAL_ARTIFACT_DIR/$(basename "$DMG_PATH")"
printf 'copied local app: %s\n' "$LOCAL_APP_DIR/mix.app"
printf 'copied local dmg: %s\n' "$LOCAL_ARTIFACT_DIR/$(basename "$DMG_PATH")"
