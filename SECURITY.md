# Security Policy / 安全策略

## Reporting a vulnerability / 报告漏洞

Do not open a public issue for a suspected credential leak, authentication bypass, path traversal, destructive state change, or code execution vulnerability. Use the repository's **Security → Advisories → Report a vulnerability** flow and include the affected version, platform, reproduction steps, impact, and whether real secrets or transcripts were exposed. Remove all real tokens, account identifiers, and transcript content from evidence.

疑似凭证泄漏、鉴权绕过、路径穿越、破坏性状态修改或代码执行问题，请勿提交公开 Issue。请使用仓库 **Security → Advisories → Report a vulnerability** 私密报告，并提供受影响版本、平台、复现步骤和影响范围；证据中必须删除真实 Token、账号标识和会话正文。

Maintainers aim to acknowledge a complete report within the severity target in
[SUPPORT.md](SUPPORT.md), establish impact and mitigation privately, and publish
a coordinated advisory after a fix is available. These are project targets,
not a paid support SLA.

维护者会按 [SUPPORT.md](SUPPORT.md) 对应严重等级的目标时间私密确认完整报告，
随后确定影响和缓解方案，并在修复可用后协调发布安全公告。这是开源项目目标，
不是付费支持 SLA。

## Scope

Security-sensitive surfaces include private credential files, local API authentication, process matching and termination, filesystem projection, backup/rollback, native-session discovery, runtime credential projection, Tauri startup, update signing, and release provenance. Mix never uses a client database as its own history store. During a verified Codex official/custom Provider transition, it may journal and update only the recognized `threads.model_provider` field in Codex's native index; it does not modify Claude indexes or conversation bodies. Mix reads a bounded amount of native-session metadata and transcript text to identify sessions and derive titles, but never uploads that content.

Only the latest tagged release receives security fixes during the 0.x period. Ad-hoc local builds are development artifacts; public releases must pass Developer ID signing, notarization, stapling, checksum, and provenance gates.

## Dependency policy

Every release runs `cargo audit` and `npm audit`; a known exploitable
vulnerability is release-blocking. Informational RustSec notices are reviewed
instead of hidden with ignore flags. The authoritative target-graph rationale
and update rule live in [docs/RELEASING.md](docs/RELEASING.md#supply-chain-rules).
