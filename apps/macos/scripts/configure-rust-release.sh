#!/bin/sh

if [ -z "${PROJECT_DIR:-}" ] || [ -z "${CARGO_DIR:-}" ]; then
  echo "PROJECT_DIR and CARGO_DIR are required for Rust release path remapping" >&2
  exit 2
fi
if [ -n "${RUSTFLAGS:-}" ] || [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
  echo "external Rust flags are not allowed in a reproducible Mix release build" >&2
  exit 2
fi

RUST_FLAG_SEPARATOR=$(printf '\037')
CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$CARGO_DIR=cargo${RUST_FLAG_SEPARATOR}--remap-path-prefix=$PROJECT_DIR=workspace"
export CARGO_ENCODED_RUSTFLAGS
# Cargo's implicit release stripping delegates to rust-objcopy and may degrade to
# a warning when LLVM runtime files are unavailable. Final Mach-O artifacts are
# stripped explicitly by strip-macos.sh before signing.
export CARGO_PROFILE_RELEASE_STRIP=none
