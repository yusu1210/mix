#!/bin/sh
set -eu

BINARY=${1:?Mach-O binary path is required}
if [ ! -f "$BINARY" ] || [ ! -x "$BINARY" ]; then
  echo "executable is missing: $BINARY" >&2
  exit 2
fi
if ! /usr/bin/file "$BINARY" | grep -q 'Mach-O'; then
  echo "executable is not Mach-O: $BINARY" >&2
  exit 2
fi

/usr/bin/strip -x "$BINARY"
