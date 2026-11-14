# Uninstalling Mix / 卸载 Mix

Uninstalling the application and erasing local data are intentionally separate
actions. Removing Mix must never imply removing Codex or Claude history.

卸载应用与清除本机数据是两个刻意分开的操作。删除 Mix 绝不应连带删除
Codex 或 Claude 的历史记录。

## Remove only the Mac App / 仅移除 Mac App

1. Choose **Quit Mix** from the menu-bar menu. Closing the window is not the
   same as quitting the menu-bar app.
2. Move the inspected `/Applications/mix.app` bundle to Trash.

This leaves Mix settings, saved credentials, and recoverable isolated runtimes
under `~/.mix`, plus all native client directories. In particular, do not
remove `~/.codex` or `~/.claude` when uninstalling Mix.

1. 在菜单栏选择 **退出 Mix**。关闭窗口并不等于退出菜单栏应用。
2. 将已经确认过的 `/Applications/mix.app` 移到废纸篓。

这会保留 `~/.mix` 中的设置、已保存凭证与可恢复隔离 Runtime，以及客户端
自身的全部原生目录。卸载 Mix 时尤其不要删除 `~/.codex` 或 `~/.claude`。

## Remove only the CLI / 仅移除 CLI

Use the ownership-checking removal procedure in [CLI.md](CLI.md#macos-installation).
It verifies both the command link and bundle identifier before removing the
installed CLI. Removing a copied binary or an unrelated `/usr/local/bin/mix`
is not part of Mix uninstall.

使用 [CLI.md](CLI.md#macos-installation) 中带所有权校验的卸载步骤。它会先
核对命令链接和 Bundle Identifier，再删除已安装 CLI。来源不明的同名二进制
或不属于 Mix 的 `/usr/local/bin/mix` 不在卸载范围内。

## Erase Mix-managed data / 清除 Mix 管理的数据

Do this only when the data itself should be erased, not for an ordinary App or
CLI upgrade:

1. Open Mix and remove saved accounts and environments individually. This lets
   the Core remove each unshared local credential and mark projected runtime
   credentials for cleanup.
2. Quit Mix and close every Codex or Claude process launched from a Mix
   isolated runtime.
3. Review `~/.mix/runtime` before deleting anything. It can contain native
   sessions created inside isolated environments. Resume or back up any work
   that must be kept using the owning client.
4. In Finder, use **Go to Folder…**, inspect `~/.mix`, and move that exact
   folder to Trash only after the previous checks. Never delete `~/.codex` or
   `~/.claude` as part of this operation.
5. If Mix reports that cleanup is still pending, keep `~/.mix/config.json` and
   retry after restoring `~/.mix` ownership and write permissions; the file
   retains the references required for safe cleanup.

仅在确实要清除数据时执行，不要把它当作普通升级步骤：

1. 在 Mix 中逐个删除已保存账号和环境，使 Core 删除未被共享的本地凭证，
   并登记隔离 Runtime 中待清理的投影凭证。
2. 退出 Mix，并关闭所有从 Mix 隔离 Runtime 启动的 Codex 或 Claude 进程。
3. 删除任何内容前检查 `~/.mix/runtime`。其中可能包含在隔离环境内产生的
   原生会话；需要保留的工作应先通过所属客户端恢复或备份。
4. 在 Finder 中选择 **前往文件夹…**，检查 `~/.mix`，确认上述条件满足后
   仅将这个文件夹移到废纸篓。不要把 `~/.codex` 或 `~/.claude` 纳入清理。
5. 如果 Mix 提示仍有待完成清理，请保留 `~/.mix/config.json`，恢复
   `~/.mix` 的所有者和写入权限后重试；该文件保存了安全重试所需的引用。

An interrupted account switch is a recovery boundary. Finish **Recover** before
removing Mix data, or preserve the complete `~/.mix` directory and seek support;
otherwise the transaction journal and backup needed to restore the previous
login can be lost.

账号切换中断属于恢复边界。清除 Mix 数据前必须先完成 **恢复**；如果无法
完成，应完整保留 `~/.mix` 并寻求支持，否则可能丢失恢复上一登录状态所需的
事务日志与备份。
