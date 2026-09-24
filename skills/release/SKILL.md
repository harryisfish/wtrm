---
name: release
description: "wtrm 的手动发版。用户明确要求发版、打 tag、发布 nightly，或调用 /release 时使用。功能做完、合并、CI 变绿都不能自动发版。"
argument-hint: "stable patch|minor|major，或 nightly"
user-invocable: true
disable-model-invocation: true
---

# /release

只在用户当前消息明确要求发版时执行。没写 `stable` 还是 `nightly`，也没写 `patch` / `minor` / `major` 时，先问，不要默认。

发版在现有 `main` 检出上做。`git fetch origin main` 后能快进就快进。只暂存本次发版文件。不要为发版新建 worktree。

## 路由

| 调用 | 结果 |
| --- | --- |
| `/release stable patch\|minor\|major` | 正式版 `vX.Y.Z`，成为 GitHub Release 的 latest |
| `/release nightly` | 下一个未发布版本的预发布 `vX.Y.Z-nightly`，不取代 latest |

版本以 `Cargo.toml` 的 `version` 为准。正式版 tag 与这个版本相同。Nightly 用**下一个**版本，不能复用已经发布的正式版号。`v0.0.1` 已是正式版，之后的 nightly 从 `v0.0.2-nightly` 起。

## 正式版

1. 确认在 `main`，工作区里没有与本次发版无关的改动需要被带上。
2. 按用户给的 patch / minor / major 改 `Cargo.toml` 和 `Cargo.lock`。
3. 在 `references/releases.md` 顶部写下这一版的用户可见内容。只写新能力和体验改进。普通修复合并成一行「修复了若干已知问题」。安全类修复不写。
4. `cargo test` 与 `cargo clippy --all-targets -- -D warnings`。
5. 提交：`chore: release vX.Y.Z`。
6. 附注 tag：`vX.Y.Z`。推送提交和 tag。
7. `.github/workflows/release.yml` 会构建 macOS、Linux、Windows 并发布。等该 workflow 成功，再把 Release URL 回报给用户。

## Nightly

1. 正式版已经占用的版本号不能再当 nightly。需要先把 `Cargo.toml` 调到下一个版本。
2. 同样把内容写进 `references/releases.md`，并标明这是预发布。
3. 跑与正式版相同的测试。
4. 提交后打附注 tag `vX.Y.Z-nightly` 并推送。tag 名里含 `nightly` 时，workflow 会标成 prerelease，且 `make_latest` 为 false。
5. 同一天需要再发一版时，删掉同名 tag 和 Release 后再打，或改用 `vX.Y.Z-nightly.YYYYMMDD`。不要保留错误版本号的 nightly。

## 记录

已发布内容在 `references/releases.md`。新发版先改这份记录，再打 tag。记录用中文，从用户能做什么来写，不抄 commit message。
