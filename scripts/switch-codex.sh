#!/bin/sh
# Script entry point for Mix's built-in switch transaction, not another backend.
# Usage: sh scripts/switch-codex.sh <account label or id> [--config PATH] [--json]
set -eu

if [ "$#" -eq 0 ]; then
  echo 'Usage: switch-codex.sh <account label or id> [--config PATH] [--json]' >&2
  exit 2
fi

exec "${MIX_BIN:-mix}" use codex "$@"
