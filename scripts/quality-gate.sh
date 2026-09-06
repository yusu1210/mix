#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
. "$PROJECT_DIR/scripts/toolchain-env.sh"
RUSTUP_HOME="$MIX_RUSTUP_HOME"
CARGO_HOME="$MIX_CARGO_HOME"
CARGO="$CARGO_HOME/bin/cargo"
QUALITY_TARGET_DIR=${MIX_QUALITY_TARGET_DIR:-/tmp/mix-quality-target}

case "$QUALITY_TARGET_DIR" in
  /*) ;;
  *) echo "MIX_QUALITY_TARGET_DIR must be an absolute path" >&2; exit 3 ;;
esac
mkdir -p "$QUALITY_TARGET_DIR"
touch "$QUALITY_TARGET_DIR/.mix-target-owned"
export CARGO_TARGET_DIR="$QUALITY_TARGET_DIR"
# The gate exercises behavior, not debugger artifacts. Avoid retaining large
# incremental objects and debug sections in the disposable quality cache.
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_DEV_STRIP=none
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_PROFILE_TEST_STRIP=none

cd "$PROJECT_DIR"
if [ ! -x "$CARGO" ]; then
  echo "Mix Rust toolchain is unavailable; run apps/macos/scripts/bootstrap-rust.sh first" >&2
  exit 3
fi
sh scripts/verify-node.sh

RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_TOOLCHAIN=1.98.0 \
  "$CARGO" fmt --all -- --check
RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_TOOLCHAIN=1.98.0 \
  "$CARGO" test --workspace --all-targets --locked
RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_TOOLCHAIN=1.98.0 \
  "$CARGO" clippy --workspace --all-targets --all-features --locked -- -D warnings

cd "$PROJECT_DIR/apps/macos"
npm test
npm run typecheck
npm run build

cd "$PROJECT_DIR"
RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_TOOLCHAIN=1.98.0 \
  "$CARGO" run --quiet --locked --package mix-release -- sbom source-check
RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_TOOLCHAIN=1.98.0 \
  "$CARGO" run --quiet --locked --package mix-release -- meta check

for script in \
  apps/macos/scripts/bootstrap-rust.sh \
  apps/macos/scripts/bootstrap-cargo-about.sh \
  apps/macos/scripts/build-desktop.sh \
  apps/macos/scripts/build-cli-bundle.sh \
  apps/macos/scripts/build-cli.sh \
  apps/macos/scripts/build-cli-installer.sh \
  apps/macos/scripts/cli-installer/preinstall \
  apps/macos/scripts/cli-installer/uninstall \
  apps/macos/scripts/configure-rust-release.sh \
  apps/macos/scripts/build-dmg.sh \
  apps/macos/scripts/build-macos-updater.sh \
  apps/macos/scripts/generate-third-party-licenses.sh \
  apps/macos/scripts/sign-macos.sh \
  apps/macos/scripts/strip-macos.sh \
  scripts/quality-gate.sh \
  scripts/clean.sh \
  scripts/toolchain-env.sh \
  scripts/verify-node.sh \
  scripts/verify-macos-architecture.sh \
  scripts/verify-macos-cli-installer.sh \
  scripts/verify-macos-artifact.sh \
  scripts/verify-macos-update-artifact.sh; do
  sh -n "$script"
done

printf '%s\n' "Mix quality gate passed"
