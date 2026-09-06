#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
EXPECTED_NODE=$(tr -d '[:space:]' < "$PROJECT_DIR/.nvmrc")

case "$EXPECTED_NODE" in
  ''|*[!0-9.]*) echo "invalid pinned Node version in .nvmrc" >&2; exit 2 ;;
esac
ACTUAL_NODE=$(node -p 'process.versions.node' 2>/dev/null || true)
if [ "$ACTUAL_NODE" != "$EXPECTED_NODE" ]; then
  echo "Mix requires Node $EXPECTED_NODE; current Node is ${ACTUAL_NODE:-unavailable}" >&2
  echo "activate the version pinned in .nvmrc before building" >&2
  exit 3
fi

printf 'Node %s verified\n' "$ACTUAL_NODE"
