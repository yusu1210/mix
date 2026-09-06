# Contributing to Mix

Thank you for improving Mix. By participating, you agree to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

Mix protects credentials and native conversation history, so changes to
switching, storage, process control, session metadata, or packaging require
evidence proportional to their risk.

## Before starting

- Use the issue forms for reproducible defects and focused feature proposals.
- Use the private route in [SECURITY.md](SECURITY.md) for vulnerabilities.
- Keep a pull request focused on one outcome and explain any credential,
  process, history, or recovery impact.

## Development checks

1. Create a branch and keep unrelated changes out of the patch.
2. Activate the exact Node version in `.nvmrc`; build and quality scripts reject other versions.
3. Use disposable client directories; never use a contributor's real `~/.codex`, `~/.claude`, or `~/.mix` for automated tests.
4. Run `sh scripts/quality-gate.sh` before requesting review.
5. Add regression coverage for every corrected failure mode.
6. Update English and Chinese copy together when user-visible behavior changes.
7. Update user documentation and [CHANGELOG.md](CHANGELOG.md) when behavior,
   compatibility, installation, privacy, or recovery changes.

Cargo and local release scripts keep disposable Rust build output under
`/tmp/mix-target` (the quality gate uses `/tmp/mix-quality-target`) so the
project root stays small. Set `CARGO_TARGET_DIR` or
`MIX_QUALITY_TARGET_DIR` to an absolute path when an isolated cache is needed.
The gate also omits debug sections and incremental objects because they are
not needed for validation. These locations are disposable build output;
neither is part of the Mix application or its user data.

Do not add telemetry, upload transcripts, store raw secrets in configuration,
weaken loopback authentication, or claim cross-provider resume guarantees
without a reviewed product and security decision. New client support should
implement the adapter boundary and state its recovery guarantees explicitly.

Release versions must match in all manifests;
`cargo run --locked --package mix-release -- meta check` is authoritative.
Releases are produced only by the tagged workflow described in
[docs/RELEASING.md](docs/RELEASING.md).
