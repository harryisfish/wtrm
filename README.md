# wtrm

扫描本机的 git worktree，按最后活跃程度列出仓库、分支、所在文件夹和路径，并在终端里多选删除。

活跃时间取这三者中的最新值：最近一次提交、git index 的修改时间、worktree 目录的修改时间。

只列出有附加 worktree 的仓库。主检出会显示出来，但不能删除。删除走 `git worktree remove`，不会直接 `rm -rf`。Enter 只删除干净的；`f` 才会对脏工作区或锁定的 worktree 使用 `--force --force`。

## 用法

```bash
cargo run --release
cargo run --release -- --list
cargo run --release -- --json
cargo run --release -- ~/project/some-repo --older-than 14d
cargo run --release -- --all
```

不带路径时，会扫描主目录下已经存在的 `project`、`Projects`、`Developer`、`dev`、`src`、`code`、`repos`、`work`、`git`、`workspace`。

交互键：

| 键 | 作用 |
| --- | --- |
| `j` / `k` | 移动 |
| `space` | 多选 |
| `a` | 选中或取消当前列表里全部可删除项 |
| `d` | 确认删除 |
| `Enter` | 只删除干净的 |
| `f` | 强制删除，包括脏和锁定的 |
| `/` | 过滤 |
| `r` | 重新扫描 |
| `q` | 退出 |

找到仓库后不再走进去，所以仓库内部的嵌套克隆不会被看到。那种目录需要单独当作扫描根。符号链接目录也会跳过。

## 同类工具

机器范围的清理已经有人做了：

- [wtkill](https://github.com/ohernandezdev/wtkill)：最接近。递归扫描、年龄、体积，TUI 删除，也有 JSON。Go。
- [gh-reaper](https://github.com/ai-ecoverse/gh-reaper)：`gh` 扩展。按年龄和体积列出，可核对 PR 是否已合并，确认后删除。
- [gwm](https://github.com/kbrdn1/gwm-cli)：Rust。单仓库和多仓库 TUI，能创建、跳转、清理，还有撤销。
- [wisetree](https://github.com/victorcorcos/wisetree)：Rust + Ratatui。单个仓库的仪表盘，按状态批量删除。
- [worktrunk](https://github.com/max-sixty/worktrunk)：Rust。目前最常用的 worktree 工作流工具，重点是创建、切换、合并，不是全机盘点。
- [git-worktree-manager](https://github.com/DaveDev42/git-worktree-manager)（`gw`）：按当前目录发现范围，不维护全局清单。
- [wt-core](https://github.com/kioku/wt-core)、[gwq](https://github.com/d-kuro/gwq)、[norn](https://github.com/Sandbye/norn)、[copse](https://github.com/getsolaris/copse)：创建、跳转、tmux 或代理会话，不是全机清理。

wtrm 只做本地盘点：最后活跃时间、所属文件夹、仓库，以及多选删除。扫描时不联网，也不查 PR。
