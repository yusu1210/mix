# Mix Privacy / 隐私说明

Effective date / 生效日期：2026-09-06

## English

Mix is local-first. The current release has no product analytics, advertising SDK, account service, cloud sync, or automatic crash upload. The desktop app communicates with its bundled local core only through an authenticated random loopback port.

Mix processes the paths, client configuration, account labels, workspace bindings, activity records, backups, and native session metadata needed for features the user invokes. The session catalog reads a bounded amount of local transcript text to derive a useful title; it does not rewrite or upload that content. Saved Codex credentials and configured secret references are stored as private files under `~/.mix/credentials`. Raw credentials are not written to Mix configuration, activity logs, backups, or the webview. Native Codex and Claude transcripts remain in their client directories.

Mix can launch Codex and Claude Code. Those clients have their own network behavior and privacy terms; Mix does not control them. macOS, package registries, GitHub, and Apple notarization may also process data when the user downloads, builds, or verifies the software.

Local and development builds do not contact an update service. A signed release build can check a configured HTTPS release endpoint at launch; users can change this to manual checks in Settings. The request is limited to ordinary update metadata such as Mix version, target platform, and architecture and does not include credentials, account labels, workspace paths, diagnostics, or transcripts. The endpoint host (initially GitHub) may receive normal network metadata such as IP address and request time under its own privacy terms. Downloads and installation always require an explicit user action.

Support diagnostics are generated only when the user requests them. A report includes Mix/platform versions, health and object counts, capability flags, and operation-kind counts. It excludes credential values, account/profile names, workspace and filesystem paths, commands and environment values, and session titles, prompts, and transcript content. Copying or exporting a report does not upload it; the user chooses whether and where to share it and should review it first.

On macOS, Mix requests permission to write text to the system clipboard only when the user presses a copy action. The desktop capability does not grant clipboard read access.

Deleting an environment after explicit confirmation removes its Mix metadata and credentials that Mix projected into its isolated runtimes. Those runtime directories and their native session histories remain so that deleting an account record cannot silently erase work. Native client histories are not deleted as part of account switching. After closing Mix and any client using a Mix runtime, users can remove `~/.mix` to erase all locally managed Mix data; keep any recovery material and session history needed first.

Future telemetry, cloud sync, or crash reporting must be opt-in, documented here before release, and must not collect raw credentials or transcript bodies.

## 中文

Mix 是本地优先产品。当前版本不包含产品埋点、广告 SDK、账号云服务、云同步或自动崩溃上传。桌面应用只通过带每次启动令牌的随机本机回环端口与内置核心通信。

Mix 仅处理用户主动使用功能所需的客户端路径、配置、账号标签、工作区绑定、活动记录、备份及原生会话元数据。会话目录会在本机限量读取 transcript 文本以生成可识别的标题，但不会改写或上传这些内容。保存的 Codex 凭证和配置的密钥引用以私有文件形式存入 `~/.mix/credentials`；原始凭证不会写入 Mix 配置、活动日志、备份或 WebView。Codex 与 Claude 的原生会话正文保留在各自客户端目录中。

Mix 可以启动 Codex 和 Claude Code；这些客户端有各自的联网行为和隐私条款，不受 Mix 控制。用户下载、构建或验证软件时，macOS、包仓库、GitHub 和 Apple 公证服务也可能处理相关数据。

本地和开发构建不会连接更新服务。签名正式版可在启动时访问配置的 HTTPS 发布端点检查更新，用户可在设置中改为手动检查。请求仅包含 Mix 版本、目标平台和架构等常规更新元数据，不包含凭证、账号标签、工作区路径、诊断信息或 transcript。端点提供方（初期为 GitHub）可能依据其隐私条款处理 IP 地址、请求时间等普通网络元数据。下载和安装始终需要用户明确操作。

支持诊断报告只在用户主动请求时生成。报告包含 Mix/平台版本、健康状态与对象计数、能力标志及操作类型计数；按设计排除凭证值、账号/环境名称、工作区和文件系统路径、命令和环境变量值，以及会话标题、提示词和正文。复制或导出报告不会自动上传，是否分享及分享位置由用户决定，分享前应自行复核。

在 macOS 上，只有用户主动点击复制操作时，Mix 才使用系统剪贴板的文本写入能力；桌面权限不包含读取用户剪贴板。

删除环境需要明确确认；操作会移除对应的 Mix 元数据，并删除 Mix 投影到隔离 Runtime 中的凭证。Runtime 目录及其中的原生会话历史会保留，避免删除账号记录时静默丢失工作。账号切换同样不会删除原生历史。关闭 Mix 以及仍在使用 Mix Runtime 的客户端并保留必要恢复材料和会话历史后，用户可以删除 `~/.mix`，清除 Mix 在本机管理的全部数据。

未来若增加遥测、云同步或崩溃报告，必须默认关闭、在发布前更新本文，并且不得收集原始凭证或会话正文。
