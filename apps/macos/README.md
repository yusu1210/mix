# Mix Mac App

The Mac App is a Tauri shell around the Rust local control plane and the
shared React interface. The installed application is self-contained and does
not require development toolchains at runtime.

## Development

```bash
nvm use
npm ci
npm run bootstrap:rust
npm test
npm run typecheck
npm run build
npm run build:desktop
```

The scripts require the exact Node version in the repository `.nvmrc`.
`bootstrap:rust` uses a versioned official `rustup-init` with an embedded
architecture-specific SHA-256 and disables rustup self-updates.

Rust and Cargo caches live outside the checkout at
`~/Library/Caches/mix/toolchain` on macOS. Set `MIX_TOOL_ROOT` to an absolute
path when an isolated toolchain is required.

`build:desktop` creates a local ad-hoc-signed `mix.app` and DMG. It bundles
the Rust Tauri executable, the React assets, and the generated Rust dependency
license evidence. The installed app uses an in-process Rust-owned local HTTP
service on a random loopback port with a per-launch token.
Stable local copies are written to `../../.build/local-app/mix.app` and
`../../.build/local-artifacts/`; Cargo's disposable target directory can be
cleaned independently.
Local Cargo output defaults to `/tmp/mix-target`, keeping the project root free
of build caches. Packaging artifacts remain in `.build`. Set `CARGO_TARGET_DIR` to an absolute path
when an isolated candidate is required; every build and packaging stage uses
that directory.

## Release

Protected release builds set `MIX_UPDATER_ENABLED=1`, provide an HTTPS update
endpoint, a validated public key, and the offline Tauri signing key. The
release workflow signs and notarizes the app, recreates the updater archive
from the final app bytes, signs the DMG, and runs architecture and Gatekeeper
checks. Local builds never contact an update endpoint.

The shipped updater permission is check-only. A new-version prompt opens the
official release page; Mix does not download, install, or relaunch itself.

The public release gate still requires Developer ID credentials, Apple
notarization, clean Apple Silicon and Intel acceptance, real Codex/Claude
authentication and native-session tests, manual DMG upgrade tests, and human
accessibility review.

## Uninstall

Quitting and removing `mix.app` preserves Mix data and every native client
directory. Data erasure is deliberately separate because isolated runtimes can
contain native sessions. Follow the repository's bilingual
[uninstall guide](../../docs/UNINSTALLING.md) instead of recursively deleting
client or home directories.
