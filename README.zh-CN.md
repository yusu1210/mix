# Mix

[English](README.md)

Mix 是面向 AI 编程客户端的本地优先账号与工作区管理器。它最核心的能力是：
一键切换 Codex 账号，不需要导出、导入会话，也不会丢失客户端原生历史。

Mix 同时提供 macOS App、CLI 和带鉴权的 Local Web；三种入口共用同一套
Rust Core 和本机数据。

## 支持范围

| 能力 | Codex | Claude Code |
| --- | --- | --- |
| 自动识别并添加当前身份 | 支持 | 仅创建环境 |
| 切换客户端全局账号 | 支持 | 不支持 |
| 官方与自定义 Provider | 支持 | 按环境配置 Provider |
| 项目隔离环境 | 支持 | 支持 |
| 发现原生会话 | 支持 | 支持 |
| 通过官方 CLI 恢复会话 | 支持 | 支持 |

Claude 的认证信息可能存放在 `CLAUDE_CONFIG_DIR` 之外，因此 Mix 不会把
Claude 环境描述成完全隔离的 OAuth 账号，也不会承诺无法验证的账号切换。

## 在 macOS 安装

Mix 支持 macOS 13 及以上版本，覆盖 Apple Silicon 和 Intel Mac。从本仓库
Releases 页面下载架构匹配的 DMG，打开后将 `mix.app` 拖入 Applications。
正式发布物会经过 Developer ID 签名和 Apple 公证，并附带 `SHA256SUMS` 与
SBOM 文件。

可选 CLI 安装包名为 `mix_<version>_cli_<architecture>.pkg`。它将签名后的
CLI Bundle 安装到 `/Library/Application Support/Mix`，并创建
`/usr/local/bin/mix`；如果该路径已有其他命令，安装会安全失败而不是覆盖。

从源码构建本地开发版：

```bash
nvm use
cd apps/macos
npm ci
npm run bootstrap:rust
npm run build:desktop
```

构建必须使用 `.nvmrc` 指定的 Node 版本和固定的 Rust 工具链。本地构建仅做
ad-hoc 签名，不等同于已公证的正式发布物。

## 第一次使用

1. 打开 Mix，连接 Codex。
2. 选择“添加当前账号”。Mix 会自动读取 Codex 当前身份，账号名称不是必填项。
3. 通过 Mix 的隔离官方登录流程添加另一个账号，或先在 Codex 登录后再添加。
4. 点击账号即可切换。Codex 桌面端正在运行时，Mix 会关闭并重新打开一次，
   确保新凭证和 Provider 路由同时生效。

切换过程包含前置校验、事务日志、结果验证与失败回滚。已有会话的正文、ID 和
位置始终保留在 Codex 原生目录。跨 Provider 切换时，Mix 只会在同一可恢复
事务内更新 Codex 必需的最小 Provider 元数据。

## CLI 与 Local Web

```bash
mix discover
mix connect codex
mix add
mix status
mix use Personal
mix sessions all --query payment
mix resume codex <resume-id>
mix web
```

`mix add` 自动生成可识别名称，`--label` 只是可选别名。`mix use` 接受无歧义
名称或不可变账号 ID。`mix web` 在随机本机回环端口打开共用 React 界面，
每次启动使用独立令牌。

项目隔离运行示例：

```bash
mix run codex Personal --workspace /absolute/path/to/project
mix connect claude
mix add claude --label Work
mix run claude Work --workspace /absolute/path/to/project
```

完整命令契约见 [CLI 文档](docs/CLI.md)。

## 本地优先安全边界

- Mix 不包含产品埋点、广告 SDK、云同步或自动崩溃上传。
- 凭证以不透明私有文件保存在 `~/.mix/credentials`，不会进入 Mix JSON、日志、
  命令行参数、诊断报告或 Web 界面。
- Codex 与 Claude 原生会话始终是唯一事实来源；Mix 没有 transcript 数据库，
  也不会导出或导入会话正文。
- 本地 HTTP 服务只监听 loopback，每次启动都需要随机令牌。
- 账号切换使用原子写入、持久事务日志、备份、身份校验和失败回滚。
- Mix 只会停止全局切换所需且由适配器明确拥有的桌面进程，不会把独立终端
  Agent 当成可重启进程。

准确边界见[隐私说明](PRIVACY.md)、[安全策略](SECURITY.md)和
[架构文档](docs/ARCHITECTURE.md)。

## 开发与贡献

仓库只有两种实现语言：

- Rust：Core、适配器、CLI、本地 HTTP 服务、事务、进程、凭证存储、发布工具
  和 Tauri Shell。
- TypeScript/React：Mac WebView 与 Local Web 共用界面。

提交 Pull Request 前运行完整门禁：

```bash
nvm use
sh scripts/quality-gate.sh
```

更多文档：

- [贡献指南](CONTRIBUTING.md)
- [更新记录](CHANGELOG.md)
- [社区行为准则](CODE_OF_CONDUCT.md)
- [平台支持](docs/PLATFORM-SUPPORT.md)
- [发布流程](docs/RELEASING.md)
- [卸载与数据保留](docs/UNINSTALLING.md)
- [支持策略](SUPPORT.md)

## 许可证

Mix 使用 [MIT License](LICENSE)。
