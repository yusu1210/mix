#!/bin/sh

case "${MIX_TOOL_ROOT:-}" in
  /*) ;;
  "")
    case "$(uname -s)" in
      Darwin) MIX_TOOL_ROOT="${HOME}/Library/Caches/mix/toolchain" ;;
      *) MIX_TOOL_ROOT="${XDG_CACHE_HOME:-${HOME}/.cache}/mix/toolchain" ;;
    esac
    ;;
  *)
    echo "MIX_TOOL_ROOT must be an absolute path" >&2
    return 3 2>/dev/null || exit 3
    ;;
esac

MIX_RUSTUP_HOME="$MIX_TOOL_ROOT/rustup"
MIX_CARGO_HOME="$MIX_TOOL_ROOT/cargo"

export MIX_TOOL_ROOT MIX_RUSTUP_HOME MIX_CARGO_HOME
