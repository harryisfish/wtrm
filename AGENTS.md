# 项目指引

wtrm 扫描本机 git worktree。删除必须走 `git worktree remove`，主检出不可删。

Cloud Agent 需要 Rust 1.85 或更新。安装后二进制在 `/usr/local/bin/wtrm`。冒烟：`bash scripts/smoke-wtrm.sh`。完整二进制实测：`python3 scripts/e2e-wtrm.py`。

只记录本项目专属约束。通用规则由 ~/.agents/AGENTS.md 提供。
