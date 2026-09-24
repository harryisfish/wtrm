# 已发布内容

新的记录写在最上面。正式版 tag 是 `vX.Y.Z`。Nightly 是预发布，不取代 latest。

## v0.0.2-nightly

2026-09-24。预发布，不取代 `v0.0.1`。

- 删除 worktree 时可以选择保留分支、只删除本地分支，或连 GitHub 远程分支一起删除。
- 别的检出还在使用的分支会留下，包括主检出的分支。

先前误发的 `v0.0.1-nightly.20260924` 已删除，内容改由这一版发布。

## v0.0.1

2026-09-23。当前正式版。

- 扫描这台机器上的 git worktree，按最后活跃程度列出仓库、分支、所在文件夹和路径。
- 在终端里多选删除。删除走 `git worktree remove`，主检出不能删除。
- 可以筛出已合并、不活跃，或创建很久且最近没有动作的 worktree。
- 可以只清理 worktree 里的依赖目录，worktree 本身还在。
- GitHub Release 提供 macOS、Linux 和 Windows 二进制包。
