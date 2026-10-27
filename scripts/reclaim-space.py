#!/usr/bin/env python3
"""Reclaim old, merged worktrees and cold Turbo cache entries on one volume."""

import argparse
import contextlib
import datetime
import fcntl
import json
import os
from pathlib import Path
import plistlib
import re
import sqlite3
import stat
import subprocess
import tempfile
import time
from dataclasses import dataclass


WORKTREE_AGE = 7 * 86400
CACHE_AGE = 86400
CACHE_LIMIT = 5 * 1024 ** 3
REPORT_LIMIT = 20
TREE_ENTRY_LIMIT = 100000
CACHE_NAME = re.compile(r"([a-f0-9]{16,64})(?:\.tar\.zst|-meta\.json|-manifest\.json)$")


class Unsafe(RuntimeError):
    pass


def command(args, check=True):
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_TERMINAL_PROMPT="0", GIT_PAGER="cat")
    try:
        result = subprocess.run(args, stdin=subprocess.DEVNULL, capture_output=True,
                                env=env, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise Unsafe("command unavailable or timed out: " + str(args[0])) from error
    if check and result.returncode:
        raise Unsafe("command failed: " + str(args[0]))
    return result


def git(path, *args, check=True):
    return command(["git", "-C", str(path), *args], check=check)


def text(result):
    return result.stdout.decode("utf-8", errors="strict").strip()


def safe_path(value):
    path = Path(value)
    if not path.is_absolute() or str(path) != os.path.abspath(str(path)):
        raise Unsafe("path must be absolute and normalized")
    if "trash" in str(path).lower():
        raise Unsafe("excluded path")
    for part in reversed([path, *path.parents]):
        if stat.S_ISLNK(part.lstat().st_mode):
            raise Unsafe("symbolic link in path: " + str(part))
    return path


def below(path, root):
    return path == root or root in path.parents


def prepare_directory(path):
    if not path.is_absolute() or "trash" in str(path).lower():
        raise Unsafe("invalid report directory")
    for part in reversed([path, *path.parents]):
        try:
            part.mkdir(mode=0o700)
        except FileExistsError:
            pass
        if not stat.S_ISDIR(part.lstat().st_mode):
            raise Unsafe("report directory contains a symbolic link or non-directory")
    return path


def identity(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink, info.st_size,
            info.st_mtime_ns, info.st_ctime_ns)


@dataclass
class Config:
    root: Path
    mount: Path
    volume_uuid: str
    wtrm: Path
    codex_db: Path
    report_dir: Path
    device: int = 0

    @classmethod
    def read(cls, path):
        data = json.loads(safe_path(path).read_text())
        fields = ("root", "mount", "volume_uuid", "wtrm", "codex_db", "report_dir")
        if set(data) != set(fields) or not all(isinstance(data[k], str) for k in fields):
            raise Unsafe("invalid configuration")
        return cls(Path(data["root"]), Path(data["mount"]), data["volume_uuid"],
                   Path(data["wtrm"]), Path(data["codex_db"]), Path(data["report_dir"]))

    def validate_volume(self):
        self.mount = safe_path(self.mount)
        self.root = safe_path(self.root)
        if not below(self.root, self.mount) or self.root == self.mount:
            raise Unsafe("project root must be inside the mounted volume")
        data = plistlib.loads(command(["diskutil", "info", "-plist", str(self.mount)]).stdout)
        if (data.get("VolumeUUID", "").lower() != self.volume_uuid.lower()
                or data.get("MountPoint") != str(self.mount)
                or not data.get("Mounted", False)):
            raise Unsafe("mounted volume identity does not match configuration")
        self.device = self.mount.stat().st_dev
        self.bound(self.root)
        safe_path(self.wtrm)

    def bound(self, value):
        path = safe_path(value)
        if not below(path, self.root) or path.stat().st_dev != self.device:
            raise Unsafe("path is outside the configured project volume")
        return path


@dataclass
class Activity:
    paths: list
    inodes: set
    captured: float
    root: Path

    @classmethod
    def capture(cls, config):
        result = command(["lsof", "-nP", "-F0pfnDi", "+f", "--", str(config.mount)])
        if result.stderr or not result.stdout:
            raise Unsafe("lsof activity is unavailable")
        paths, inodes = [], set()
        current, process = {}, False

        def finish():
            name = current.get("n")
            if name and name.startswith("/") and "trash" not in name.lower():
                paths.append((current.get("f"), Path(os.path.realpath(name))))
            if "D" in current and "i" in current:
                inodes.add((int(current["D"], 16), int(current["i"])))

        try:
            for token in result.stdout.split(b"\0"):
                token = token.lstrip(b"\n")
                if not token:
                    continue
                key, value = chr(token[0]), os.fsdecode(token[1:])
                if key == "p":
                    finish()
                    current = {}
                    process = bool(int(value))
                elif key == "f":
                    if not process:
                        raise ValueError("file without process")
                    finish()
                    current = {"f": value}
                elif key in ("n", "D", "i") and current:
                    current[key] = value
                else:
                    raise ValueError("unexpected lsof field")
            finish()
        except (ValueError, UnicodeError) as error:
            raise Unsafe("cannot parse lsof activity") from error
        if not process or not paths:
            raise Unsafe("incomplete lsof activity")
        return cls(paths, inodes, time.monotonic(), config.root)

    def fresh(self):
        if time.monotonic() - self.captured > 5:
            raise Unsafe("activity snapshot expired")

    def uses_tree(self, path):
        self.fresh()
        return any(below(name, path) or (fd == "cwd" and below(name, self.root) and below(path, name))
                   for fd, name in self.paths)

    def uses_file(self, path, info):
        self.fresh()
        return ((info.st_dev, info.st_ino) in self.inodes
                or any(name == path for _, name in self.paths))


def session_paths(config, now):
    database = safe_path(config.codex_db)
    try:
        with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=2)) as connection:
            connection.execute("PRAGMA query_only=ON")
            rows = connection.execute(
                "SELECT cwd,is_pinned,archived,updated_at,recency_at FROM threads "
                "WHERE coalesce(is_pinned,-1)!=0 OR coalesce(archived,-1)!=1").fetchall()
    except sqlite3.Error as error:
        raise Unsafe("Codex session metadata is unavailable") from error
    protected = []
    for cwd, pinned, archived, updated, recency in rows:
        if not isinstance(cwd, str) or not cwd.startswith("/"):
            raise Unsafe("unknown Codex session path")
        path = Path(os.path.realpath(cwd))
        if not (below(path, config.mount) or below(config.root, path)):
            continue
        if pinned not in (0, 1) or archived not in (0, 1):
            raise Unsafe("unknown Codex session state")
        stamps = [stamp for stamp in (updated, recency) if isinstance(stamp, (int, float))]
        if pinned or (not archived and stamps and max(stamps) >= now - WORKTREE_AGE):
            protected.append((path, "protected Codex session"))
        elif not archived and not stamps:
            raise Unsafe("unknown Codex session time")
    return protected


def session_reason(path, sessions):
    return next((reason for session, reason in sessions
                 if below(path, session) or below(session, path)), None)


def scan(config):
    result = json.loads(command([str(config.wtrm), "--json", str(config.root)]).stdout)
    if (not isinstance(result, dict) or result.get("schema_version") != 1
            or result.get("complete") is not True or result.get("errors")
            or not isinstance(result.get("worktrees"), list)
            or not all(isinstance(row, dict) for row in result["worktrees"])):
        raise Unsafe("incomplete wtrm scan")
    return result["worktrees"]


def github_repo(path):
    url = text(git(path, "remote", "get-url", "origin"))
    match = re.fullmatch(r"(?:git@github\.com:|https://github\.com/)([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?)(?:\.git)?", url)
    if not match:
        raise Unsafe("repository origin is not a supported GitHub URL")
    return match[1]


def merged_pr(path, branch, head):
    repository = github_repo(path)
    rows = json.loads(command([
        "gh", "pr", "list", "--repo", repository, "--state", "all", "--head", branch,
        "--limit", "100", "--json",
        "state,headRefOid,headRefName,headRepository,headRepositoryOwner,mergedAt"
    ]).stdout)
    if not isinstance(rows, list) or len(rows) >= 100:
        raise Unsafe("incomplete GitHub pull request result")
    if any(row.get("state") == "OPEN" for row in rows):
        raise Unsafe("branch has an open pull request")
    owner, name = repository.split("/")
    for row in rows:
        if (row.get("state") == "MERGED" and row.get("mergedAt")
                and row.get("headRefOid") == head and row.get("headRefName") == branch
                and (row.get("headRepository") or {}).get("name", "").lower() == name.lower()
                and (row.get("headRepositoryOwner") or {}).get("login", "").lower() == owner.lower()):
            return
    raise Unsafe("no merged pull request matches the exact branch and HEAD")


def clean_head(path, branch, head):
    if text(git(path, "rev-parse", "HEAD")) != head:
        raise Unsafe("HEAD changed")
    if text(git(path, "symbolic-ref", "--quiet", "--short", "HEAD")) != branch:
        raise Unsafe("branch changed")
    if git(path, "status", "--porcelain=v1", "-z", "--untracked-files=normal",
           "--ignore-submodules=none").stdout:
        raise Unsafe("worktree has uncommitted content")
    upstream = git(path, "rev-parse", "--verify", "@{upstream}", check=False)
    if not upstream.returncode:
        if int(text(git(path, "rev-list", "--count", "@{upstream}..HEAD"))):
            raise Unsafe("worktree has unpushed commits")
    elif upstream.returncode != 128:
        raise Unsafe("cannot determine upstream")


def check_contents(path):
    stack, count = [path], 0
    while stack:
        current = stack.pop()
        with open_directory(current) as fd, os.scandir(fd) as entries:
            for entry in entries:
                count += 1
                if count > TREE_ENTRY_LIMIT:
                    raise Unsafe("nested checkout check exceeded its bound")
                if "trash" in entry.name.lower():
                    raise Unsafe("excluded content inside worktree")
                if entry.name in (".planning", "planning"):
                    raise Unsafe("planning content inside worktree")
                if entry.name == ".git":
                    if current != path:
                        raise Unsafe("nested Git checkout")
                    continue
                if entry.is_dir(follow_symlinks=False):
                    stack.append(current / entry.name)


def reclaim_worktrees(config, worktrees, now, apply):
    results = []
    try:
        sessions = session_paths(config, now)
    except (Unsafe, OSError) as error:
        return [{"action": "skip_all", "reason": str(error)}]
    for row in worktrees:
        path = Path(row.get("path", ""))
        item = {"path": str(path), "action": "skip"}
        results.append(item)
        try:
            if any(not isinstance(row.get(flag), bool) for flag in ("main", "bare", "locked", "missing")):
                raise Unsafe("unknown worktree state")
            if row.get("main") or row.get("bare") or row.get("locked") or row.get("missing"):
                raise Unsafe("main, bare, locked, or missing worktree")
            created = row.get("created_at")
            if not isinstance(created, (int, float)) or created <= 0 or now - created < WORKTREE_AGE:
                raise Unsafe("worktree is younger than seven days or age is unknown")
            path = config.bound(path)
            config.bound(row["repo_root"])
            config.bound(row["git_common_dir"])
            if not safe_path(path / ".git").is_file():
                raise Unsafe("worktree has no Git marker")
            branch, head = row.get("branch"), row.get("head")
            if not branch or branch.startswith("(") or not re.fullmatch(r"[a-f0-9]{40,64}", head or ""):
                raise Unsafe("detached or unknown worktree identity")
            if any(below(Path(other.get("path", "")), path)
                   and Path(other.get("path", "")) != path for other in worktrees):
                raise Unsafe("registered nested worktree")
            reason = session_reason(path, sessions)
            if reason:
                raise Unsafe(reason)
            clean_head(path, branch, head)
            merged_pr(path, branch, head)
            check_contents(path)
            activity = Activity.capture(config)
            if activity.uses_tree(path) or activity.uses_tree(Path(row["git_common_dir"])):
                raise Unsafe("worktree is in use")
            clean_head(path, branch, head)
            current_sessions = session_paths(config, now)
            reason = session_reason(path, current_sessions)
            if reason:
                raise Unsafe(reason)
            config.bound(path)
            activity.fresh()
            args = [str(config.wtrm), "delete", "--id", row["id"], "--expected-head", head,
                    "--branches", "keep", "--json", "--yes" if apply else "--dry-run", str(config.root)]
            result = json.loads(command(args).stdout)
            items = result.get("items", [])
            expected = "ok" if apply else "dry-run"
            if (result.get("errors") or len(items) != 1 or items[0].get("result") != expected
                    or Path(items[0].get("path", "")) != path):
                raise Unsafe("wtrm refused or returned an incomplete delete result")
            item.update(action="removed" if apply else "would_remove", head=head)
        except (Unsafe, OSError, ValueError, KeyError) as error:
            item["reason"] = str(error)
    return results


def repository_caches(config, worktrees):
    repositories = {}
    with os.scandir(config.root) as entries:
        for entry in entries:
            if "trash" in entry.name.lower() or not entry.is_dir(follow_symlinks=False):
                continue
            path = Path(entry.path)
            if not (path / ".git").exists():
                continue
            try:
                config.bound(path)
                safe_path(path / ".git")
                config.bound(text(git(path, "rev-parse", "--path-format=absolute", "--git-common-dir")))
                if Path(text(git(path, "rev-parse", "--show-toplevel"))) != path:
                    continue
                repositories[path] = {path}
            except (Unsafe, OSError):
                continue
    for row in worktrees:
        try:
            repo = config.bound(row["repo_root"])
            path = config.bound(row["path"])
            if repo in repositories:
                repositories[repo].add(path)
        except (Unsafe, OSError, KeyError):
            continue
    return {repo: [path / ".turbo" / "cache" for path in sorted(paths)
                   if os.path.lexists(path / ".turbo" / "cache")]
            for repo, paths in repositories.items()}


@contextlib.contextmanager
def open_directory(path):
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for name in path.parts[1:]:
            new = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = new
        yield fd
    finally:
        os.close(fd)


def reclaim_cache(config, repo, directories, now, apply):
    result = {"repo": str(repo), "before_bytes": 0, "removed_bytes": 0,
              "would_remove_bytes": 0, "entries": []}
    groups, handles = [], contextlib.ExitStack()
    item = None
    try:
        with handles:
            for directory in directories:
                config.bound(directory)
                fd = handles.enter_context(open_directory(directory))
                grouped = {}
                for name in os.listdir(fd):
                    if "trash" in name.lower():
                        raise Unsafe("excluded cache content")
                    info = os.stat(name, dir_fd=fd, follow_symlinks=False)
                    if stat.S_ISDIR(info.st_mode):
                        raise Unsafe("unknown directory inside cache")
                    result["before_bytes"] += info.st_blocks * 512
                    match = CACHE_NAME.fullmatch(name)
                    if match:
                        grouped.setdefault(match[1], []).append((name, info))
                for members in grouped.values():
                    groups.append((directory, fd, sorted(members)))
            remaining = result["before_bytes"]
            groups.sort(key=lambda group: max(info.st_mtime for _, info in group[2]))
            activity = Activity.capture(config) if remaining > CACHE_LIMIT else None
            for directory, fd, members in groups:
                if remaining <= CACHE_LIMIT:
                    break
                item = {"paths": [str(directory / name) for name, _ in members], "action": "skip"}
                result["entries"].append(item)
                if any(not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                       or now - info.st_mtime < CACHE_AGE for _, info in members):
                    item["reason"] = "recent, linked, or non-regular cache member"
                    continue
                if not any(name.endswith(".tar.zst") for name, _ in members):
                    item["reason"] = "cache group has no archive"
                    continue
                if time.monotonic() - activity.captured > 2:
                    activity = Activity.capture(config)
                if any(activity.uses_file(directory / name, info) for name, info in members):
                    item["reason"] = "cache member is open"
                    continue
                if any(identity(os.stat(name, dir_fd=fd, follow_symlinks=False)) != identity(info)
                       for name, info in members):
                    item["reason"] = "cache group changed"
                    continue
                config.bound(directory)
                opened = os.fstat(fd)
                current = directory.stat()
                if (current.st_dev, current.st_ino) != (opened.st_dev, opened.st_ino):
                    raise Unsafe("cache directory changed")
                key = CACHE_NAME.fullmatch(members[0][0]).group(1)
                current_members = {name for name in os.listdir(fd)
                                   if CACHE_NAME.fullmatch(name) and CACHE_NAME.fullmatch(name).group(1) == key}
                if current_members != {name for name, _ in members}:
                    item["reason"] = "cache group membership changed"
                    continue
                activity.fresh()
                size = sum(info.st_blocks * 512 for _, info in members)
                if apply:
                    item["removed_paths"] = []
                    for name, info in members:
                        if identity(os.stat(name, dir_fd=fd, follow_symlinks=False)) != identity(info):
                            raise Unsafe("cache member changed during removal")
                        os.unlink(name, dir_fd=fd)
                        item["removed_paths"].append(str(directory / name))
                        result["removed_bytes"] += info.st_blocks * 512
                        remaining -= info.st_blocks * 512
                else:
                    result["would_remove_bytes"] += size
                    remaining -= size
                item["action"] = "removed" if apply else "would_remove"
            result["remaining_bytes"] = remaining
            result["protected_over_limit"] = max(0, remaining - CACHE_LIMIT)
    except (Unsafe, OSError, ValueError) as error:
        result["error"] = str(error)
        result["remaining_bytes"] = None
        result["protected_over_limit"] = None
        if item is not None:
            item["action"] = "partial" if item.get("removed_paths") else "error"
            item["reason"] = str(error)
    return result


def run(config, apply=False, now=None):
    now = time.time() if now is None else now
    result = {"started_at": datetime.datetime.fromtimestamp(now, datetime.timezone.utc).isoformat(),
              "apply": apply, "worktrees": [], "caches": [], "errors": [],
              "activity_scope": "canonical session cwd and open files; later cross-cwd use cannot be inferred"}
    try:
        config.validate_volume()
        before = os.statvfs(config.mount)
        result["free_before_bytes"] = before.f_bavail * before.f_frsize
        worktrees = scan(config)
        result["worktrees"] = reclaim_worktrees(config, worktrees, now, apply)
        for repo, directories in repository_caches(config, worktrees).items():
            if directories:
                result["caches"].append(reclaim_cache(config, repo, directories, now, apply))
        after = os.statvfs(config.mount)
        result["free_after_bytes"] = after.f_bavail * after.f_frsize
    except (Unsafe, OSError, ValueError, KeyError, plistlib.InvalidFileException) as error:
        result["errors"].append(str(error))
    result["status"] = "error" if result["errors"] or any(c.get("error") for c in result["caches"]) else "complete"
    return result


def save_report(directory, result):
    prepare_directory(directory)
    payload = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
    with tempfile.NamedTemporaryFile(mode="w", dir=directory, prefix=".run-", delete=False) as handle:
        handle.write(payload)
        temporary = handle.name
    target = directory / ("run-" + str(time.time_ns()) + ".json")
    os.replace(temporary, target)
    for old in sorted(directory.glob("run-*.json"), reverse=True)[REPORT_LIMIT:]:
        if stat.S_ISREG(old.lstat().st_mode) and not old.is_symlink():
            old.unlink()
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--apply", action="store_true", help="execute instead of dry-run")
    args = parser.parse_args()
    config = Config.read(args.config)
    prepare_directory(config.report_dir)
    lock_path = safe_path(config.wtrm.parent) / "maintenance.lock"
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print(json.dumps({"status": "skipped", "reason": "another run holds the lock"}))
            return 0
        result = run(config, args.apply)
        report = save_report(config.report_dir, result)
        print(json.dumps({"status": result["status"], "report": str(report)}))
        return 0 if result["status"] == "complete" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Unsafe, OSError, ValueError) as error:
        print(json.dumps({"status": "error", "reason": str(error)}))
        raise SystemExit(1)
