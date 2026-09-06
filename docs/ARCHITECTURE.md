# Mix Architecture

## Decision

Mix has one domain implementation and one visual implementation:

- Rust owns the domain model, Codex/Claude adapters, configuration, atomic
  filesystem operations, transaction journal, rollback, process control,
  private local credential store, CLI, and loopback HTTP service.
- TypeScript/React owns the Mac WebView and Local Web UI.
- Tauri is a thin Rust shell for macOS windowing, menu bar actions, native
  folder selection, clipboard writing, and signed-release version checks.

A CLI command, Local Web request, and Mac App action enter the same Rust service
methods; there is no parallel switching implementation.

The updater integration is deliberately check-only. Its Tauri ACL grants no
download, install, or process-restart command. A detected version opens the
official release page for a notarized DMG installation; Mix does not replace
its running App bundle in the background.

## Account continuity

The client owns its native state. Mix stores profile metadata and opaque
credential references in `~/.mix`; credential values are held by Mix's private
per-user file store. A managed Codex switch projects the selected credential into the
client's required file-auth layout, creates a durable backup and journal, then
verifies the resulting account identity. A failure restores the old projection.
Codex configuration is not an account-sized snapshot. Mix stores only the
account/Provider overlay and composes it over the latest live user configuration
at switch or isolated launch time. Provider selection, endpoints, compatible
model settings, and the selected Provider definition follow the account; MCP
servers, plugins, projects, features, notifications, personality, and other
user preferences remain shared. Legacy full snapshots are read through the same
overlay extractor, so they cannot restore stale shared settings. The resulting
configuration is staged inside the switch journal before the live file changes.
This follows Codex's documented base-plus-overlay configuration model while
avoiding a hidden dependency on a CLI-only `--profile` selector.
Account capture and switch validation reject recognized literal
credential-bearing Codex configuration entries before they can enter Mix
profile storage. Ordinary non-secret values remain configuration; secret
settings should reference environment-variable names whose values stay outside
the profile or in Mix's private local credential directory.
Claude `settings.json` is revalidated immediately before every profile launch
or runtime resume, not only when it is first imported. A later file change
cannot bypass the adapter's path and sensitive-setting policy.

Codex `sessions`, `archived_sessions`, rollout bodies, and private indexes are
never moved, imported, or exported. Claude native project history follows the
same ownership rule. For a Codex official/custom Provider transition, the Codex
adapter creates a consistent staged copy of each native index and may update
only `threads.model_provider` plus the first
`session_meta.model_provider` value in referenced rollouts, with a durable
journal and rollback. This is a client-compatibility projection, not a
Mix-owned history store; conversation bodies remain byte-stable. Routine
catalog discovery remains bounded and read-only, and resume invokes the
client's documented command. A missing or pruned rollout referenced by the
native index is skipped during the Provider projection; it remains visible as
an unrecoverable native session and must not prevent an account switch. A
missing, corrupt, or unsupported `state_5.sqlite` is likewise left untouched;
only a recognized index with `threads.model_provider` is projected. A Codex
desktop account switch launches the app and waits for the declared process
identity before verifying the stable target projection. If both system-wide and
per-user ChatGPT apps are installed,
Mix stops both copies that share the native Codex home, restarts the copy that was
running, and addresses that exact app path for launch and activation. The
first post-switch request therefore cannot inherit an old thread's Provider
route. Provider route and credential are one
profile projection: Mix never writes either half while the managed desktop
process is still running. Existing thread metadata is rebound to the selected
Provider only when Codex requires the official/custom bucket transition;
continuing one from Mix also supplies the selected Provider explicitly to the
official resume operation.

## Project runtimes

Global switching is for changing the active login used by a native client.
Project launch is the concurrency path: Mix materializes a profile into a
persistent project runtime and launches Codex with `CODEX_HOME` or Claude
Code with `CLAUDE_CONFIG_DIR`. Two projects can therefore run simultaneously
with different environments. A private-store-backed environment secret is read only
at launch and never placed in argv, logs, Mix JSON, or the UI.
Core rejects credential-like variable names from the ordinary environment map;
they must be represented by an opaque local credential reference in the secret map.

Every profile has an immutable UUID that is separate from its internal name
and editable label. Runtime paths and markers carry that UUID. Deleting an
environment and later creating another with the same name therefore cannot
make old Claude sessions look as if they belong to the replacement, and a
delayed cleanup intent cannot revoke files from the replacement. Older runtime
markers without a provable UUID fail closed until a new launch establishes the
current identity.

Profile labels also record their origin. A generated label follows the latest
verified native identity or Provider metadata; a user label always wins and is
never replaced by a later identity refresh. This is explicit persisted state,
not a string-pattern heuristic, so the rule is identical for OAuth accounts,
API-key accounts, custom Providers, and future adapters.

Projects are keyed by canonical paths and can be discovered from recorded
session working directories and folders explicitly selected by the user. A project binding selects the
profile for a client; it does not rewrite existing sessions and does not
require Mix to become an agent execution service.

## Local service

The Rust HTTP service binds only to loopback and requires a random per-launch
token. The Tauri shell starts it in-process and exposes only the base URL and
token to its WebView. Local Web receives the same token in a URL fragment that
is never sent in an HTTP request. No
cloud service, telemetry, credential sync, or transcript upload is required.
The browser removes the fragment immediately, keeps the token only for the current tab,
and receives a restrictive CSP, no-referrer policy, MIME sniffing protection,
and frame denial from the local server.

## Failure model

Every state-changing operation takes one Mix resource lock before the config
lock, then uses atomic writes for durable configuration. Every config mutation
receives a unique revision; if a post-rename durability call reports an
ambiguous failure, Core reloads that revision before deciding whether the
mutation committed. A global switch stops
every running desktop process whose executable identity is explicitly declared
by that client's adapter, synchronizes the final live Codex credential, and only
then creates a backup and durable prepared journal. Independent terminal agents
are not enumerated or terminated because Mix cannot safely reconstruct their
invocations; this concurrency boundary remains part of real-account release
acceptance. The transaction projects the target state, verifies the
identity, persists the new active account, durably marks the transaction
committed, and finally removes temporary backup material. A restored or
committed journal can only finish cleanup; it can never apply rollback twice.
Backup file paths and transaction-vault references are derived from the
transaction identity instead of trusted from serialized journal fields.
On interruption, other mutations are blocked until recovery succeeds.
Symlinks, unsafe paths, invalid references, unsupported credential stores, and
unknown client state fail closed.
The switch command validates the target profile and credential before stopping
the managed desktop process or writing live files. The selected account action
is the user's explicit intent; backup, projection verification, rollback, and
restart are one Core transaction rather than a second UI protocol.
When a rollback succeeds, the public error carries the stable nested
`cause_code`, `cleanup_pending`, and optional `cleanup_code`; the visual client
localizes those codes instead of displaying raw backend paths or credential
material. It never tells the user to inspect an activity record that was not
created.

Removing a profile commits its metadata removal and runtime revocation intent
in one config mutation, then scrubs Mix-materialized runtime credentials and
writes a durable revocation marker. Profile directories, unshared credential
references, and runtime revocations share an idempotent pending-cleanup queue;
failures survive restart and are retried without touching native history or a
new profile that later reuses the same name. Concurrent launches hold per-runtime
credential leases, so one exiting process cannot remove credentials still used
by another; the final wrapper scrubs them. The next Mix startup prunes dead
leases and retries revocations after crashes. Runtime and native session history
remain in place.

A runtime session carries a validated Mix marker and resumes with the same
profile credential re-projected from the vault. A live native session normally
has no provable account owner. When its working directory matches a registered
project binding, Core requires that bound profile to be active before resume;
otherwise it fails closed. An unbound live session resumes with the current
account and the UI states that attribution explicitly. Native Claude resume
also receives the project-bound environment, or the active environment when no
project binding exists, so API keys and gateway variables are not silently
dropped at the handoff boundary.

Codex account resolution uses both the opaque credential fingerprint and the
normalized Provider route. This lets one API key back more than one named
gateway without collapsing those profiles together. Only one exact match may be
synchronized. An edited or otherwise unknown route is treated as an unmanaged
account until it is explicitly added; Mix never guesses from the last active
profile, a credential-only match, or map order.

## Local credential storage

Mac App, CLI, and Local Web use one cross-platform store under
`~/.mix/credentials`. References are mapped to non-reversible SHA-256 filenames;
the directory is mode `700` and each credential is atomically replaced with
mode `600` on Unix. Every read, write, existence check, and deletion rejects a
replaced credential directory, links, non-regular files, oversized values, and
group/world-accessible permissions. Values never enter `config.json`, logs,
diagnostics, UI responses, or native session data.

This deliberately matches the security boundary of the native clients'
file-based login while avoiding Keychain prompts, code-signing identity coupling,
and different App/CLI behavior. Developer ID signing and notarization remain
distribution-integrity requirements, not credential-storage dependencies.

## Extension boundary

New clients implement the Rust adapter capability contract: discovery,
configuration validation, native session catalog, native resume arguments,
runtime directory semantics, and any native desktop process/reopen contract.
Each built-in adapter owns its descriptor—stable kind, product label, default
home, executable command, native session roots and profile naming—so discovery,
registration, naming, and CLI account-versus-environment behavior contain no
per-client branches. The descriptor's profile category must agree with whether
the adapter implements global account projection, which is enforced by tests.
Clients with global account state additionally implement one optional projection
capability covering source detection, credential validation, complete file
materialization, pre/post-restart verification, and credential synchronization.
Adapters that can identify the current native login implement the separate
optional account-capture capability. The adapter owns identity extraction,
route-aware matching and credential refresh; Core owns generated profile ids,
private files, opaque credential references and rollback cleanup. Account
capture therefore contains no client-kind branch and does not assume an
`auth.json`, `config.toml`, Provider shape or credential format.
Native isolated sign-in is a third optional adapter capability. The adapter
supplies bounded bootstrap files, an argument-vector launch plan and the rule
for recognizing a completed login. Core owns the durable enrollment record,
timeout, terminal handoff, duplicate detection, profile persistence and
cleanup. Active-login repair reuses the same captured-account contract instead
of a client-specific service path.
The Core switch transaction contains no provider name, account identity, or
endpoint condition; adding another global-account client does not change its
state machine.
Process commands are product capabilities derived by the adapter, never stale
user configuration. An adapter may expose only the capabilities the client can
actually guarantee. This prevents a generic account UI from claiming OAuth
isolation or history continuity that the client does not provide.
