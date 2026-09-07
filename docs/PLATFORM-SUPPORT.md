# Platform support

Mix targets macOS builds. No public binary release is available yet. The
table describes build targets, not completed clean-machine or client
acceptance. Source code may compile elsewhere, but an untested build is not
a supported distribution.

| Platform | Mac App | CLI / Local Web | Release status |
| --- | --- | --- | --- |
| macOS 13+ Apple Silicon | Local build | Local build | Signing, notarization, and clean-machine acceptance required |
| macOS 13+ Intel | Local build | Local build | Signing, notarization, and clean-machine acceptance required |
| Linux | Not provided | Source builds only | No installer or compatibility guarantee |
| Windows | Not provided | Not supported | No credential, process, or installer acceptance |

Both macOS architectures use the same Rust Core and React UI. Release artifacts
are built on native runners and must pass architecture, signature,
notarization, installation, uninstall, and client-compatibility acceptance.

Client capabilities are intentionally narrower than platform support:

- Codex supports managed global account switching, custom Provider routing,
  isolated project runtimes, native session discovery, and native resume.
- Claude Code supports isolated environments, native session discovery, and
  native resume. Mix does not claim full Claude OAuth account switching because
  official authentication can live outside `CLAUDE_CONFIG_DIR`.

Native session files remain owned by each client on every platform. Mix does
not export, import, or move conversation bodies.
