#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DMG_PATH=${1:?DMG path is required}
EXPECTED_ARCH=${2:?expected architecture is required: arm64 or x86_64}
SIGNING_MODE=${3:-local}

case "$EXPECTED_ARCH" in
  arm64|x86_64) ;;
  *) echo "unsupported architecture: $EXPECTED_ARCH" >&2; exit 2 ;;
esac
case "$SIGNING_MODE" in
  local|release) ;;
  *) echo "signing mode must be local or release" >&2; exit 2 ;;
esac

DMG_PATH=$(CDPATH='' cd -- "$(dirname -- "$DMG_PATH")" && pwd)/$(basename "$DMG_PATH")
hdiutil verify "$DMG_PATH"
ATTACH_OUTPUT=$(hdiutil attach -readonly -nobrowse "$DMG_PATH")
MOUNT_PATH=$(printf '%s\n' "$ATTACH_OUTPUT" | awk -F '\t' 'NF >= 3 && $NF ~ /^\/Volumes\// {value=$NF} END {print value}')
if [ -z "$MOUNT_PATH" ]; then
  echo "DMG did not expose a mounted volume" >&2
  exit 3
fi
cleanup() { hdiutil detach "$MOUNT_PATH" >/dev/null; }
trap cleanup EXIT INT TERM

APP_BUNDLE="$MOUNT_PATH/mix.app"
test -d "$APP_BUNDLE"
test "$(/usr/bin/readlink "$MOUNT_PATH/Applications")" = /Applications
test -f "$APP_BUNDLE/Contents/Resources/icon.icns"
LICENSE_EVIDENCE="$APP_BUNDLE/Contents/Resources/THIRD-PARTY-LICENSES.html"
test -s "$LICENSE_EVIDENCE"
grep -F "Mix third-party licenses" "$LICENSE_EVIDENCE" >/dev/null
if grep -E '/Users/|node_modules|\.tools/' "$LICENSE_EVIDENCE" >/dev/null; then
  echo "bundled license evidence leaks a build path" >&2
  exit 4
fi
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
sh "$SCRIPT_DIR/verify-macos-architecture.sh" "$APP_BUNDLE" "$EXPECTED_ARCH"
if /usr/bin/grep -R -a -l -E '/Users/|/home/|/var/folders/|[A-Za-z]:\\\\Users\\\\' "$APP_BUNDLE" >/dev/null; then
  echo "application bundle leaks a build-machine path" >&2
  exit 4
fi
if [ "$SIGNING_MODE" = release ]; then
  SIGNATURE=$(/usr/bin/codesign -dvvv "$APP_BUNDLE" 2>&1)
  printf '%s\n' "$SIGNATURE" | grep -F "Authority=Developer ID Application:" >/dev/null
  printf '%s\n' "$SIGNATURE" | grep -E 'flags=.*runtime' >/dev/null
  printf '%s\n' "$SIGNATURE" | grep -F "Timestamp=" >/dev/null
  if printf '%s\n' "$SIGNATURE" | grep -F "TeamIdentifier=not set" >/dev/null; then
    echo "release bundle has no Apple TeamIdentifier" >&2
    exit 4
  fi
  APP_TEAM=$(printf '%s\n' "$SIGNATURE" | sed -n 's/^TeamIdentifier=//p' | head -1)
  if [ -z "$APP_TEAM" ]; then
    echo "release bundle has no Apple TeamIdentifier" >&2
    exit 4
  fi
  /usr/bin/xcrun stapler validate "$DMG_PATH"
  /usr/sbin/spctl --assess --type execute --verbose=2 "$APP_BUNDLE"
  /usr/sbin/spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG_PATH"
fi

printf 'verified dmg=%s arch=%s signing=%s\n' "$DMG_PATH" "$EXPECTED_ARCH" "$SIGNING_MODE"
