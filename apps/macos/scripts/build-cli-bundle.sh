#!/bin/sh
set -eu

CLI_BINARY=${1:?CLI binary path is required}
UI_ROOT=${2:?UI distribution path is required}
OUTPUT_BUNDLE=${3:?output mix-cli.app path is required}
VERSION=${4:?version is required}

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
APP_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)

if [ ! -f "$CLI_BINARY" ] || [ ! -x "$CLI_BINARY" ]; then
  echo "Mix CLI executable is missing: $CLI_BINARY" >&2
  exit 2
fi
if [ ! -d "$UI_ROOT" ] || [ ! -f "$UI_ROOT/index.html" ]; then
  echo "Mix visual interface is incomplete: $UI_ROOT" >&2
  exit 2
fi
if [ "$(basename "$OUTPUT_BUNDLE")" != mix-cli.app ]; then
  echo "CLI bundle must be named mix-cli.app: $OUTPUT_BUNDLE" >&2
  exit 2
fi
case "$VERSION" in
  ''|*[!0-9A-Za-z.+-]*) echo "invalid Mix version: $VERSION" >&2; exit 2 ;;
esac
BUILD_VERSION=${VERSION%%[-+]*}
if ! printf '%s\n' "$BUILD_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "invalid numeric bundle version: $VERSION" >&2
  exit 2
fi

OUTPUT_PARENT=$(dirname "$OUTPUT_BUNDLE")
mkdir -p "$OUTPUT_PARENT"
OUTPUT_PARENT=$(CDPATH='' cd -- "$OUTPUT_PARENT" && pwd)
OUTPUT_BUNDLE="$OUTPUT_PARENT/mix-cli.app"
STAGING=$(mktemp -d "$OUTPUT_PARENT/.mix-cli.XXXXXX")
cleanup() {
  rm -rf "$STAGING"
}
trap cleanup EXIT INT TERM

CONTENTS="$STAGING/mix-cli.app/Contents"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources/ui"
/usr/bin/ditto --norsrc "$CLI_BINARY" "$CONTENTS/MacOS/mix"
chmod 755 "$CONTENTS/MacOS/mix"
/usr/bin/ditto --norsrc "$UI_ROOT" "$CONTENTS/Resources/ui"
/usr/bin/ditto --norsrc "$APP_DIR/src-tauri/cli-Info.plist" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $BUILD_VERSION" "$CONTENTS/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_VERSION" "$CONTENTS/Info.plist"

sh "$SCRIPT_DIR/sign-macos.sh" "$STAGING/mix-cli.app"

rm -rf "$OUTPUT_BUNDLE"
mv "$STAGING/mix-cli.app" "$OUTPUT_BUNDLE"
printf 'built Mix CLI bundle: %s\n' "$OUTPUT_BUNDLE"
