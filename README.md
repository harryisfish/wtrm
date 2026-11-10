# wtrm

扫描这台机器上的 git worktree。按最后活跃程度列出仓库、分支、所在文件夹和路径，在终端里多选删除，或只清掉依赖目录。

给本地有一堆 worktree、想按「已合并」「很久没动」「创建很久且最近没动」收拾的人用。手动扫描不联网，也不查 pull request；后台维护会联网核对 GitHub PR。

## 安装

[GitHub Release](https://github.com/harryisfish/wtrm/releases) 提供 macOS（arm64、x86_64）、Linux（x86_64）和 Windows（x86_64）的二进制包。当前版本是 `v0.0.5`。

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

启动后立刻进入界面，扫描在后台进行；初次扫描尚无数据时，只响应 `q`、`Esc` 和 `Ctrl+C`。扫描完成后的空列表仍可按 `r` 刷新或 `/` 过滤。扫描不走进 `node_modules` 这类依赖目录。脏工作区检测用 `git status --porcelain=v1 -unormal`，不会把未跟踪目录（例如巨大的 `node_modules`）逐文件走一遍。

扫描也跳过路径中包含 `trash` 的目录，不区分大小写，已登记的工作树也遵守这个排除规则。

删除走 `git worktree remove`，由 git 移除该 worktree 的目录。主检出不会被删。确认页列出全部目标和后果，可以滚动查看完整路径。普通删除默认保留分支，用 `y` 或输入 `delete` 后按 Enter 确认。只输入 `delete` 不会执行。脏工作区、状态未知或锁定的 worktree 会被普通删除拦住；路径已经不在、且未锁定的也可以删除。

`f` 只切换强制模式，不会立即删除。强制删除使用 `--force --force`，可能丢失未提交的改动。`b` 切换分支策略，默认保留本地和远程分支。只删本地会使用 `git branch -D`，包括未合并的分支；GitHub 策略在本地分支删除成功后，再用 `git push --delete` 删除 GitHub 远程分支。别的检出还在用的分支会留下。强制删除或任一种分支删除策略都属于高风险确认，必须输入 `delete` 后按 Enter，`y` 不执行。切换 `f` 或 `b` 会清空已输入的确认词。终端太小而无法同时显示后果和操作提示时，确认会禁用，并提示调整终端大小。

操作结束后进入完整结果页，先显示失败和被拦住的条目，再显示成功条目。每条结果的完整路径、原因和多行错误都可以滚动查看，`R` 可重新打开结果和扫描读取错误。成功删除的 worktree 立刻从列表去掉；成功清理依赖的 worktree 会保留。不再全盘扫描，需要最新状态时按 `r`。

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

`delete --expected-head <完整 commit hash>` 只接受一个目标，删除前复核提交和分支。加 `--force` 可丢弃未提交内容，但仍保留锁定、缺失、状态未知、含子模块或已注册嵌套工作树的目标。这个组合只向 Git 传一个 `--force`，不会越过锁定保护。 这些额外保护在 dry-run 中返回 `blocked` 和 `blocked_by`，执行时可能返回 `failed` 且 `blocked_by` 为空，拒绝原因在 `reason` 中，目标仍保留。不带 `--expected-head` 的手动强制删除仍使用两个 `--force`。

## 后台定期清理 macOS 外置盘

时间命中并不代表本地内容已保存。允许丢弃未提交修改和未跟踪文件的策略必须显式配置，执行前应先检查 dry-run 报告。

`wtrm maintain --config <path>` 每次扫描并生成一份报告，默认 dry-run。显式 `--yes` 才删除，`--apply` 仍可使用。macOS LaunchAgent 定期调用此入口，不需要保持终端打开。

后台维护的初始扫描最多等待 5 分钟。单个 `wtrm delete` 调用只重新扫描目标所属仓库，包含安全复核和删除，最多等待 10 分钟。普通外部命令各最多等待 60 秒。初始扫描失败、结果不完整或超时时，会停止本轮清理并将原因写入报告，不会跳过检查继续删除。删除调用超时会记录该项原因，不会报告为已删除。

单项删除的 CLI 错误和非零退出码保存在该项 `warnings` 中。执行报告只有在唯一返回项的操作、id 和路径完全匹配、结果为 `ok`，且目标路径确认不存在时才记为 `removed`；命令同时报告其他扫描错误时，仍保留这个已确认的删除结果和警告。结果未知、失败、不匹配或路径仍存在时记为 `skip`。dry-run 遇到任何命令错误都不会记为 `would_remove`。

创建年龄和空闲年龄按「或者」匹配。例如 `--created-before 30d --inactive 7d` 允许创建满 30 天或最近 7 天未活跃的工作树进入安全检查。只要一个条件满足即可，创建很久但刚活跃的工作树也可能匹配。单个条件用 `none` 禁用，两个条件不能同时禁用。默认只启用创建满 7 天，间隔为 `1h`。时长接受正整数和 `s`、`m`、`h`、`d`、`w`，不带单位时为秒，拒绝零、负数和溢出值。手动查询的多个条件仍是「并且」。

空闲年龄沿用扫描的近似活跃时间，即提交、git index 和 worktree 根目录的最新修改时间，不能准确反映所有文件阅读或嵌套文件活动。未知、无效或未来的时间不会命中对应条件。两种策略都检查目标工作树及其专属 Git 管理目录中的实时进程 cwd 和打开文件。父目录 cwd、共享 Git 对象访问和兄弟工作树的 Git 管理文件不会单独阻止回收。主检出、锁定、缺失、嵌套、状态未知，以及包含当前进程目录、维护程序、配置或报告的工作树会保留。

默认保守策略还要求 GitHub PR 已合并且 HEAD 完全相同，并重新检查未提交内容和未推送提交。含 `planning` 或 `.planning` 的工作树会保留。此策略还检查 Codex 会话元数据，pinned 会话和最近 7 天的未归档会话会保护相关目录。无关目录的会话不锁住整盘。会话后来跨目录访问的行为无法只凭 cwd 推断，程序同时检查当时打开的文件。

默认保守策略也检查 Git 忽略的内容，只放行 `node_modules`、`target`、`.next`、`.turbo`、`.venv`、`venv`、`__pycache__`、`Pods`、`.gradle`、`DerivedData`、`build`、`dist`、`.artifacts` 这些可重建目录和 `.DS_Store`。其他 ignored 文件或目录，例如 `.env.local`、`.cursor` 配置和本地数据，会阻止回收并记入报告。此保护不改变手动 `wtrm delete` 的行为。

安装时显式加 `--discard-stale`，或在配置中设置 `"discard_stale": true`，允许丢弃年龄命中的工作树。此策略删除未提交修改、未跟踪文件和 ignored 本地内容，包括 `planning` 与 `.planning`，不要求 PR 已合并或提交已推送，也不调用 GitHub。它不读取 Codex 会话数据库，祖先目录或当前工作树的会话记录，包括 pinned 和最近 7 天的未归档记录，都不会阻止回收。文件不会备份，所有分支仍保留。它仍拒绝子模块和路径名含 `trash` 的内容，并保留实时占用、锁定、嵌套、维护程序、配置、报告、当前进程目录和卷身份保护。创建年龄与空闲年龄仍按「或者」匹配，创建阈值可命中刚编辑过的工作树。只按空闲年龄丢弃时使用 `--created-before none --inactive 14d --discard-stale`。

新安装默认只回收工作树，保留所有分支，不运行 `clean` 或 `--force`。只有 `discard_stale` 策略使用带 `--expected-head` 的强制删除。加 `--include-cache` 才同时回收旧 Turbo 缓存。缓存按仓库合计主检出和附加工作树的 `.turbo/cache`，软上限是 5 GiB。超过上限时，按修改时间淘汰超过 24 小时、未被打开的完整 hash 组，包括归档、meta 和 manifest。近期或被占用的组可以让缓存暂时超过上限，报告会显示剩余超限量。未知文件保留。

缓存发现包括扫描根本身、wtrm 已发现的仓库，以及根目录下最多 8 层的主仓库，即使它没有附加工作树。发现过程跳过依赖和构建目录，找到仓库后不再进入其内部，最多检查 10 万个条目。

构建私有二进制后安装。`--volume-uuid` 使用 `diskutil info` 显示的卷 UUID，`--report-dir` 指向本任务 planning 的 `evidence/maintenance` 绝对路径。

```bash
CARGO_TARGET_DIR=.artifacts/target cargo build --locked --release
python3 scripts/install-reclaim-space.py \
  --wtrm "$PWD/.artifacts/target/release/wtrm" \
  --root /Volumes/Work/projects --mount /Volumes/Work \
  --volume-uuid EXTERNAL-VOLUME-UUID \
  --created-before 30d --inactive 7d --interval 6h \
  --report-dir /absolute/task-planning/evidence/maintenance
```

仅按空闲年龄清理时，改用 `--created-before none --inactive 14d`。安装器写入 `~/.local/lib/wtrm-maintenance` 和 `~/Library/LaunchAgents/com.wtrm.maintenance.plist`，不覆盖现有 `wtrm`，也不加载任务。策略保存在私有 `config.json`：

```json
{
  "root": "/Volumes/Work/projects",
  "mount": "/Volumes/Work",
  "volume_uuid": "EXTERNAL-VOLUME-UUID",
  "wtrm": "/absolute/private-install/wtrm",
  "codex_db": "/absolute/home/.codex/state_5.sqlite",
  "report_dir": "/absolute/task-planning/evidence/maintenance",
  "created_before": "30d",
  "inactive": "7d",
  "reclaim_cache": false,
  "discard_stale": false
}
```

直接修改 JSON 策略会在下一轮生效，`null` 禁用对应年龄条件。`discard_stale` 必须为布尔值，省略时为 `false`。`codex_db` 字段仍需保留，但 `discard_stale` 模式不要求该路径存在。安装器输出的 `discard_stale` 和报告的 `worktree_policy.discard_stale` 标明所选策略。`maintain` 从配置文件读取根目录和阈值，拒绝命令行扫描根、筛选参数和非默认扫描深度；上述年龄参数传给安装器。原有六字段配置继续按创建满 7 天、开启缓存回收和保守策略运行。运行间隔保存在 plist 的 `StartInterval` 秒数中。修改间隔时先停用任务，再用新参数重新运行安装器并加载。

先运行安装器输出的 dry-run 命令，检查报告后再启用：

```bash
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.wtrm.maintenance.plist"
launchctl kickstart "gui/$(id -u)/com.wtrm.maintenance"
```

定时任务以私有 `wtrm maintain --config <path> --yes` 为主进程。`maintain` 只启动自身目录中的普通文件 `reclaim-space.py`，拒绝符号链接和其他用户可写的目录或脚本。它固定使用 `/usr/bin/python3 -I` 隔离模式，等待子进程完成并传递退出状态。维护需要 `git`、`diskutil` 和 `lsof`。默认保守策略还需要可读的 Codex 会话数据库和已认证的 `gh`，`discard_stale` 策略不需要这两项。外置盘权限仍由 macOS 系统设置控制，安装器不修改权限。此维护入口只支持 macOS，不影响其他平台的手动扫描和删除。

任务按配置间隔运行，每轮重新扫描，使用同一安装目录的单实例锁，仅保留最近 20 份报告。launchd 不会并发启动同一任务，错过的周期不会堆积。不要用多个安装目录同时清理同一个扫描根，它们不共享锁。外置盘身份不匹配或活动探测失败时保留数据。默认保守策略在 GitHub 状态无法确认时也保留数据。文件和进程检查不能原子锁住随后启动的其他程序，程序会在删除前复核身份；不要把它当作并发写入锁。

停用定时任务：

```bash
launchctl bootout "gui/$(id -u)/com.wtrm.maintenance"
```

## 交互

列表高亮表示当前行，`●` 表示已选中，`·` 表示受保护的主检出或裸仓库。移动当前行不会改变多选结果。

| 键 | 作用 |
| --- | --- |
| `j` / `k` 或方向键 | 浏览时移动；查看页和确认页滚动 |
| `PgUp` / `PgDn`、`Home` / `End` | 翻页、到开头或末尾；查看页和确认页按正文滚动 |
| `Enter` | 浏览时查看当前 worktree 的完整详情 |
| `space` | 多选；持续显示已选总数及被筛选隐藏的数量 |
| `v` | 查看全部已选 worktree，包括被筛选隐藏的目标 |
| `c` | 清空全部选择，包括被筛选隐藏的目标 |
| `m` | 只筛出已合并的，不改选择 |
| `i` | 只筛出不活跃的（默认 14 天），不改选择 |
| `s` | 只筛出创建超过 30 天、且最近 7 天没动的，不改选择 |
| `0` | 浏览时清空文字和预设筛选，不改选择 |
| `a` | 切换当前可见列表里全部可删除项（不是无条件全选） |
| `d` | 打开全部已选 worktree 的删除确认页；未选择时使用当前行 |
| `x` | 打开全部已选 worktree 的依赖清理确认页；未选择时使用当前行 |
| `y` 或输入 `delete` 后按 `Enter` | 确认普通删除；清理依赖仅用 `y`。强制或删除分支必须输入 `delete` 后按 `Enter` |
| `f` | 删除确认页切换强制模式，包括脏和锁定的 worktree；不会直接执行 |
| `b` | 确认删除时切换分支策略：保留、只删本地、或连 GitHub 远程一起删 |
| `/` | 按文字过滤；Enter 应用，Esc 清空 |
| `R` | 查看全部操作结果和扫描读取错误 |
| `?` | 查看完整快捷键与保护规则 |
| `r` | 重新扫描；失效的选择会提示 `selection pruned: N` |
| `Esc` / `q` | 查看页返回；确认页用 Esc 取消；浏览时退出 |
| `Ctrl+C` | 从任意页面退出 |

状态列用 `[main]`、`[dirty]`、`[clean]`、`[locked]`、`[merged]` 标记，颜色只做辅助。宽窄终端都用 `active` 表示距最后活跃的时间；未知时间不标成陈旧警告。窄终端隐藏 folder 和 path，按 Enter 可滚动查看完整路径、ID、状态与日期。

文字和预设筛选持续显示在表格上方。`m`、`i`、`s` 会替换预设并清空文字筛选；之后用 `/` 输入文字可叠加筛选。筛选不改选择，删除和清理始终使用全部已选目标。没有匹配和没有附加 worktree 会分别提示；有选择被隐藏时可用 `v` 核对，或用 `c` 全部清空。

启动时如果同时给了 `--created-before` 和 `--inactive`，`s` 改用这两个时长。只给 `--inactive` 时，它改的是 `i`，`s` 仍是创建 30 天、空闲 7 天。只给 `--created-before` 时，`s` 的创建时长改用它，空闲仍是 7 天。

`x` 删除这些目录，包括子目录里的同名目录：`node_modules`、`target`、`.next`、`.turbo`、`.venv`、`venv`、`__pycache__`、`Pods`、`.gradle`。worktree 还在。主检出和路径已经不存在的 worktree 不会被清理。

找到仓库后不再走进去，所以仓库内部的嵌套克隆不会被看到。那种目录需要单独当作扫描根。符号链接目录也会跳过。

## 和其他工具

机器范围的清理已经有人做了。wtrm 提供本地盘点、交互删除和依赖清理，也可在 macOS 上按配置定期回收满足保护条件的旧 worktree。

- [wtkill](https://github.com/ohernandezdev/wtkill)：最接近。递归扫描、年龄、体积，TUI 删除，也有 JSON。
- [gh-reaper](https://github.com/ai-ecoverse/gh-reaper)：`gh` 扩展。按年龄和体积列出，可核对 PR 是否已合并。
- [gwm](https://github.com/kbrdn1/gwm-cli)：单仓库和多仓库 TUI，能创建、跳转、清理。
- [wisetree](https://github.com/victorcorcos/wisetree)：单个仓库的仪表盘，按状态批量删除。
- [worktrunk](https://github.com/max-sixty/worktrunk)：创建、切换、合并，不是全机盘点。

## 许可证

[MIT](LICENSE)
