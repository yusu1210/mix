# Support / 支持

## English

- Usage questions and product feedback: GitHub Discussions when enabled.
- Reproducible non-security defects: GitHub Issues with Mix version, client version, expected/actual behavior, reproduction steps, and a redacted diagnostic report.
- Create the report in Settings → Support & diagnostics → Copy diagnostics, or run `mix diagnostics > ~/Desktop/mix-diagnostics.json`.
- Review every attachment before publishing it. Mix excludes credentials, account/profile names, paths, commands, environment values, and session content by design, but a user remains the final reviewer of shared data.
- Security vulnerabilities: follow [SECURITY.md](SECURITY.md), never a public issue.
- If an in-app update check fails, the current version remains unchanged. Retry from Settings → Software updates; if it still fails, open the official release page manually, verify `SHA256SUMS`, and install the notarized DMG without deleting `~/.mix` or native client history. Mix does not download or install updates in the background. Never install an update offered outside the official release endpoint.
- Commercial support: no paid SLA is offered by the open-source 0.x release. A future commercial plan must publish its response and recovery commitments separately.

### Severity and response targets

| Severity | Examples | Open-source project target |
| --- | --- | --- |
| Critical | Credential disclosure, remote/local code execution across the documented trust boundary, unrecoverable native-history loss | Private acknowledgement within 2 business days; containment decision within 5 business days |
| High | Authentication bypass, wrong-account mutation, transaction recovery failure with a safe manual recovery path | Private acknowledgement within 5 business days; triage and owner within 10 business days |
| Normal | Reproducible functional, accessibility, localization, packaging, or documentation defect without a critical/high impact | Triage when maintainer capacity permits; no guaranteed response time |

These are public project targets, not contractual SLAs. Repository
administrators should keep GitHub Issues and private Security Advisories
enabled; Discussions is the preferred channel for usage questions when it is
enabled.

## 中文

- 使用问题和产品建议：启用后通过 GitHub Discussions 提交。
- 可复现的非安全缺陷：通过 GitHub Issues 提交，并附 Mix 版本、客户端版本、预期/实际行为、复现步骤和脱敏诊断报告。
- 在“设置 → 支持与诊断 → 复制诊断报告”生成报告，或运行 `mix diagnostics > ~/Desktop/mix-diagnostics.json`。
- 发布附件前仍需自行复核。Mix 会按设计排除凭证、账号/环境名称、路径、命令、环境变量值及会话内容，但最终分享责任仍属于提交者。
- 安全漏洞：必须遵循 [SECURITY.md](SECURITY.md)，不要提交公开 Issue。
- 应用内检查更新失败时，当前版本保持不变。可在“设置 → 软件更新”重试；仍失败时，手动打开官方发布页，核对 `SHA256SUMS` 后安装已公证 DMG，不要删除 `~/.mix` 或客户端原生历史。Mix 不会在后台下载或安装更新。不要安装非官方发布端点提供的更新。
- 商业支持：开源 0.x 版本目前不提供付费 SLA；未来商业计划需另行公布响应与恢复承诺。

### 严重等级与响应目标

| 等级 | 示例 | 开源项目目标 |
| --- | --- | --- |
| 严重 | 凭证泄漏、突破既定信任边界的代码执行、不可恢复的原生历史丢失 | 2 个工作日内私密确认，5 个工作日内作出遏制决策 |
| 高 | 鉴权绕过、错误账号被修改、事务恢复失败但仍有安全人工恢复路径 | 5 个工作日内私密确认，10 个工作日内完成分级并明确负责人 |
| 一般 | 不造成严重/高等级影响的可复现功能、无障碍、本地化、打包或文档缺陷 | 按维护者精力分级，不承诺固定响应时间 |

以上是公开项目目标，不是合同 SLA。仓库管理员应保持 GitHub Issues 和私密
Security Advisories 可用；启用 Discussions 后，使用问题优先通过该渠道提交。
