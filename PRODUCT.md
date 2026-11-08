# wtrm

<!-- impeccable:product-schema 1 -->

## Platform

Terminal CLI / TUI。现有 Rust 应用，使用 ratatui 与 crossterm。产品不属于 web、iOS 或 Android。

## Users

同等服务两类用户：本机维护多个 Git worktree 的开发者，以及通过稳定 JSON 协议操作的 Agent / 自动化脚本。

## Product Purpose

盘点本机 Git worktree，按活跃程度、创建时间和合并状态判断清理对象，安全地删除附加 worktree 或只清理依赖目录。

## Positioning

面向机器范围的本地盘点，不局限于当前仓库。以本地 Git 信息作判断，不联网查询 PR，也不把 squash merge 当作已经识别。

## Operating Context

开发者通过 TUI 浏览、筛选、多选并确认操作。脚本通过查询、计划、dry-run 和显式执行的 JSON 协议操作，用稳定 id 定位，不解析截断路径或模拟 TUI 删除按键。

## Capabilities and Constraints

- 保持终端工具形态，扫描不联网。
- 主检出不可删除，worktree 删除必须调用 git worktree remove。
- 破坏性操作必须显式确认。Agent / 脚本默认 dry-run，使用 --yes 执行。
- TUI 的颜色只辅助表达状态，文字标记必须保留。
- 支持按已合并、不活跃、创建时间筛选。合并判断基于默认分支祖先关系，不能识别 squash merge。
- 依赖清理保留 worktree；脏工作区、锁定状态与分支删除策略必须明确表达。

## Evidence on Hand

README.md 记录用法和 JSON 契约。src/ui.rs 实现交互界面，src/scan.rs 实现扫描，src/ops.rs 与 src/remove.rs 实现操作及安全规则。scripts/smoke-wtrm.sh 提供现有冒烟验证。

## Product Principles

- 终端用户与自动化调用方同等重要。
- 清楚呈现对象和风险，再执行清理。
- 用稳定协议支持自动化，用可见状态支持人工判断。
- 保持本地扫描的事实边界，不暗示远程 PR 已核验。
