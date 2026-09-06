#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
PACKAGE=${1:?CLI installer package path is required}
EXPECTED_ARCH=${2:?expected architecture is required: arm64 or x86_64}
SIGNING_MODE=${3:-local}
EXPECTED_VERSION=${4:?expected semantic version is required}

case "$EXPECTED_ARCH" in
  arm64|x86_64) ;;
  *) echo "unsupported architecture: $EXPECTED_ARCH" >&2; exit 2 ;;
esac
case "$SIGNING_MODE" in
  local|release) ;;
  *) echo "signing mode must be local or release" >&2; exit 2 ;;
esac
PACKAGE_VERSION=${EXPECTED_VERSION%%[-+]*}
if ! printf '%s\n' "$EXPECTED_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
  echo "invalid expected version: $EXPECTED_VERSION" >&2
  exit 2
fi
case "$EXPECTED_ARCH" in
  arm64) ASSET_ARCH=aarch64 ;;
  x86_64) ASSET_ARCH=x86_64 ;;
esac
EXPECTED_NAME="mix_${EXPECTED_VERSION}_cli_${ASSET_ARCH}.pkg"
if [ "$(basename "$PACKAGE")" != "$EXPECTED_NAME" ]; then
  echo "unexpected CLI installer name: $(basename "$PACKAGE")" >&2
  exit 2
fi
if [ ! -s "$PACKAGE" ]; then
  echo "CLI installer is missing: $PACKAGE" >&2
  exit 2
fi
PACKAGE=$(CDPATH='' cd -- "$(dirname -- "$PACKAGE")" && pwd)/$(basename "$PACKAGE")

TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/mix-cli-installer.XXXXXX")
WEB_PID=
cleanup() {
  if [ -n "$WEB_PID" ] && kill -0 "$WEB_PID" 2>/dev/null; then
    kill "$WEB_PID" 2>/dev/null || true
    wait "$WEB_PID" 2>/dev/null || true
  fi
  rm -rf "$TEMP_DIR"
}
trap cleanup EXIT INT TERM

/usr/sbin/pkgutil --expand-full "$PACKAGE" "$TEMP_DIR/expanded"
grep -F "hostArchitectures=\"$EXPECTED_ARCH\"" "$TEMP_DIR/expanded/Distribution" >/dev/null
grep -F '13.0' "$TEMP_DIR/expanded/Distribution" >/dev/null
grep -F "<product id=\"dev.mix.cli.product\" version=\"$PACKAGE_VERSION\"/>" "$TEMP_DIR/expanded/Distribution" >/dev/null
PACKAGE_ROOT="$TEMP_DIR/expanded/mix-cli-component.pkg"
PACKAGE_INFO="$PACKAGE_ROOT/PackageInfo"
grep -F 'identifier="dev.mix.cli.pkg"' "$PACKAGE_INFO" >/dev/null
grep -F 'install-location="/"' "$PACKAGE_INFO" >/dev/null
grep -F "version=\"$PACKAGE_VERSION\"" "$PACKAGE_INFO" >/dev/null
grep -F '<preinstall file="./preinstall"' "$PACKAGE_INFO" >/dev/null

PAYLOAD="$PACKAGE_ROOT/Payload"
APP_BUNDLE="$PAYLOAD/Library/Application Support/Mix/mix-cli.app"
UNINSTALLER="$PAYLOAD/Library/Application Support/Mix/uninstall-cli"
COMMAND_LINK="$PAYLOAD/usr/local/bin/mix"
CLI="$APP_BUNDLE/Contents/MacOS/mix"
test -x "$CLI"
test -x "$UNINSTALLER"
sh -n "$UNINSTALLER"
grep -F 'Run this uninstaller with sudo.' "$UNINSTALLER" >/dev/null
grep -F 'not the Mix-managed command link' "$UNINSTALLER" >/dev/null
grep -F 'bundle identifier is not dev.mix.cli' "$UNINSTALLER" >/dev/null
grep -F 'Mix data and native Codex/Claude history were kept' "$UNINSTALLER" >/dev/null
test -s "$APP_BUNDLE/Contents/Resources/ui/index.html"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_BUNDLE/Contents/Info.plist")" = "$PACKAGE_VERSION"
test -L "$COMMAND_LINK"
test "$(/usr/bin/readlink "$COMMAND_LINK")" = "/Library/Application Support/Mix/mix-cli.app/Contents/MacOS/mix"
PREINSTALL="$PACKAGE_ROOT/Scripts/preinstall"
grep -F 'already exists and is not managed by Mix' "$PREINSTALL" >/dev/null
grep -F 'can only be installed on the current macOS startup volume' "$PREINSTALL" >/dev/null
grep -F 'symbolic-link directory' "$PREINSTALL" >/dev/null
grep -F 'is not a regular directory' "$PREINSTALL" >/dev/null
grep -F 'is not a regular application directory' "$PREINSTALL" >/dev/null
grep -F 'is not owned by Mix' "$PREINSTALL" >/dev/null
grep -F 'is not a regular file' "$PREINSTALL" >/dev/null
grep -F 'is not associated with an installed Mix CLI application' "$PREINSTALL" >/dev/null
if "$PREINSTALL" unused / "$TEMP_DIR/alternate-volume" >/dev/null 2>&1; then
  echo "CLI installer accepted an alternate target volume" >&2
  exit 4
fi

/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
sh "$PROJECT_DIR/scripts/verify-macos-architecture.sh" "$APP_BUNDLE" "$EXPECTED_ARCH"
if /usr/bin/grep -R -a -l -E '/Users/|/home/|/var/folders/|[A-Za-z]:\\\\Users\\\\' "$APP_BUNDLE" >/dev/null; then
  echo "CLI installer leaks a build-machine path" >&2
  exit 4
fi
"$CLI" --help | grep -F "Switch AI coding accounts without losing native sessions" >/dev/null

CONFIG_ROOT=$(mktemp -d "$TEMP_DIR/config.XXXXXX")
STATE=$("$CLI" --config "$CONFIG_ROOT/config.json" --json status)
if [ "$SIGNING_MODE" = release ]; then
  printf '%s\n' "$STATE" | grep -E '"kind"[[:space:]]*:[[:space:]]*"local-files"' >/dev/null
  SIGNATURE=$(/usr/bin/codesign -dvvv "$APP_BUNDLE" 2>&1)
  printf '%s\n' "$SIGNATURE" | grep -F "Authority=Developer ID Application:" >/dev/null
  printf '%s\n' "$SIGNATURE" | grep -E 'flags=.*runtime' >/dev/null
  printf '%s\n' "$SIGNATURE" | grep -F "Timestamp=" >/dev/null
  TEAM=$(printf '%s\n' "$SIGNATURE" | sed -n 's/^TeamIdentifier=//p' | head -1)
  if [ -z "$TEAM" ] || [ "$TEAM" = "not set" ]; then
    echo "release CLI bundle has no Apple TeamIdentifier" >&2
    exit 4
  fi
  PACKAGE_SIGNATURE=$(/usr/sbin/pkgutil --check-signature "$PACKAGE")
  printf '%s\n' "$PACKAGE_SIGNATURE" | grep -F "Developer ID Installer:" >/dev/null
  printf '%s\n' "$PACKAGE_SIGNATURE" | grep -F "($TEAM)" >/dev/null
  /usr/bin/xcrun stapler validate "$PACKAGE"
  /usr/bin/xcrun stapler validate "$APP_BUNDLE"
  /usr/sbin/spctl --assess --type install --verbose=2 "$PACKAGE"
else
  printf '%s\n' "$STATE" | grep -E '"kind"[[:space:]]*:[[:space:]]*"local-files"' >/dev/null
  if [ -e "$APP_BUNDLE/Contents/embedded.provisionprofile" ]; then
    echo "local CLI bundle unexpectedly contains a provisioning profile" >&2
    exit 4
  fi
fi

WEB_OUTPUT="$TEMP_DIR/local-web.stdout"
WEB_ERROR="$TEMP_DIR/local-web.stderr"
"$CLI" --config "$CONFIG_ROOT/web-config.json" web --no-open --port 0 >"$WEB_OUTPUT" 2>"$WEB_ERROR" &
WEB_PID=$!
WEB_URL=
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
  if ! kill -0 "$WEB_PID" 2>/dev/null; then
    echo "packaged Local Web exited during startup" >&2
    sed -n '1,40p' "$WEB_ERROR" >&2
    exit 4
  fi
  WEB_URL=$(sed -n 's/^Mix Local Web: //p' "$WEB_OUTPUT" | head -1)
  if [ -n "$WEB_URL" ]; then break; fi
  sleep 0.1
done
case "$WEB_URL" in
  http://127.0.0.1:*'/#token='*) ;;
  *) echo "packaged Local Web did not publish a safe authenticated URL" >&2; exit 4 ;;
esac
WEB_ORIGIN=${WEB_URL%%/#token=*}
WEB_TOKEN=${WEB_URL#*#token=}
if [ "${#WEB_TOKEN}" -ne 64 ] || printf '%s' "$WEB_TOKEN" | grep -Eq '[^0-9a-f]'; then
  echo "packaged Local Web produced an invalid launch token" >&2
  exit 4
fi

INDEX="$TEMP_DIR/local-web.index"
HEADERS="$TEMP_DIR/local-web.headers"
/usr/bin/curl --fail --silent --show-error --dump-header "$HEADERS" --output "$INDEX" "$WEB_ORIGIN/"
grep -F '<title>mix</title>' "$INDEX" >/dev/null
grep -Eiq '^cache-control:[[:space:]]*no-store' "$HEADERS"
grep -Eiq "^content-security-policy:.*frame-ancestors 'none'" "$HEADERS"
UNAUTHORIZED=$(/usr/bin/curl --silent --output /dev/null --write-out '%{http_code}' "$WEB_ORIGIN/api/state")
test "$UNAUTHORIZED" = 401
AUTHENTICATED=$(/usr/bin/curl --fail --silent --show-error --header "x-mix-token: $WEB_TOKEN" "$WEB_ORIGIN/api/state")
printf '%s\n' "$AUTHENTICATED" | grep -Eq '"needs_setup"[[:space:]]*:[[:space:]]*true'
DENIED_ORIGIN=$(/usr/bin/curl --silent --output /dev/null --write-out '%{http_code}' \
  --request POST \
  --header "x-mix-token: $WEB_TOKEN" \
  --header 'content-type: application/json' \
  --header 'origin: https://example.invalid' \
  --data '{}' \
  "$WEB_ORIGIN/api/apps")
test "$DENIED_ORIGIN" = 403
kill "$WEB_PID"
wait "$WEB_PID" 2>/dev/null || true
WEB_PID=

printf 'verified CLI installer=%s arch=%s signing=%s\n' "$PACKAGE" "$EXPECTED_ARCH" "$SIGNING_MODE"
