#!/bin/sh
set -eu

APP_BUNDLE=${1:?application bundle path is required}
EXPECTED_ARCH=${2:?expected architecture is required: arm64 or x86_64}

case "$EXPECTED_ARCH" in
  arm64|x86_64) ;;
  *) echo "unsupported architecture: $EXPECTED_ARCH" >&2; exit 2 ;;
esac
if [ ! -d "$APP_BUNDLE/Contents" ]; then
  echo "incomplete application bundle: $APP_BUNDLE" >&2
  exit 2
fi

MACHO_TYPES=$(
  find "$APP_BUNDLE/Contents" -type f -exec /usr/bin/file -b {} + \
    | grep '^Mach-O' || true
)
if [ -z "$MACHO_TYPES" ]; then
  echo "application bundle contains no Mach-O files: $APP_BUNDLE" >&2
  exit 3
fi
if printf '%s\n' "$MACHO_TYPES" | grep -F 'universal binary' >/dev/null; then
  echo "architecture-specific bundle contains a universal Mach-O file" >&2
  exit 3
fi
if printf '%s\n' "$MACHO_TYPES" | grep -Fv "$EXPECTED_ARCH" >/dev/null; then
  echo "application bundle contains a Mach-O file for another architecture" >&2
  exit 3
fi
case "$EXPECTED_ARCH" in
  arm64) FORBIDDEN_ARCH=x86_64 ;;
  x86_64) FORBIDDEN_ARCH=arm64 ;;
esac
if printf '%s\n' "$MACHO_TYPES" | grep -F "$FORBIDDEN_ARCH" >/dev/null; then
  echo "application bundle mixes $EXPECTED_ARCH and $FORBIDDEN_ARCH Mach-O files" >&2
  exit 3
fi

printf 'verified Mach-O tree: bundle=%s arch=%s files=%s\n' \
  "$APP_BUNDLE" "$EXPECTED_ARCH" "$(printf '%s\n' "$MACHO_TYPES" | wc -l | tr -d ' ')"
