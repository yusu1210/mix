#!/bin/sh
set -eu

APP_BUNDLE=${1:?application bundle path is required}
OUTPUT_PATH=${2:?output dmg path is required}
VOLUME_NAME=${3:-mix}

if [ ! -d "$APP_BUNDLE" ]; then
  echo "application bundle does not exist: $APP_BUNDLE" >&2
  exit 2
fi

OUTPUT_DIR=$(dirname "$OUTPUT_PATH")
mkdir -p "$OUTPUT_DIR"
STAGING_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mix-dmg.XXXXXX")
trap 'rm -rf "$STAGING_DIR"' EXIT INT TERM

ditto "$APP_BUNDLE" "$STAGING_DIR/$(basename "$APP_BUNDLE")"
ln -s /Applications "$STAGING_DIR/Applications"

# Keep layout independent from Finder. The signed and notarized DMG is
# verifiable, but secure timestamps and filesystem identifiers make it
# intentionally non-byte-reproducible.
hdiutil create \
  -srcfolder "$STAGING_DIR" \
  -volname "$VOLUME_NAME" \
  -format UDZO \
  -ov \
  "$OUTPUT_PATH"

echo "$OUTPUT_PATH"
