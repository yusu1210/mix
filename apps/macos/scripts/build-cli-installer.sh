#!/bin/sh
set -eu

CLI_BUNDLE=${1:?signed mix-cli.app path is required}
OUTPUT_PACKAGE=${2:?output .pkg path is required}
VERSION=${3:?version is required}

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)

if [ -L "$CLI_BUNDLE" ] || [ ! -d "$CLI_BUNDLE" ] || [ "$(basename "$CLI_BUNDLE")" != mix-cli.app ]; then
  echo "expected a complete mix-cli.app bundle: $CLI_BUNDLE" >&2
  exit 2
fi
if [ ! -x "$CLI_BUNDLE/Contents/MacOS/mix" ] || [ ! -s "$CLI_BUNDLE/Contents/Resources/ui/index.html" ]; then
  echo "Mix CLI bundle is incomplete: $CLI_BUNDLE" >&2
  exit 2
fi
case "$OUTPUT_PACKAGE" in
  *.pkg) ;;
  *) echo "CLI installer output must end with .pkg" >&2; exit 2 ;;
esac
PACKAGE_VERSION=${VERSION%%[-+]*}
if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
  echo "invalid Mix version: $VERSION" >&2
  exit 2
fi
BINARY_ARCH=$(/usr/bin/lipo -archs "$CLI_BUNDLE/Contents/MacOS/mix")
case "$BINARY_ARCH" in
  arm64) ASSET_ARCH=aarch64 ;;
  x86_64) ASSET_ARCH=x86_64 ;;
  *) echo "Mix CLI installer requires one exact architecture: $BINARY_ARCH" >&2; exit 2 ;;
esac
EXPECTED_NAME="mix_${VERSION}_cli_${ASSET_ARCH}.pkg"
if [ "$(basename "$OUTPUT_PACKAGE")" != "$EXPECTED_NAME" ]; then
  echo "unexpected CLI installer name: $(basename "$OUTPUT_PACKAGE")" >&2
  exit 2
fi

OUTPUT_DIR=$(dirname "$OUTPUT_PACKAGE")
mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(CDPATH='' cd -- "$OUTPUT_DIR" && pwd)
OUTPUT_PACKAGE="$OUTPUT_DIR/$(basename "$OUTPUT_PACKAGE")"
STAGING=$(mktemp -d "$OUTPUT_DIR/.mix-cli-installer.XXXXXX")
cleanup() { rm -rf "$STAGING"; }
trap cleanup EXIT INT TERM

PAYLOAD="$STAGING/payload"
SCRIPTS="$STAGING/scripts"
INSTALL_ROOT="$PAYLOAD/Library/Application Support/Mix"
mkdir -p "$INSTALL_ROOT" "$PAYLOAD/usr/local/bin" "$SCRIPTS"
/usr/bin/ditto --norsrc "$CLI_BUNDLE" "$INSTALL_ROOT/mix-cli.app"
/usr/bin/codesign --verify --deep --strict "$INSTALL_ROOT/mix-cli.app"
/bin/ln -s "/Library/Application Support/Mix/mix-cli.app/Contents/MacOS/mix" \
  "$PAYLOAD/usr/local/bin/mix"
/bin/cp "$SCRIPT_DIR/cli-installer/uninstall" "$INSTALL_ROOT/uninstall-cli"
/bin/chmod 755 "$INSTALL_ROOT/uninstall-cli"
/bin/cp "$SCRIPT_DIR/cli-installer/preinstall" "$SCRIPTS/preinstall"
/bin/chmod 755 "$SCRIPTS/preinstall"

COMPONENT_PACKAGE="$STAGING/mix-cli-component.pkg"
set -- \
  --root "$PAYLOAD" \
  --scripts "$SCRIPTS" \
  --identifier dev.mix.cli.pkg \
  --version "$PACKAGE_VERSION" \
  --install-location / \
  --ownership recommended
/usr/bin/pkgbuild "$@" "$COMPONENT_PACKAGE"

REQUIREMENTS="$STAGING/requirements.plist"
/usr/libexec/PlistBuddy -c 'Clear dict' "$REQUIREMENTS"
/usr/libexec/PlistBuddy -c 'Add :os array' "$REQUIREMENTS"
/usr/libexec/PlistBuddy -c 'Add :os:0 string 13.0' "$REQUIREMENTS"
/usr/libexec/PlistBuddy -c 'Add :arch array' "$REQUIREMENTS"
/usr/libexec/PlistBuddy -c "Add :arch:0 string $BINARY_ARCH" "$REQUIREMENTS"

DISTRIBUTION="$STAGING/Distribution.xml"
/usr/bin/productbuild --synthesize \
  --product "$REQUIREMENTS" \
  --package "$COMPONENT_PACKAGE" \
  "$DISTRIBUTION"

PACKAGE="$STAGING/mix-cli.pkg"
set -- \
  --distribution "$DISTRIBUTION" \
  --package-path "$STAGING" \
  --identifier dev.mix.cli.product \
  --version "$PACKAGE_VERSION"
if [ -n "${APPLE_INSTALLER_SIGNING_IDENTITY:-}" ]; then
  set -- "$@" --sign "$APPLE_INSTALLER_SIGNING_IDENTITY" --timestamp
fi
/usr/bin/productbuild "$@" "$PACKAGE"

rm -f "$OUTPUT_PACKAGE"
mv "$PACKAGE" "$OUTPUT_PACKAGE"
printf 'built Mix CLI installer: %s\n' "$OUTPUT_PACKAGE"
