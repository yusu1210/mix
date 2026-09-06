#!/bin/sh
set -eu

APP_BUNDLE=${1:?application bundle path is required}
if [ ! -d "$APP_BUNDLE" ]; then
  echo "incomplete application bundle: $APP_BUNDLE" >&2
  exit 2
fi

if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
  BUNDLE_ID=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP_BUNDLE/Contents/Info.plist")
  case "$BUNDLE_ID" in
    dev.mix.desktop|dev.mix.cli) ;;
    *) echo "unsupported Mix bundle identifier: $BUNDLE_ID" >&2; exit 2 ;;
  esac
  /usr/bin/codesign --force --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$APP_BUNDLE"
else
  /usr/bin/codesign --force --options runtime --sign - "$APP_BUNDLE"
fi
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
