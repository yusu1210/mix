#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
ARCHIVE=${1:?updater archive path is required}
SIGNATURE=${2:?updater signature path is required}
EXPECTED_ARCH=${3:?expected architecture is required: arm64 or x86_64}
SIGNING_MODE=${4:-local}
: "${MIX_UPDATER_PUBKEY:?MIX_UPDATER_PUBKEY is required for cryptographic verification}"

case "$EXPECTED_ARCH" in
  arm64|x86_64) ;;
  *) echo "unsupported architecture: $EXPECTED_ARCH" >&2; exit 2 ;;
esac
case "$SIGNING_MODE" in
  local|release) ;;
  *) echo "signing mode must be local or release" >&2; exit 2 ;;
esac

ARCHIVE=$(CDPATH='' cd -- "$(dirname -- "$ARCHIVE")" && pwd)/$(basename "$ARCHIVE")
SIGNATURE=$(CDPATH='' cd -- "$(dirname -- "$SIGNATURE")" && pwd)/$(basename "$SIGNATURE")
test -s "$ARCHIVE"
test -s "$SIGNATURE"

if /usr/bin/tar -tzf "$ARCHIVE" | awk 'BEGIN {bad=0} /^\// || /(^|\/)\.\.($|\/)/ {bad=1} END {exit !bad}'; then
  echo "updater archive contains an unsafe path" >&2
  exit 3
fi

TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mix-update-verify.XXXXXX")
cleanup() { rm -rf "$TEMP_DIR"; }
trap cleanup EXIT INT TERM
/usr/bin/tar -xzf "$ARCHIVE" -C "$TEMP_DIR"
APP_BUNDLE="$TEMP_DIR/mix.app"
test -d "$APP_BUNDLE"
test -f "$APP_BUNDLE/Contents/Resources/icon.icns"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
sh "$PROJECT_DIR/scripts/verify-macos-architecture.sh" "$APP_BUNDLE" "$EXPECTED_ARCH"

. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_DIR="$MIX_RUSTUP_HOME"
CARGO_DIR="$MIX_CARGO_HOME"
if [ ! -x "$CARGO_DIR/bin/cargo" ]; then
  echo "Mix Rust toolchain is required for updater signature verification" >&2
  exit 3
fi
RUSTUP_HOME="$RUSTUP_DIR" CARGO_HOME="$CARGO_DIR" RUSTUP_TOOLCHAIN=1.98.0 \
  "$CARGO_DIR/bin/cargo" run --quiet --locked \
  --manifest-path "$PROJECT_DIR/apps/macos/src-tauri/Cargo.toml" \
  --example verify_update_signature -- \
  "$MIX_UPDATER_PUBKEY" "$SIGNATURE" "$ARCHIVE"

if [ "$SIGNING_MODE" = release ]; then
  APP_SIGNATURE=$(/usr/bin/codesign -dvvv "$APP_BUNDLE" 2>&1)
  printf '%s\n' "$APP_SIGNATURE" | grep -F "Authority=Developer ID Application:" >/dev/null
  printf '%s\n' "$APP_SIGNATURE" | grep -E 'flags=.*runtime' >/dev/null
  printf '%s\n' "$APP_SIGNATURE" | grep -F "Timestamp=" >/dev/null
  APP_TEAM=$(printf '%s\n' "$APP_SIGNATURE" | sed -n 's/^TeamIdentifier=//p' | head -1)
  if [ -z "$APP_TEAM" ]; then
    echo "release bundle has no Apple TeamIdentifier" >&2
    exit 4
  fi
  /usr/bin/xcrun stapler validate "$APP_BUNDLE"
  /usr/sbin/spctl --assess --type execute --verbose=2 "$APP_BUNDLE"
fi

printf 'verified update=%s arch=%s signing=%s\n' "$ARCHIVE" "$EXPECTED_ARCH" "$SIGNING_MODE"
