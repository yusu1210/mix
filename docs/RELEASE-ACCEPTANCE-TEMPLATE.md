# Mix release acceptance evidence / Mix 发布验收证据

Copy this file for one release candidate, replace every placeholder, and commit the completed file to the release repository. Supply its commit-addressed `https://raw.githubusercontent.com/<owner>/<repository>/<commit-sha>/<path>` URL to the protected **Publish accepted release** workflow. Branch, tag, `github.com/blob`, redirecting, query-string, and cross-repository URLs are rejected. A checkbox is not evidence by itself: every gate needs the tester, UTC time, exact device/client version, result, and durable sanitized evidence link.

为每个候选版本复制本模板并替换所有占位内容，将完成后的文件存放在受保护 **Publish accepted release** 工作流所填写的内容寻址 GitHub HTTPS 地址。勾选本身不是证据：每个闸门都必须记录测试人、UTC 时间、设备与客户端精确版本、结果及持久化脱敏证据链接。

Do not include tokens, cookies, OAuth codes, account identifiers, email addresses, local usernames, home paths, session prompts, transcript text, private provider URLs, or unredacted diagnostics. Use dedicated test accounts and disposable machines or operating-system users.

不得写入 token、Cookie、OAuth code、账号 ID、邮箱、本机用户名、Home 路径、会话问题与正文、私有 Provider 地址或未经脱敏的诊断信息。必须使用专用测试账号以及一次性设备或系统用户。

## Release identity / 发布身份

| Field / 字段 | Reviewed value / 审核值 |
| --- | --- |
| Mix version / 版本 | `X.Y.Z` |
| Git tag | `vX.Y.Z` |
| Git commit SHA | `<40/64-hex>` |
| Draft release URL | `https://…` |
| `SHA256SUMS` SHA-256 | `<64-lowercase-hex>` |
| Previous public version / 上一公开版本 | `X.Y.Z` or `none` |
| Evidence owner / 证据负责人 | `<name/team>` |
| Review window UTC / 验收时间段 | `<start> — <end>` |

The tested DMG, updater archive, SBOM, and license evidence must match the draft release and its `SHA256SUMS`; do not test a locally rebuilt substitute.

所有测试对象必须来自同一个 Draft Release，并与其 `SHA256SUMS` 一致；不得以本机重新构建的 DMG、更新包、SBOM 或许可证文件替代。

The two deterministic archive passes must be byte-identical for the same
signed bundle. Do not require separately signed/notarized DMGs to be
byte-identical; verify their recorded hash, signature, ticket, SBOM, and
provenance instead.

同一已签名 App Bundle 的两次确定性归档结果必须字节完全一致。不要要求
分别签名、公证的 DMG 字节完全一致；应验证其记录的哈希、代码签名、
公证票据、SBOM 和供应链来源。

## Test inventory / 测试清单

| ID | Architecture / 架构 | OS and build / 系统版本 | Clean machine/user / 净机或新用户 | Mix asset SHA-256 | Codex version | Claude version | Tester | UTC |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M1 | Apple Silicon | `<value>` | `yes` | `<digest>` | `<value>` | `<value>` | `<name>` | `<time>` |
| M2 | Intel | `<value>` | `yes` | `<digest>` | `<value>` | `<value>` | `<name>` | `<time>` |

## Gate 1 — Codex authentication compatibility / Codex 认证兼容

Workflow check: `codex_oauth_api_key_provider_matrix`

- [ ] Fresh official OAuth sign-in succeeds in a disposable user; Mix saves only after explicit confirmation.
- [ ] Expired/rotated OAuth state is refreshed by Codex, then `mix sync codex` updates only the matching saved account and never overwrites a newer vault generation. The normal A → B switch proves the same synchronization occurs automatically without requiring a separate UI action.
- [ ] Official API-key mode and one reviewed custom Provider configuration launch successfully without writing raw secrets to Mix config, logs, activity, diagnostics, process arguments, or UI responses.
- [ ] With a disposable `CODEX_HOME` and loopback capture server, the exact shipped Codex version proves that `requires_openai_auth` sends the isolated `auth.json` API Key only to the selected custom Provider path. The probe must not contact either a real Provider or OpenAI and must record no raw credential.
- [ ] `file` credential storage works; explicit `keyring`, `auto`, and `ephemeral` modes fail closed for managed capture/switch instead of using a stale `auth.json`.
- [ ] Upgrade from the preceding supported Codex version and forced client exit/restart retain correct authentication state.
- [ ] Failure cases show a useful bilingual recovery action and leave the previous account usable.
- [ ] The shipped App and packaged CLI use the same `~/.mix/credentials` store. A credential created by either surface is readable and updatable by the other without a password prompt before and after upgrade; directory/file modes are `700`/`600`.
- [ ] The Developer ID Installer-signed and notarized CLI package installs its provisioned `mix-cli.app` under `/Library/Application Support/Mix`, exposes `/usr/local/bin/mix`, and does not overwrite a foreign command at that path.
- [ ] The installed CLI contains `Contents/Resources/ui`; `mix web` opens the complete bilingual interface on a clean machine without `--ui`, and fails before starting the server when those signed resources are missing.

Evidence / 证据：`<tester, UTC, versions, sanitized logs/screenshots, result, issue links>`

## Gate 2 — Codex account continuity and rollback / Codex 账号连续性与回滚

Workflow check: `codex_account_switch_history_rollback`

- [ ] Prepare two dedicated Codex accounts and pre-existing native sessions in the same persistent workspace/runtime.
- [ ] Switch A → B from the main window and B → A from the menu bar. Each explicit target-account selection starts the same Core transaction without a second modal, stops only the exact client, creates a backup and non-secret activity event, and rejects rapid duplicate actions.
- [ ] When A and B use different Providers, the restarted Codex client uses the target route before its first request and never sends the target credential to the previously active endpoint; continuing an old thread from Mix explicitly uses the selected Provider.
- [ ] Record the exact Codex App and bundled CLI versions. Verify both lifecycle paths: switching a running Codex client, and opening Codex when the requested account is already active but the app is closed. Process appearance or a successful `open` exit code alone is insufficient evidence that the client is ready.
- [ ] No export/import is performed. Native indexes remain client-owned; only the documented Provider metadata fields may change, while rollout bodies and transcript content remain byte-stable and visible after both directions of switching.
- [ ] Add or change MCP servers, plugins, projects, features, notifications and personality after capturing A and B. Switching A → B → A preserves those latest shared settings while applying only each account's Provider/endpoint/model compatibility overlay; a legacy full profile snapshot cannot restore stale shared settings.
- [ ] A recovery-grade A session opens through one-click **Continue** using the exact native resume command and original working directory; B rows never claim one-click resume or export support.
- [ ] A blocked or rejected action has no side effect. Injected write/restart/verification failure restores projected credentials and config; an interrupted transaction blocks later mutations until explicit recovery succeeds. Native session state is never written by Mix.
- [ ] Preview completes without reading the target credential. Credential fingerprint verification occurs only after final confirmation, and no system password prompt appears.
- [ ] An unsaved current login cannot be overwritten, a mismatched account fingerprint is rejected, and a newer saved credential generation is preserved.
- [ ] The menu bar labels the already active account as **Open / 打开**, not as another switch. Selecting it activates a running client without restarting it, or opens it once when closed; it does not claim to create a new native session.

Evidence / 证据：`<before/after catalog counts, sanitized backup/activity proof, failure injection, tester, UTC, result>`

## Gate 3 — Claude boundary and native resume / Claude 边界与原生恢复

Workflow check: `claude_auth_and_native_resume`

- [ ] Official Claude authentication is tested on both architectures without claiming that `CLAUDE_CONFIG_DIR` switches macOS Keychain OAuth accounts.
- [ ] Two environment profiles preserve their native directories and project transcripts.
- [ ] A verified Claude transcript resumes through the official native `--resume` contract with the expected project directory.
- [ ] A native Claude resume inherits the most-specific project-bound environment, or the active environment when unbound; API-key and gateway variables are present in the launched process without appearing in argv or handoff text.
- [ ] Unsupported or ambiguous records remain copy/source-only and display the explicit reason.
- [ ] Provider/API-key environments use private local credential references; no raw secret appears in Mix configuration, diagnostics, logs, UI responses, or Terminal handoff files.
- [ ] English and Chinese UI consistently describe Claude as environment/history management, not guaranteed account isolation.

Evidence / 证据：`<tester, UTC, versions, sanitized screenshots/logs, result>`

## Gate 4 — Apple Silicon clean install/uninstall / Apple Silicon 净机安装卸载

Workflow check: `macos_aarch64_clean_install_uninstall`

- [ ] Verify DMG SHA-256, Developer ID signature, stapled ticket and Gatekeeper acceptance before opening.
- [ ] Drag to Applications, first launch succeeds without development toolchains on `PATH`, and no unexpected network connection occurs when updater is disabled.
- [ ] Onboarding, account switching, rollback, one-click native history resume, menu-bar lifecycle, diagnostics copy and Chinese/English switching complete successfully.
- [ ] Closing the window keeps the menu-bar app/core healthy; reopen restores state; Quit removes owned processes, token and readiness files.
- [ ] Removing the App is documented and predictable. Native Codex/Claude history is not deleted; Mix data removal is a separate explicit user action.
- [ ] Install and upgrade the architecture-matched CLI package, then run the packaged `uninstall-cli` helper. The helper rejects a substituted command link or bundle identifier before deletion; package receipts and `/usr/local/bin/mix` behave predictably, while `~/.mix` and native Codex/Claude history remain untouched.

Evidence / 证据：`<device, OS, commands, sanitized screenshots/logs, tester, UTC, result>`

## Gate 5 — Intel clean install/uninstall / Intel 净机安装卸载

Workflow check: `macos_x86_64_clean_install_uninstall`

Repeat every Gate 4 item on a native Intel Mac using the x86_64 draft asset. Rosetta-only execution on Apple Silicon does not satisfy this gate. Confirm every bundled Mach-O file is x86_64 and no arm64 library is mixed into the App.

在原生 Intel Mac 上使用 x86_64 Draft 产物重复 Gate 4 全部项目。Apple Silicon 上仅通过 Rosetta 运行不能关闭本闸门；还必须确认 App 内所有 Mach-O 均为 x86_64，不混入 arm64 库。

Evidence / 证据：`<device, OS, architecture output, sanitized logs/screenshots, tester, UTC, result>`

## Gate 6 — Signed update and recovery / 签名更新与恢复

Workflow check: `signed_update_and_failure_recovery`

- [ ] Install the preceding public version, detect this candidate from the production HTTPS endpoint, and verify displayed version/release notes.
- [ ] Confirm the shipped Tauri capability permits update checks only and rejects download, install, and process-restart commands.
- [ ] Test **Later → Review update → Open official release page**, then complete a manual notarized-DMG upgrade with Mix settings, private local credentials, workspace bindings and native client history intact.
- [ ] Offline checks and tampered update metadata fail safely without opening or changing the installed App.
- [ ] Verify both architecture entries in `latest.json`, updater signatures, archive/App signatures and final installed architecture.

Evidence / 证据：`<from/to versions, endpoint, sanitized failure logs, both architectures, tester, UTC, result>`

## Gate 7 — VoiceOver, keyboard, localization and layout / 无障碍、键盘、本地化与布局

Workflow check: `voiceover_keyboard_responsive_ui`

- [ ] A human tester completes onboarding, direct account switching, switch failure recovery, interrupted recovery, session search/filter/Continue, settings and quit using VoiceOver without pointer input.
- [ ] Reading/focus order, control role/name/value/state, disabled reason, dialog announcement, error recovery and focus return are understandable in both English and Chinese.
- [ ] Full keyboard flow includes `Tab`, reverse tab, arrows where appropriate, `Enter`, `Space`, `Esc` and `⌘K`; no keyboard trap exists.
- [ ] Light/dark/system themes, Reduce Motion, supported 720/900/1040+ widths and 200% zoom/text-size scenarios have no clipped critical action, horizontal page overflow or color-only status.
- [ ] Destructive actions and irreversible boundaries have explicit text; session recovery grades are not communicated by color alone.

Evidence / 证据：`<human tester, assistive setup, task completion notes, sanitized video/screenshots, UTC, result>`

## Gate 8 — Privacy, security, licensing and support / 隐私、安全、许可证与支持

Workflow check: `privacy_security_support_signoff`

- [ ] Security reviewer verifies loopback binding, per-launch token deletion, Origin/request limits, local credential-store fail-closed behavior, private permissions, symlink/path escape rejection and transaction recovery.
- [ ] Shared diagnostics contain no account/profile labels, filesystem paths, commands, environment values, credentials, session titles/prompts/transcripts or private Provider data.
- [ ] Privacy notice, support boundary, vulnerability-reporting route, MIT license, SBOM, complete target license evidence and artifact provenance are reviewed and reachable from the product/release.
- [ ] Support owner, severity model, response targets, rollback/manual recovery instructions and security-update channel are staffed for the announced audience.
- [ ] Optional telemetry/crash reporting is either absent or explicitly opt-in with documented fields, retention and deletion; behavior matches both languages.
- [ ] Open high/critical security, data-loss, account-misattribution, updater or accessibility defects are zero; accepted lower-severity risks are listed with owner and deadline.

Evidence / 证据：`<reviewers, reports, issue queries, policy/support URLs, UTC, result>`

## Final decision / 最终结论

| Gate | Result | Reviewer | UTC | Immutable evidence link |
| --- | --- | --- | --- | --- |
| Codex auth matrix | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| Codex continuity/rollback | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| Claude auth/resume | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| Apple Silicon install | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| Intel install | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| Signed update/recovery | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| VoiceOver/keyboard/layout | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |
| Privacy/security/operations | `PASS/FAIL` | `<name>` | `<time>` | `https://…` |

Final release decision / 最终发布决定：`ACCEPT / REJECT`

Release manager / 发布负责人：`<name>`

Decision UTC / 决策时间：`<time>`

Known accepted risks / 已接受风险：`<none or linked list>`

Only `ACCEPT` with all eight gates at `PASS` may be submitted to the protected publication workflow. Before publishing, complete the sealed block below, verify it against the exact Draft Release and `SHA256SUMS`, then compute the completed file digest without editing it afterward:

只有八项全部 `PASS` 且最终结论为 `ACCEPT` 时，才能运行受保护发布工作流。填写下方密封区块，使用精确 Draft Release 和 `SHA256SUMS` 验证后，不得再次编辑文件，并计算其精确摘要：

```bash
cargo run --locked --package mix-release -- readiness evidence-verify \
  --version X.Y.Z \
  --repository owner/repository \
  --tag vX.Y.Z \
  --draft-release-url https://github.com/owner/repository/releases/tag/vX.Y.Z \
  --sha256sums-sha256 <SHA256-of-downloaded-SHA256SUMS> \
  /absolute/path/to/mix-vX.Y.Z-acceptance.md
cargo run --locked --package mix-release -- readiness evidence-hash /absolute/path/to/mix-vX.Y.Z-acceptance.md
```

Enter the 64-character lowercase evidence-file digest as `evidence_sha256`. The publication workflow downloads that HTTPS file again, limits it to 2 MiB, and revalidates both digest and sealed decision. The attested `RELEASE-ACCEPTANCE.json` binds the evidence URL and digest, Draft Release URL, `SHA256SUMS` digest, approver, workflow run, eight decisions, and every released payload hash.

将该 64 位小写证据文件摘要填写到 `evidence_sha256`。发布工作流会重新下载 HTTPS 文件、限制为 2 MiB，并复核摘要与密封决策。最终带证明的 `RELEASE-ACCEPTANCE.json` 会绑定证据 URL 与摘要、Draft Release URL、`SHA256SUMS` 摘要、审批人、工作流运行记录、八项决定及全部发布文件哈希。

## Sealed decision / 密封决策

Replace every value below with the exact reviewed release data. Keep this as the final block in the file and do not add another marker. Every evidence URL must be HTTPS and point to durable sanitized evidence. Use an empty `known_risk_links` array only when no lower-severity risk was accepted.

将以下值替换为本次候选版本的精确审核数据。此区块必须位于文件末尾，且不得增加第二个标记。所有证据链接必须是 HTTPS 持久化脱敏证据；只有没有接受任何低等级风险时，`known_risk_links` 才能为空数组。

<!-- mix-release-decision:v1 -->
```json
{
  "schema_version": 1,
  "product": "Mix",
  "version": "X.Y.Z",
  "tag": "vX.Y.Z",
  "repository": "owner/repository",
  "draft_release_url": "https://github.com/owner/repository/releases/tag/vX.Y.Z",
  "sha256sums_sha256": "<64-lowercase-hex>",
  "decision": "ACCEPT",
  "release_manager": "<name>",
  "decision_at": "<RFC3339 UTC>",
  "known_risk_links": [],
  "checks": {
    "codex_oauth_api_key_provider_matrix": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "codex_account_switch_history_rollback": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "claude_auth_and_native_resume": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "macos_aarch64_clean_install_uninstall": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "macos_x86_64_clean_install_uninstall": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "signed_update_and_failure_recovery": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "voiceover_keyboard_responsive_ui": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]},
    "privacy_security_support_signoff": {"result":"PASS","reviewer":"<name>","tested_at":"<RFC3339 UTC>","evidence":["https://…"]}
  }
}
```
