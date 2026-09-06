#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-/tmp/mix-target}
TARGET_MARKER="$TARGET_DIR/.mix-target-owned"

case "$TARGET_DIR" in
  /*) ;;
  *) echo "CARGO_TARGET_DIR must be an absolute path" >&2; exit 3 ;;
esac

# Cleaning is filesystem housekeeping. It must not require a Rust toolchain,
# and it must remove the same disposable directory used by release builds.
case "$TARGET_DIR" in
  /|/tmp|/tmp/|"$PROJECT_DIR"|"$PROJECT_DIR/"|"${HOME:-}"|"${HOME:-}/")
    echo "refusing to clean an unsafe Cargo target directory: $TARGET_DIR" >&2
    exit 3
    ;;
esac
if [ -L "$TARGET_DIR" ]; then
  echo "refusing to clean an unsafe Cargo target directory: $TARGET_DIR" >&2
  exit 3
fi
if [ -e "$TARGET_DIR" ] && [ "$TARGET_DIR" != "/tmp/mix-target" ] && [ ! -f "$TARGET_MARKER" ]; then
  echo "refusing to clean an unowned Cargo target directory: $TARGET_DIR" >&2
  exit 3
fi

for RUNNING_ROOT in "$TARGET_DIR" "$PROJECT_DIR/target"; do
  if ps -axo pid=,args= 2>/dev/null | awk \
    -v root="$RUNNING_ROOT" \
    '{ $1 = ""; sub(/^[[:space:]]+/, ""); if (index($0, root) == 1) found = 1 }
     END { exit(found ? 0 : 1) }'; then
    echo "refusing to clean while a process uses the Cargo target directory: $RUNNING_ROOT" >&2
    exit 3
  fi
done
rm -rf -- "$TARGET_DIR"

# Older builds may have used Cargo's repository-local default before the
# project moved its disposable target directory outside the checkout.
if [ -d "$PROJECT_DIR/target" ] || [ -L "$PROJECT_DIR/target" ]; then
  rm -rf -- "$PROJECT_DIR/target"
fi

printf '%s\n' "Cleaned Cargo build artifacts; source, app bundles, and local data were kept."
