# wtrm

扫描本机的 git worktree，按最后活跃程度列出仓库、分支、所在文件夹和路径，并在终端里多选删除。

活跃时间取这三者中的最新值：最近一次提交、git index 的修改时间、worktree 目录的修改时间。

只列出有附加 worktree 的仓库。主检出会显示出来，但不能删除。删除走 `git worktree remove`，不会直接 `rm -rf`。Enter 只删除干净的；`f` 才会对脏工作区或锁定的 worktree 使用 `--force --force`。

## 用法

```bash
cargo run --release
cargo run --release -- --list
cargo run --release -- --json
cargo run --release -- --merged
cargo run --release -- --inactive 14d
cargo run --release -- --created-before 30d --inactive 7d
cargo run --release -- --all
```

`--older-than` 是 `--inactive` 的别名。几个条件同时给出时是「并且」。已合并只看本地默认分支的祖先，不联网，也认不出 squash merge。交互里的 `i` 默认 14 天，`s` 默认创建超过 30 天且最近 7 天没动；启动时如果同时给了 `--created-before` 和 `--inactive`，`s` 改用这两个数。

不带路径时，会扫描主目录下已经存在的 `project`、`Projects`、`Developer`、`dev`、`src`、`code`、`repos`、`work`、`git`、`workspace`。

交互键：

| 键 | 作用 |
| --- | --- |
| `j` / `k` | 移动 |
| `space` | 多选 |
| `m` | 筛出已合并的，并选中 |
| `i` | 筛出不活跃的（默认 14 天），并选中 |
| `s` | 筛出创建超过 30 天、且最近 7 天没动的，并选中 |
| `0` | 取消筛选 |
| `a` | 选中或取消当前列表里全部可删除项 |
| `d` | 确认删除 worktree |
| `x` | 只清理选中 worktree 里的依赖目录 |
| `Enter` | 删除时只删干净的；清理依赖时直接确认 |
| `f` | 强制删除，包括脏和锁定的 |
| `/` | 按文字过滤 |
| `r` | 重新扫描 |
| `q` | 退出 |

`x` 会删掉 `node_modules`、`target`、`.next`、`.turbo`、`.venv`、`venv`、`__pycache__`、`Pods`、`.gradle`，包括子目录里的同名目录。worktree 本身还在。

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

wtrm 做本地盘点：最后活跃时间、创建时间、是否已合并、所属文件夹、仓库，以及多选删除或清理依赖。扫描时不联网，也不查 PR。
