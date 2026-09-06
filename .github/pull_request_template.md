## Summary

Describe the user-visible or maintenance outcome.

## Verification

- [ ] `sh scripts/quality-gate.sh`
- [ ] Regression coverage was added or the reason it is unnecessary is stated.
- [ ] English and Chinese UI copy remain aligned when behavior changed.
- [ ] Tests use disposable client and Mix directories, not real user state.
- [ ] No credentials, account identifiers, private URLs, paths, prompts, or transcripts are included.
- [ ] Documentation and `CHANGELOG.md` were updated when users or operators are affected.

## Risk and recovery

Describe any impact on credentials, process control, native session metadata,
packaging, updates, or rollback. Write `None` when the change does not touch
these surfaces.
