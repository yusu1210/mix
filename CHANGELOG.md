# Changelog

All notable user-facing changes to Mix are documented here. Mix follows
[Semantic Versioning](https://semver.org/) during the 0.x development series.

## [Unreleased]

### Added

- The CLI can explicitly delegate a Codex switch to an existing local CC Switch
  wrapper with `mix use --backend cc-switch <ALIAS>`, then reconcile the active
  account when it is already managed by Mix.

### Fixed

- Codex installations that moved the CLI to `ChatGPT.app/Contents/Resources/codex-cli/bin/codex` are discovered automatically, including existing Mix configurations that still point to the old path.
- While the Mac App or Local Web is running, newer native Codex OAuth
  credentials are saved to matching accounts without changing their routes.
  Switching also captures the current login before refreshing the target.
- A switch interrupted by expired login now continues once after successful,
  identity-checked sign-in; failed sign-in never loops or switches accounts.
- Desktop launch no longer requires a background CLI worker. A launch failure
  keeps the verified target account and asks the user to open the client,
  rather than restoring old credentials and restarting the client again.
- Login refresh failures retain allowlisted OAuth status/code diagnostics,
  without storing upstream responses or credentials.
- Codex history databases and sidecars larger than 16 MiB no longer fail during
  transaction backup, application, or rollback. These files are copied with
  bounded memory and atomic replacement.
- Refreshing an account's login no longer copies another account's provider
  route into its saved configuration. OAuth refresh uses form-encoded requests.

## [0.1.3] - 2026-09-07

### Fixed

- Codex account switching no longer fails when a WAL-mode history database
  has no `-wal` or `-shm` file after the client exits. Existing WAL content is
  preserved while staging and verifying both supported history locations.
- Failed account switches now record their stage, error code, and app version
  locally and include those fields in diagnostics without credentials or paths.

## [0.1.2] - 2026-09-06

Initial public release.

### Added

- macOS App, menu-bar controls, CLI, and authenticated Local Web interface.
- Transactional Codex account switching for OAuth, API-key, and custom Provider
  profiles, with identity verification, restart handling, recovery, and
  rollback.
- Native Codex and Claude session discovery and native CLI resume without
  export/import.
- Isolated Codex and Claude project environments.
- Chinese and English interface, diagnostics, update checks, signed macOS
  packaging, checksums, SBOMs, and release provenance.

### Security

- Private per-user credential files, atomic configuration writes, durable
  switch journals, authenticated loopback APIs, restrictive CSP, and redacted
  diagnostics.
