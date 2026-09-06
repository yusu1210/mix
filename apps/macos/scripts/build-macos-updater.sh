#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/mix-target}
case "$TARGET_DIR" in
  /*) ;;
  *) echo "CARGO_TARGET_DIR must be an absolute path" >&2; exit 3 ;;
esac
APP_BUNDLE=${1:?application bundle path is required}
OUTPUT_ARCHIVE=${2:?output .app.tar.gz path is required}

: "${TAURI_SIGNING_PRIVATE_KEY:?TAURI_SIGNING_PRIVATE_KEY is required}"
: "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:?TAURI_SIGNING_PRIVATE_KEY_PASSWORD is required}"

if [ ! -d "$APP_BUNDLE" ] || [ "$(basename "$APP_BUNDLE")" != mix.app ]; then
  echo "expected a complete mix.app bundle: $APP_BUNDLE" >&2
  exit 2
fi
case "$OUTPUT_ARCHIVE" in
  *.app.tar.gz) ;;
  *) echo "updater output must end with .app.tar.gz" >&2; exit 2 ;;
esac

APP_PARENT=$(CDPATH='' cd -- "$(dirname -- "$APP_BUNDLE")" && pwd)
OUTPUT_DIR=$(dirname "$OUTPUT_ARCHIVE")
mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(CDPATH='' cd -- "$OUTPUT_DIR" && pwd)
OUTPUT_ARCHIVE="$OUTPUT_DIR/$(basename "$OUTPUT_ARCHIVE")"

rm -f "$OUTPUT_ARCHIVE" "$OUTPUT_ARCHIVE.sig"
RELEASE_TOOL=${MIX_RELEASE_TOOL:-$TARGET_DIR/release/mix-release}
if [ ! -x "$RELEASE_TOOL" ]; then
  echo "Mix release tool is missing: $RELEASE_TOOL" >&2
  exit 2
fi
"$RELEASE_TOOL" archive create --input "$APP_PARENT/mix.app" --output "$OUTPUT_ARCHIVE"

cd "$APP_DIR"
npm run tauri signer sign -- "$OUTPUT_ARCHIVE"
test -s "$OUTPUT_ARCHIVE"
test -s "$OUTPUT_ARCHIVE.sig"
printf 'built signed updater archive: %s\n' "$OUTPUT_ARCHIVE"
