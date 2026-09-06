# Changelog

All notable user-facing changes to Mix are documented here. Mix follows
[Semantic Versioning](https://semver.org/) during the 0.x development series.

## [Unreleased]

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
