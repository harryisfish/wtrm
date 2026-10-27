# wtrm

扫描这台机器上的 git worktree。按最后活跃程度列出仓库、分支、所在文件夹和路径，在终端里多选删除，或只清掉依赖目录。

给本地有一堆 worktree、想按「已合并」「很久没动」「创建很久且最近没动」收拾的人用。扫描不联网，也不查 pull request。

## 安装

[GitHub Release](https://github.com/harryisfish/wtrm/releases) 提供 macOS（arm64、x86_64）、Linux（x86_64）和 Windows（x86_64）的二进制包。当前版本是 `v0.0.3`。

从源码构建需要 Rust 1.85 或更新：

```bash
cargo install --path .
```

## 用法

在终端里直接运行，进入交互界面：

```bash
wtrm
```

不带路径时，扫描 `HOME` 下已经存在的 `project`、`Projects`、`Developer`、`dev`、`src`、`code`、`repos`、`work`、`git`、`workspace`。当前目录不在这些根下面时，也会扫进去。`--all` 改为从 `HOME` 往下扫，最多 8 层，仍会跳过依赖目录和系统目录。也可以直接传入要扫的目录。没有 `HOME` 时，`--all` 会失败。

活跃时间取这三者中的最新值：最近一次提交、git index 的修改时间、worktree 目录的修改时间。只列出有附加 worktree 的仓库。主检出会显示，但不能删除。

启动后立刻进入界面，扫描在后台进行；空列表时只响应 `q`。扫描不走进 `node_modules` 这类依赖目录。脏工作区检测用 `git status --porcelain=v1 -unormal`，不会把未跟踪目录（例如巨大的 `node_modules`）逐文件走一遍。

扫描也跳过路径中包含 `trash` 的目录，不区分大小写，已登记的工作树也遵守这个排除规则。

删除走 `git worktree remove`，由 git 移除该 worktree 的目录。主检出不会被删。确认删除时输入 `y` 或 `delete` 才删干净的 worktree；路径已经不在、且未锁定的也可以。`f` 对脏工作区或锁定的 worktree 使用 `--force --force`。`b` 切换分支策略：保留、只删本地，或连 GitHub 远程一起删。别的检出还在用的分支会留下。Enter 不会执行删除。删除按条目进行，底部会留下 `last result`（`ok` / `blocked` / `failed` 和原因）；删完后立刻从列表去掉，不会再全盘扫描，需要最新状态时按 `r`。

常用筛选：

```bash
wtrm --list
wtrm --json
wtrm --merged
wtrm --inactive 14d
wtrm --created-before 30d --inactive 7d
wtrm --all
```

`--older-than` 是 `--inactive` 的别名。时长用 `s`、`m`、`h`、`d`、`w`。几个条件同时给出时是「并且」。已合并看默认分支的祖先：优先本地分支，本地没有时用对应的远程跟踪引用。不联网，也认不出 squash merge。

非交互输出在 `--list`、`--json`、子命令，或标准输出不是终端时使用。`--json` 输出稳定对象，错误放在 `errors` 里，不需要再解析 stderr。

## Agent / 脚本使用

优先走查询 → 计划 → 显式执行。不要模拟 TUI 按键。默认 dry-run，显式 `--yes` 才改磁盘。用 JSON 里的稳定 `id` 定位，避免路径截断和 shell 转义。

```bash
wtrm --json ~/project
wtrm plan --merged --json ~/project
wtrm delete --id <id> --dry-run --json
wtrm delete --id <id> --yes --json
wtrm delete --path /abs/worktree --yes --json
wtrm clean --path /abs/worktree --dry-run --json
wtrm clean --id <id> --yes --json
```

JSON 顶层固定为：

```json
{
  "schema_version": 1,
  "roots": ["~/project"],
  "scanned_repos": 12,
  "worktrees": [],
  "items": [],
  "errors": [],
  "complete": true,
  "scan_ms": 42
}
```

`worktrees` 仍是扫描结果，每项带 `id`。`plan` / `delete` / `clean` 的 `items` 固定包含 `action`、`id`、`path`、`reason`、`blocked_by`、`result`。`result` 为 `dry-run`、`ok`、`blocked` 或 `failed`。主检出、脏工作区、锁定会标 `blocked_by`。`scan_ms` 是这次扫描耗时（毫秒）。扫描完成但没有匹配时：`complete` 为 true、`worktrees` / `items` 为空。部分根读失败时错误在 `errors` 里，`complete` 仍为 true。JSON 写在 stdout；`--list` 在 stderr 是终端时会额外打一行耗时，JSON 模式不写这行。

`--list` 表格第一列是短 id（8 位）。完整 16 位 id 在 JSON 和 TUI 详情行。`delete` / `clean` 必须带 `--path` 或 `--id`。

## 自动回收 macOS 外置盘

`scripts/reclaim-space.py` 默认只生成报告。`--apply` 才回收已合并工作树和旧 Turbo 缓存，保留所有分支，不运行 `clean` 或 `--force`。

工作树必须创建满 7 天。程序检查 GitHub PR 已合并且 HEAD 完全相同，重新检查未提交内容、子模块、未推送提交、打开文件和 Codex 会话。锁定、嵌套、含 planning、状态未知的工作树会保留。Pinned 会话始终保护相关目录，最近 7 天的未归档会话也保护相关目录。无关目录的会话不锁住整盘；会话后来跨目录访问的行为无法只凭 cwd 推断，程序同时检查当时打开的文件。

Git 忽略的内容也会检查。自动删除只放行 `node_modules`、`target`、`.next`、`.turbo`、`.venv`、`venv`、`__pycache__`、`Pods`、`.gradle`、`DerivedData`、`build`、`dist`、`.artifacts` 这些可重建目录和 `.DS_Store`。其他 ignored 文件或目录，例如 `.env.local`、`.cursor` 配置和本地数据，会阻止回收并记入报告。此保护不改变手动 `wtrm delete` 的行为。

缓存按仓库合计主检出和附加工作树的 `.turbo/cache`。软上限是 5 GiB。超过上限时，按修改时间淘汰超过 24 小时、未被打开的完整 hash 组，包括归档、meta 和 manifest。近期或被占用的组可以让缓存暂时超过上限，报告会显示剩余超限量。未知文件保留。

缓存发现包括扫描根本身、wtrm 已发现的仓库，以及根目录下最多 8 层的主仓库，即使它没有附加工作树。发现过程跳过依赖和构建目录，找到仓库后不再进入其内部，最多检查 10 万个条目。

构建私有二进制后安装。`--volume-uuid` 使用 `diskutil info` 显示的卷 UUID，`--report-dir` 指向本任务 planning 的 `evidence/maintenance` 绝对路径。

```bash
CARGO_TARGET_DIR=.artifacts/target cargo build --locked --release
python3 scripts/install-reclaim-space.py \
  --wtrm "$PWD/.artifacts/target/release/wtrm" \
  --root /Volumes/ssd/project --mount /Volumes/ssd \
  --volume-uuid EXTERNAL-VOLUME-UUID \
  --report-dir /absolute/task-planning/evidence/maintenance
```

安装器写入 `~/.local/lib/wtrm-maintenance` 和 `~/Library/LaunchAgents/com.wtrm.maintenance.plist`，不覆盖现有 `wtrm`，也不加载任务。先运行安装器输出的 dry-run 命令，检查报告后再启用：

```bash
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.wtrm.maintenance.plist"
launchctl kickstart "gui/$(id -u)/com.wtrm.maintenance"
```

任务每小时运行，使用单实例锁，仅保留最近 20 份报告。外置盘身份不匹配、活动探测失败或 GitHub 状态无法确认时保留数据。文件和进程检查不能原子锁住随后启动的其他程序，程序会在删除前复核身份；不要把它当作并发写入锁。

停用定时任务：

```bash
launchctl bootout "gui/$(id -u)/com.wtrm.maintenance"
```

## 交互

| 键 | 作用 |
| --- | --- |
| `j` / `k` | 移动 |
| `space` | 多选；状态栏显示 `selection changed` |
| `m` | 只筛出已合并的，不改选择 |
| `i` | 只筛出不活跃的（默认 14 天），不改选择 |
| `s` | 只筛出创建超过 30 天、且最近 7 天没动的，不改选择 |
| `0` | 取消筛选，不改选择 |
| `a` | 切换当前可见列表里全部可删除项（不是无条件全选） |
| `d` | 确认删除 worktree |
| `x` | 只清理选中 worktree 里的依赖目录 |
| `y` 或输入 `delete` | 删除干净的 worktree；清理依赖时确认。底部显示每条 `ok`/`blocked`/`failed` |
| `f` | 强制删除，包括脏和锁定的 |
| `b` | 确认删除时切换分支策略：保留、只删本地、或连 GitHub 远程一起删 |
| `/` | 按文字过滤；Enter 应用，Esc 清空 |
| `r` | 重新扫描；失效的选择会提示 `selection pruned: N` |
| `q` | 退出 |

状态列用 `[main]`、`[dirty]`、`[clean]`、`[locked]`、`[merged]` 标记，颜色只做辅助。窄终端隐藏 folder 和 path，完整路径在选中行详情里。

启动时如果同时给了 `--created-before` 和 `--inactive`，`s` 改用这两个时长。只给 `--inactive` 时，它改的是 `i`，`s` 仍是创建 30 天、空闲 7 天。只给 `--created-before` 时，`s` 的创建时长改用它，空闲仍是 7 天。

`x` 删除这些目录，包括子目录里的同名目录：`node_modules`、`target`、`.next`、`.turbo`、`.venv`、`venv`、`__pycache__`、`Pods`、`.gradle`。worktree 还在。主检出和路径已经不存在的 worktree 不会被清理。

找到仓库后不再走进去，所以仓库内部的嵌套克隆不会被看到。那种目录需要单独当作扫描根。符号链接目录也会跳过。

## 和其他工具

机器范围的清理已经有人做了。wtrm 只做本地盘点：最后活跃时间、创建时间、是否已合并、所属文件夹、仓库，以及多选删除或清理依赖。

- [wtkill](https://github.com/ohernandezdev/wtkill)：最接近。递归扫描、年龄、体积，TUI 删除，也有 JSON。
- [gh-reaper](https://github.com/ai-ecoverse/gh-reaper)：`gh` 扩展。按年龄和体积列出，可核对 PR 是否已合并。
- [gwm](https://github.com/kbrdn1/gwm-cli)：单仓库和多仓库 TUI，能创建、跳转、清理。
- [wisetree](https://github.com/victorcorcos/wisetree)：单个仓库的仪表盘，按状态批量删除。
- [worktrunk](https://github.com/max-sixty/worktrunk)：创建、切换、合并，不是全机盘点。

## 许可证

[MIT](LICENSE)
