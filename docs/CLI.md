# Mix CLI

`mix` is the scriptable surface for the same Rust Core used by the Mac App
and Local Web. It reads and writes the same private `~/.mix` state and never
exports or imports native conversation history.

## macOS installation

Download the architecture-matched, signed and notarized
`mix_<version>_cli_<architecture>.pkg` from the official release and open it
with macOS Installer. It installs the protected CLI App at
`/Library/Application Support/Mix/mix-cli.app` and creates
`/usr/local/bin/mix`. Installation fails instead of replacing an unrelated
command already present at that path. The package never changes shell startup
files.

The app-like bundle keeps the CLI and Local Web UI on the same version. Source
builds may run `cargo run --package mix-cli -- <command>` and use
`mix web --ui apps/macos/dist` after building the React UI.

To remove only the installed CLI while keeping Mix settings, saved vault
items, and native Codex/Claude history:

```bash
sudo "/Library/Application Support/Mix/uninstall-cli"
```

The packaged uninstaller validates the command link, application directory,
and bundle identifier as root before deleting anything. It fails closed if
any target is no longer owned by Mix, removes only the CLI package, and leaves
all user data and native history in place.
Removing the Mac App or CLI never implies deleting `~/.mix`, `~/.codex`, or
`~/.claude`.

For the separate Mac App removal and Mix-data retention/erasure boundaries,
follow [UNINSTALLING.md](UNINSTALLING.md). In particular, inspect isolated
runtimes before erasing `~/.mix`; they can contain native sessions created by
the owning client.

## Setup

```bash
mix discover
mix connect codex
mix add
mix status

# Optional: add Claude Code as an isolated environment.
mix connect claude
mix add claude
```

`init` is optional: every command initializes missing local Mix state and runs
safe pending cleanup before doing its requested work. It remains available as
an explicit installation check.

`connect` registers an installed client using its detected native directory.
`add` defaults to Codex, captures the currently signed-in account, and derives
the visible identity when the client exposes one. `add claude` creates an
isolated blank Claude environment with an automatically generated name; sign-in
remains inside Claude's own official flow. `--label` is an optional, editable
alias rather than a required setup field.

To add another Codex account without affecting the current login, use the Mac
App's isolated sign-in flow. Claude Code environments can also be created from
the CLI with `mix add claude --label Work`; its native sign-in remains owned by
the client.

## Daily commands

```bash
mix status
mix use Personal
mix use claude Work
mix sync codex
mix run codex Personal --workspace /absolute/path/to/project
mix run claude Work --workspace /absolute/path/to/project
mix sessions all --query payment
mix resume codex <resume-id>
mix recover
mix diagnostics
mix web
```

`mix sync codex` refreshes both the matching account's opaque login in the
private local credential directory and its account/Provider overlay. Shared live settings such as MCP
servers, plugins, projects, features, notifications, and personality are not
copied into the account and therefore cannot be rolled back by a later switch.

`web` opens the default browser, prints a fallback loopback URL, and runs until
`Ctrl-C`. Use `--no-open` for a headless terminal. `--json` also stays headless
and emits one compact readiness object before waiting. The per-launch token is
carried only in the URL fragment; the page removes that fragment immediately
and retains the token only in the current tab. Official macOS CLI installers
contain an app-like bundle: the executable automatically loads the exact React build from
`mix-cli.app/Contents/Resources/ui`. A missing UI is an installation error and
the server will not start. `--ui <dist>` is an explicit development override,
not a normal setup step.

`use` and `run` accept either the visible account/environment label or its
internal id. An ambiguous duplicate label is rejected instead of guessed.
For Codex, `use` performs the guarded global account switch: preflight, process
handling, backup, durable journal, verification, and rollback. For an
environment-only client such as Claude Code, `use` only selects Mix's default
isolated environment; it does not modify the client's native home or login.
`run` creates a persistent project runtime and launches the selected client
with `CODEX_HOME` or `CLAUDE_CONFIG_DIR`; different projects can run
concurrently. Native session directories remain owned by Codex or Claude Code.

When the requested Codex account is already active, `use` does not create a
fake switch or rewrite configuration. It verifies the managed account and
opens or activates the native client without restarting it. Human output says
`Current account activated`; JSON retains the stable `already_active` status.

`sessions` prints each continuation locator as a resume id. `resume`
re-enumerates the native catalog and launches the client's official resume
command only when the session still passes A-level safety checks.

Use `--json` before the command for machine-readable output. Successful JSON
is written to stdout. Operational errors are written to stderr with a stable
`MIX_*` code and exit status `2`.
The Local Web API additionally returns stable nested rollback fields such as
`cause_code` and `cleanup_pending`; clients must localize those codes and must
not treat backend message text as a user-interface contract.

```bash
mix --json status > status.json
mix --json use codex Work
```

## Recovery

If Mix reports an interrupted switch, keep the client closed and run:

```bash
mix status
mix recover
```

New global switches and account metadata changes stay blocked until recovery
finishes. The journal contains paths and credential references, never tokens
or transcript contents.

## Boundaries

Mix never accepts a credential as a command-line argument or prints credential
values. The Codex adapter reads and, only during a verified official/custom
Provider switch, minimally updates Codex's own `threads.model_provider` index
field and verifiable fixed-width `session_meta` headers. Missing, pruned, or
malformed native rollouts are not a switch error and remain unrecoverable
during session discovery. It never copies or rewrites conversation bodies, and
the operation is journaled and reversible.
When adding a Codex account, Mix also rejects recognized literal credential
entries in `config.toml` rather than copying them into profile storage.
Ordinary non-secret MCP environment, header, and version settings remain
available. Configure secrets through Codex's environment-backed `env_key`,
`env_http_headers`, `bearer_token_env_var`, or `env_vars` settings first.
Codex history stays in its original `sessions`/`archived_sessions` directories;
Claude history stays in its native projects directory. `sessions` is a bounded,
read-only catalog.

The Mac App is the recommended daily surface for account identity, one-click
switching, project bindings, isolated sign-in, and recovery. Local Web provides
the same authenticated local API. CLI is intended for repeatable setup,
terminal-first launches, automation, and diagnostics.
