#!/usr/bin/env python3
"""Drive the real wtrm binary through CLI and TUI. Temp dirs only."""
from __future__ import annotations

import json
import os
import pty
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TERM = "xterm-256color"
FAILS: list[str] = []
PASSES: list[str] = []


def log(ok: bool, name: str, detail: str = "") -> None:
    msg = name if not detail else f"{name}: {detail}"
    if ok:
        PASSES.append(msg)
        print(f"PASS: {msg}")
    else:
        FAILS.append(msg)
        print(f"FAIL: {msg}", file=sys.stderr)


def git(cwd: Path, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
    base = {
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        **os.environ,
    }
    if env:
        base.update(env)
    return subprocess.run(
        ["git", "-C", str(cwd), "-c", "user.email=wtrm@example.com", "-c", "user.name=wtrm", *args],
        env=base,
        check=True,
        capture_output=True,
        text=True,
    )


def init_repo(path: Path, branch: str = "main") -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "init", "-b", branch, str(path)],
        check=True,
        capture_output=True,
        env={**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"},
    )
    git(path, "commit", "--allow-empty", "-m", "init")


def worktree(repo: Path, dest: Path, branch: str) -> None:
    git(repo, "worktree", "add", "-b", branch, str(dest))


def wtrm_cmd(binary: str, *args: str, extra_env: dict | None = None) -> subprocess.CompletedProcess:
    env = {**os.environ, **(extra_env or {})}
    return subprocess.run([binary, *args], capture_output=True, text=True, env=env)


def tui(binary: str, scan_root: str, keys: bytes, timeout: float = 12.0) -> tuple[int, bytes]:
    env = os.environ.copy()
    env["TERM"] = TERM
    env["COLUMNS"] = "140"
    env["LINES"] = "40"
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir("/")
        os.execve(binary, [binary, scan_root], env)
    buf = b""
    ready = False
    sent = False
    deadline = time.time() + timeout
    try:
        while time.time() < deadline:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    chunk = os.read(fd, 8192)
                except OSError:
                    break
                if not chunk:
                    break
                buf += chunk
                if not ready and (
                    b"help" in buf or b"worktrees" in buf or b"j/k" in buf or b"confirm" in buf
                ):
                    ready = True
            if ready and not sent:
                time.sleep(0.2)
                for i, byte in enumerate(keys):
                    os.write(fd, bytes([byte]))
                    time.sleep(0.12 if keys[i : i + 1] not in (b"q",) else 0.05)
                sent = True
            wpid, status = os.waitpid(pid, os.WNOHANG)
            if wpid:
                code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else 1
                return code, buf
        if not sent:
            try:
                os.write(fd, keys + b"q")
            except OSError:
                pass
        for _ in range(40):
            wpid, status = os.waitpid(pid, os.WNOHANG)
            if wpid:
                code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else 1
                return code, buf
            time.sleep(0.1)
            try:
                os.write(fd, b"q")
            except OSError:
                pass
        os.kill(pid, 9)
        os.waitpid(pid, 0)
        return 1, buf + b"\nTIMEOUT"
    finally:
        try:
            os.close(fd)
        except OSError:
            pass


def main() -> int:
    binary = os.environ.get("WTRM") or shutil.which("wtrm") or "/usr/local/bin/wtrm"
    if not os.access(binary, os.X_OK):
        print("wtrm binary not found", file=sys.stderr)
        return 1
    root = Path(tempfile.mkdtemp(prefix="wtrm-e2e-"))
    print(f"using {binary} root={root}")
    try:
        run(binary, root)
    finally:
        shutil.rmtree(root, ignore_errors=True)
    print(f"\n{len(PASSES)} passed, {len(FAILS)} failed")
    for item in FAILS:
        print(f"  - {item}", file=sys.stderr)
    return 1 if FAILS else 0


def run(binary: str, root: Path) -> None:
    ver = wtrm_cmd(binary, "--version")
    log(ver.returncode == 0 and ver.stdout.startswith("wtrm "), "version", ver.stdout.strip())
    help_out = wtrm_cmd(binary, "--help")
    log("Scan git worktrees" in help_out.stdout and "--merged" in help_out.stdout, "help")

    empty = wtrm_cmd(binary, "--list", str(root / "empty"))
    Path(root / "empty").mkdir()
    empty = wtrm_cmd(binary, "--list", str(root / "empty"))
    log("no repos with linked worktrees" in empty.stdout, "empty list")

    bad = wtrm_cmd(binary, "--list", "--inactive", "nope", str(root / "empty"))
    log(bad.returncode != 0 and "cannot parse --inactive" in bad.stderr, "invalid duration")

    scan_home = root / "home"
    demo = scan_home / "project" / "demo"
    feat = scan_home / "project" / "demo-feature"
    idle = scan_home / "project" / "demo-idle"
    init_repo(demo)
    worktree(demo, feat, "feature")
    worktree(demo, idle, "idle")
    git(idle, "commit", "--allow-empty", "-m", "idle-commit")

    listed = wtrm_cmd(binary, "--list", str(scan_home / "project"))
    log("\tfeature\t" in listed.stdout and "clean,merged" in listed.stdout, "list merged feature")
    log("\tidle\t" in listed.stdout and "\tmain\t" in listed.stdout, "list idle and main")

    rows = json.loads(wtrm_cmd(binary, "--json", str(scan_home / "project")).stdout)
    by = {r["branch"]: r for r in rows}
    log(
        len(rows) == 3
        and by["main"]["main"] is True
        and by["feature"]["merged"] is True
        and by["idle"]["merged"] is False,
        "json branches",
    )

    merged = wtrm_cmd(binary, "--list", "--merged", str(scan_home / "project"))
    log("\tfeature\t" in merged.stdout and "\tidle\t" not in merged.stdout, "--merged filter")

    older = wtrm_cmd(binary, "--list", "--older-than", "0s", str(scan_home / "project"))
    log("\tfeature\t" in older.stdout, "--older-than alias")

    created = wtrm_cmd(binary, "--list", "--created-before", "8w", str(scan_home / "project"))
    log(
        "\tfeature\t" not in created.stdout and "no repos with linked worktrees" in created.stdout,
        "--created-before 8w",
    )

    env = {"HOME": str(scan_home)}
    default_list = wtrm_cmd(binary, "--list", extra_env=env)
    log("\tfeature\t" in default_list.stdout, "default HOME/project roots")
    all_list = wtrm_cmd(binary, "--list", "--all", "--max-depth", "6", extra_env=env)
    log("\tfeature\t" in all_list.stdout, "--all")

    code, _ = tui(binary, str(scan_home / "project"), b"q")
    log(code == 0, "TUI quit")

    code, _ = tui(binary, str(scan_home / "project"), b"mq")
    log(code == 0, "TUI merged preset then quit")
    code, _ = tui(binary, str(scan_home / "project"), b"/feat\x1bq")
    log(code == 0, "TUI text filter Esc then quit")

    keep_repo = root / "keep" / "repo"
    keep_wt = root / "keep" / "wt"
    init_repo(keep_repo)
    worktree(keep_repo, keep_wt, "keep-me")
    code, buf = tui(binary, str(root / "keep"), b"d\rq")
    gone = not keep_wt.exists()
    branches = git(keep_repo, "branch", "--list", "keep-me").stdout
    log(gone, "TUI delete worktree keeps dir gone", f"exit={code}")
    log("keep-me" in branches, "TUI delete default keeps local branch")

    loc_repo = root / "local" / "repo"
    loc_wt = root / "local" / "wt"
    init_repo(loc_repo)
    worktree(loc_repo, loc_wt, "drop-local")
    tui(binary, str(root / "local"), b"db\rq")
    log(not loc_wt.exists(), "TUI delete+local removes worktree")
    loc_branches = git(loc_repo, "branch", "--list", "drop-local").stdout
    log("drop-local" not in loc_branches, "TUI b once deletes local branch")
    log("main" in git(loc_repo, "branch", "--list", "main").stdout, "main branch kept after local delete")

    gh_bare = root / "github.com" / "demo.git"
    gh_bare.parent.mkdir(parents=True)
    subprocess.run(["git", "init", "--bare", "-b", "main", str(gh_bare)], check=True, capture_output=True)
    gh_repo = root / "gh" / "repo"
    init_repo(gh_repo)
    git(gh_repo, "remote", "add", "origin", str(gh_bare))
    git(gh_repo, "push", "-u", "origin", "main")
    gh_wt = root / "gh" / "wt"
    worktree(gh_repo, gh_wt, "gh-drop")
    git(gh_repo, "push", "-u", "origin", "gh-drop")
    tui(binary, str(root / "gh"), b"dbb\rq")
    log(not gh_wt.exists(), "TUI delete+github removes worktree")
    log(
        "gh-drop" not in git(gh_repo, "branch", "--list", "gh-drop").stdout,
        "TUI github scope deletes local branch",
    )
    remote_branches = subprocess.run(
        ["git", "--git-dir", str(gh_bare), "branch", "--list", "gh-drop"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    log("gh-drop" not in remote_branches, "TUI github scope deletes origin branch")

    dirty_repo = root / "dirty" / "repo"
    dirty_wt = root / "dirty" / "wt"
    init_repo(dirty_repo)
    worktree(dirty_repo, dirty_wt, "dirty-br")
    (dirty_wt / "extra.txt").write_text("x\n")
    tui(binary, str(root / "dirty"), b"d\rq")
    log(dirty_wt.exists(), "Enter refuses dirty worktree")
    tui(binary, str(root / "dirty"), b"dfq")
    log(not dirty_wt.exists(), "f force-deletes dirty worktree")

    lock_repo = root / "lock" / "repo"
    lock_wt = root / "lock" / "wt"
    init_repo(lock_repo)
    worktree(lock_repo, lock_wt, "locked-br")
    git(lock_repo, "worktree", "lock", str(lock_wt))
    tui(binary, str(root / "lock"), b"d\rq")
    log(lock_wt.exists(), "Enter refuses locked worktree")
    tui(binary, str(root / "lock"), b"dfq")
    log(not lock_wt.exists(), "f force-deletes locked worktree")

    main_repo = root / "mainonly" / "repo"
    main_wt = root / "mainonly" / "wt"
    init_repo(main_repo)
    worktree(main_repo, main_wt, "side")
    tui(binary, str(root / "mainonly"), b"j d\rq")
    log(main_repo.exists() and (main_repo / ".git").exists(), "main checkout still present")
    listed_after = wtrm_cmd(binary, "--list", str(root / "mainonly"))
    log("\tmain\t" in listed_after.stdout or main_repo.exists(), "main not deleted by TUI")

    clean_repo = root / "deps" / "repo"
    clean_wt = root / "deps" / "wt"
    init_repo(clean_repo)
    worktree(clean_repo, clean_wt, "deps-br")
    (clean_wt / "node_modules" / "pkg").mkdir(parents=True)
    (clean_wt / "node_modules" / "pkg" / "a.js").write_text("x\n")
    (clean_wt / "src").mkdir()
    (clean_wt / "src" / "app.js").write_text("ok\n")
    (clean_wt / "target" / "debug").mkdir(parents=True)
    tui(binary, str(root / "deps"), b"x\rq")
    log(clean_wt.exists(), "clean keeps worktree")
    log(not (clean_wt / "node_modules").exists(), "clean removes node_modules")
    log(not (clean_wt / "target").exists(), "clean removes target")
    log((clean_wt / "src" / "app.js").read_text() == "ok\n", "clean keeps source")

    cargo = subprocess.run(["cargo", "test", "--locked"], cwd="/workspace", capture_output=True, text=True)
    log(cargo.returncode == 0 and "14 passed" in cargo.stdout, "cargo test --locked", cargo.stdout[-400:])


if __name__ == "__main__":
    sys.exit(main())
