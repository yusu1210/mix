# Changelog

All notable user-facing changes to Mix are documented here. Mix follows
[Semantic Versioning](https://semver.org/) during the 0.x development series.

## [Unreleased]

### Added

- A project-owned shell entry point, `scripts/switch-codex.sh`, uses the same
  built-in account switch as the CLI and App; no external script is required.

### Changed

- Removed the experimental external-script `--backend cc-switch` / `--script`
  CLI options. Use `mix use <ACCOUNT>` or the project-owned shell entry point.

### Fixed

- Codex native token rotations are captured even when `last_refresh` is
  unchanged. Older refresh timestamps are ignored; token issue time resolves
  equal refresh timestamps when available.
- A rejected OAuth refresh can recover using another identity-checked local
  credential, including optional read-only CC Switch caches, before requiring
  sign-in. Duplicate/rejected tokens are not retried, and network/rate-limit
  errors never trigger credential fallback. Recovered credentials stay paired
  with the saved Provider route.
- Codex switches select the newest matching login before sending a refresh
  request, verify a different OAuth account even when its access token has not
  expired, and propagate successful rotations to same-account Mix profiles.
- Copied diagnostics include safe OAuth status/error codes. Blocking switch
  operations run outside the CLI and HTTP async executors.
- Direct Tauri builds rebuild the shared UI before bundling, preventing stale
  frontend assets and mismatched displayed versions.
- Updated vulnerable TLS/certificate dependencies and frontend development
  dependencies to patched versions.
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
