# Mix

[简体中文](README.zh-CN.md)

Mix is a local-first account and workspace manager for AI coding clients. Its
primary promise is one-click Codex account switching without exporting,
importing, or losing native conversation history.

Mix provides one macOS app, one CLI, and an authenticated Local Web interface.
All three use the same Rust Core and the same local state.

## What Mix supports

| Capability | Codex | Claude Code |
| --- | --- | --- |
| Detect and add the current identity | Yes | Environment only |
| Switch the native global account | Yes | No |
| Official and custom Providers | Yes | Provider-specific environments |
| Isolated project environments | Yes | Yes |
| Discover native sessions | Yes | Yes |
| Resume through the native CLI | Yes | Yes |

Claude authentication may be stored outside `CLAUDE_CONFIG_DIR`, so Mix does
not present Claude environments as fully isolated OAuth accounts. The interface
states this boundary instead of promising unsupported switching.

## Install on macOS

Mix targets macOS 13 or later on Apple Silicon and Intel Macs. There is no
public binary release yet; use the source-build instructions below. Passing
CI or a local ad-hoc signature is not evidence of completed release acceptance.

When a release is available on the [Releases page](https://github.com/yusu1210/mix/releases),
download the architecture-matched DMG, open it, and drag `mix.app` to
Applications. Public releases must be Developer ID signed, notarized, and
accompanied by `SHA256SUMS` and SBOM files.

The optional CLI installer is named
`mix_<version>_cli_<architecture>.pkg`. It installs the signed CLI bundle under
`/Library/Application Support/Mix` and exposes `/usr/local/bin/mix` without
overwriting an unrelated command.

To build a local development app instead:

```bash
nvm use
cd apps/macos
npm ci
npm run bootstrap:rust
npm run build:desktop
```

The build requires the exact Node version in `.nvmrc` and the pinned Rust
toolchain. Local builds are ad-hoc signed development artifacts; they are not
equivalent to notarized release downloads.

From the repository root, open the completed local build with:

```bash
open .build/local-app/mix.app
```

No separate runtime installation is needed to open that bundle. The DMG is
in `.build/local-artifacts/`. See [release requirements](docs/RELEASING.md)
for the remaining signing and real-client acceptance gates.

## First use

1. Open Mix and connect Codex.
2. Choose **Add current account**. Mix reads the identity already active in
   Codex; a manual name is optional.
3. Add another account with Mix's isolated official sign-in flow, or add the
   account after signing in through Codex.
4. Select an account to switch. If the Codex desktop app is running, Mix closes
   and reopens that app once so the new credential and Provider route take
   effect together.

Wait for running desktop tasks to finish before switching: restarting Codex
interrupts those tasks. Preserving session history does not keep an in-flight
request running. If a switch fails, copy the diagnostics from Settings; do
not delete native history or repeatedly retry while work is running.

The switch is validated, journaled, verified, and rolled back on failure.
Existing session bodies, IDs, and locations stay in their native Codex
directories. A Provider transition may update only the minimal Provider
metadata required by Codex, inside the same recoverable transaction.

## CLI and Local Web

```bash
mix discover
mix connect codex
mix add
mix status
mix use Personal
mix sessions all --query payment
mix resume codex <resume-id>
mix web
```

`mix add` derives an identity automatically; `--label` adds an optional alias.
`mix use` accepts an unambiguous label or immutable account ID. `mix web` opens
the shared React UI on a random loopback port with a per-launch token.

For isolated project work:

```bash
mix run codex Personal --workspace /absolute/path/to/project
mix connect claude
mix add claude --label Work
mix run claude Work --workspace /absolute/path/to/project
```

See [the CLI reference](docs/CLI.md) for the complete command contract.

## Local-first safety model

- Mix has no analytics, advertising SDK, cloud sync, or automatic crash upload.
- Credentials are opaque private files under `~/.mix/credentials`; they are not
  stored in Mix JSON, logs, argv, diagnostics, or the web interface.
- Native Codex and Claude sessions remain the source of truth. Mix has no
  transcript database and never exports or imports conversation bodies.
- The local HTTP service binds to loopback and requires a random token for every
  launch.
- Account switching uses atomic writes, a durable journal, backups, identity
  verification, and rollback.
- Mix stops only the adapter-owned desktop process needed for a global switch;
  independent terminal agents are not treated as restartable processes.

Read [Privacy](PRIVACY.md), [Security](SECURITY.md), and
[Architecture](docs/ARCHITECTURE.md) for the exact boundaries.

## Development

The repository contains only two implementation languages:

- Rust: Core, adapters, CLI, local HTTP service, transactions, process control,
  credential storage, release tooling, and the Tauri shell.
- TypeScript/React: the Mac WebView and Local Web interface.

Run the complete local gate before opening a pull request:

```bash
nvm use
sh scripts/quality-gate.sh
```

Additional documentation:

- [Contributing](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)
- [Code of Conduct](CODE_OF_CONDUCT.md)
- [Platform support](docs/PLATFORM-SUPPORT.md)
- [Release process](docs/RELEASING.md)
- [Uninstall and data retention](docs/UNINSTALLING.md)
- [Support](SUPPORT.md)

## License

Mix is available under the [MIT License](LICENSE).
