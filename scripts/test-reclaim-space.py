#!/usr/bin/env python3
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
PROJECT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("maintenance", PROJECT / "scripts/reclaim-space.py")
M = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = M
SPEC.loader.exec_module(M)
BINARY = Path(os.environ.get("WTRM_TEST_BINARY", os.environ.get("WTRM", PROJECT / ".artifacts/target/debug/wtrm"))).resolve()
TEST_ROOT = PROJECT / ".artifacts/maintenance-tests"
TEST_ROOT.mkdir(parents=True, exist_ok=True)


class MaintenanceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(dir=TEST_ROOT)
        self.base = Path(self.temporary.name)
        self.mount = self.base / "volume"
        self.root = self.mount / "project"
        self.root.mkdir(parents=True)
        self.repo = self.root / "demo"
        self.linked = self.repo / ".worktrees/topic"
        self.now = time.time() + 8 * 86400
        self.env = patch.dict(os.environ, {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"})
        self.env.start()
        self.git(self.root, "init", "-b", "main", str(self.repo))
        (self.repo / ".gitignore").write_text(".worktrees/\n.turbo/\nnode_modules/\n.planning/\n")
        (self.repo / "source.txt").write_text("source\n")
        self.git(self.repo, "add", ".")
        self.git(self.repo, "commit", "-m", "initial")
        self.git(self.repo, "remote", "add", "origin", "https://github.com/test-owner/demo.git")
        self.git(self.repo, "worktree", "add", "-b", "topic", str(self.linked))
        self.database = self.base / "state.sqlite"
        with sqlite3.connect(self.database) as db:
            db.execute("CREATE TABLE threads (cwd TEXT,is_pinned INTEGER,archived INTEGER,updated_at INTEGER,recency_at INTEGER)")
        self.config = M.Config(self.root, self.mount, "TEST-UUID", BINARY,
                               self.database, self.base / "reports")
        self.original_command = M.command
        self.open_paths = []
        self.lsof_bad = False
        self.uuid = "TEST-UUID"
        self.mount_point = str(self.mount)
        self.writable = True
        self.pr_state = "MERGED"
        self.command_patch = patch.object(M, "command", side_effect=self.fake_command)
        self.command_patch.start()

    def tearDown(self):
        self.command_patch.stop()
        self.env.stop()
        self.temporary.cleanup()

    def git(self, cwd, *args):
        return subprocess.run(["git", "-C", str(cwd), "-c", "user.name=fixture",
                               "-c", "user.email=fixture@example.invalid", *args],
                              check=True, capture_output=True).stdout.decode().strip()

    def fake_command(self, args, check=True):
        if args[0] == "diskutil":
            fields = {"VolumeUUID": self.uuid, "Writable": self.writable, "Internal": False}
            if self.mount_point is not None:
                fields["MountPoint"] = self.mount_point
            data = plistlib.dumps(fields)
            return subprocess.CompletedProcess(args, 0, data, b"")
        if args[0] == "lsof":
            if self.lsof_bad:
                raise M.Unsafe("lsof failed")
            data = b"p123\0f1\0n/irrelevant-observer-file\0\n"
            for path in self.open_paths:
                info = path.stat()
                data += ("f2\0n%s\0D0x%x\0i%d\0\n" %
                         (path, info.st_dev, info.st_ino)).encode()
            return subprocess.CompletedProcess(args, 0, data, b"")
        if args[0] == "gh":
            row = {"state": self.pr_state, "mergedAt": "2026-01-01T00:00:00Z",
                   "headRefOid": self.git(self.linked, "rev-parse", "HEAD"), "headRefName": "topic",
                   "headRepository": {"name": "demo"}, "headRepositoryOwner": {"login": "test-owner"}}
            return subprocess.CompletedProcess(args, 0, json.dumps([row]).encode(), b"")
        return self.original_command(args, check)

    def run_maintenance(self, apply=False):
        return M.run(self.config, apply=apply, now=self.now)

    def target_result(self, result):
        return next(row for row in result["worktrees"] if row.get("path") == str(self.linked))

    def cache_group(self, key, age=2 * 86400, repo=None):
        directory = (self.repo if repo is None else repo) / ".turbo/cache"
        directory.mkdir(parents=True, exist_ok=True)
        paths = [directory / (key + suffix) for suffix in (".tar.zst", "-meta.json", "-manifest.json")]
        for path in paths:
            path.write_bytes(bytes(range(256)) * 32)
            os.utime(path, (self.now - age, self.now - age))
        return paths

    def test_dry_run_and_delete_keep_source_branch(self):
        result = self.run_maintenance()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual(self.target_result(result)["action"], "would_remove", result)
        self.assertTrue(self.linked.exists())
        result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "removed", result)
        self.assertFalse(self.linked.exists())
        self.assertTrue(self.git(self.repo, "show-ref", "--verify", "refs/heads/topic"))
        self.assertEqual((self.repo / "source.txt").read_text(), "source\n")

    def test_expected_head_refuses_changed_commit_and_invalid_options(self):
        rows = json.loads(self.original_command([str(BINARY), "--json", str(self.root)]).stdout)["worktrees"]
        row = next(row for row in rows if row["path"] == str(self.linked))
        self.git(self.linked, "commit", "--allow-empty", "-m", "unpublished")
        args = [str(BINARY), "delete", "--id", row["id"], "--expected-head", row["head"], "--yes", "--json", str(self.root)]
        result = self.original_command(args, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.linked.exists())
        self.assertIn(b"changed", result.stdout)
        for extra in (["--force"], ["--id", row["id"]]):
            self.assertNotEqual(self.original_command(args + extra, check=False).returncode, 0)

    def test_untracked_and_unpushed_content_are_preserved(self):
        (self.linked / "untracked.txt").write_text("keep\n")
        result = self.run_maintenance(True)
        self.assertIn("uncommitted", self.target_result(result)["reason"])
        (self.linked / "untracked.txt").unlink()
        old = self.git(self.linked, "rev-parse", "HEAD")
        self.git(self.repo, "update-ref", "refs/remotes/origin/topic", old)
        self.git(self.linked, "branch", "--set-upstream-to=origin/topic")
        self.git(self.linked, "commit", "--allow-empty", "-m", "ahead")
        result = self.run_maintenance(True)
        self.assertIn("unpushed", self.target_result(result)["reason"])
        self.assertTrue(self.linked.exists())

    def ignore(self, pattern):
        with (self.repo / ".git/info/exclude").open("a") as handle:
            handle.write("\n" + pattern + "\n")

    def test_ignored_local_file_is_preserved_in_dry_run_and_apply(self):
        self.ignore(".env.local")
        local = self.linked / ".env.local"
        content = b"UNIQUE_LOCAL_SETTING=fixture-only\n"
        local.write_bytes(content)
        for apply in (False, True):
            with self.subTest(apply=apply):
                result = self.run_maintenance(apply)
                item = self.target_result(result)
                self.assertEqual(item["action"], "skip", item)
                self.assertIn("ignored local content: .env.local", item["reason"])
                self.assertEqual(local.read_bytes(), content)

    def test_unknown_ignored_directory_is_preserved(self):
        self.ignore("private-data/")
        directory = self.linked / "private-data"
        directory.mkdir()
        local = directory / "notes.db"
        content = b"unique local data\x00"
        local.write_bytes(content)
        for apply in (False, True):
            with self.subTest(apply=apply):
                result = self.run_maintenance(apply)
                item = self.target_result(result)
                self.assertEqual(item["action"], "skip", item)
                self.assertIn("private-data/", item["reason"])
                self.assertEqual(local.read_bytes(), content)

    def test_allowed_ignored_cache_does_not_block_worktree_removal(self):
        cache = self.linked / "node_modules/package/cache.bin"
        cache.parent.mkdir(parents=True)
        cache.write_bytes(b"rebuildable cache")
        result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "removed", result)
        self.assertFalse(self.linked.exists())
        self.assertTrue(self.git(self.repo, "show-ref", "--verify", "refs/heads/topic"))

    def test_ignored_local_file_created_after_initial_check_is_preserved(self):
        self.ignore(".env.local")
        local = self.linked / ".env.local"
        content = b"created during observation\n"
        capture = M.Activity.capture

        def create_local(config):
            activity = capture(config)
            local.write_bytes(content)
            return activity

        with patch.object(M.Activity, "capture", side_effect=create_local):
            result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "skip", result)
        self.assertIn("ignored local content", self.target_result(result)["reason"])
        self.assertEqual(local.read_bytes(), content)

    def test_dirty_submodule_is_preserved(self):
        source = self.base / "dependency"
        source.mkdir()
        self.git(source, "init", "-b", "main")
        (source / "file").write_text("initial")
        self.git(source, "add", ".")
        self.git(source, "commit", "-m", "dependency")
        self.git(self.linked, "-c", "protocol.file.allow=always", "submodule", "add", str(source), "dependency")
        self.git(self.linked, "commit", "-am", "submodule")
        (self.linked / "dependency/file").write_text("changed")
        result = self.run_maintenance(True)
        self.assertIn("uncommitted", self.target_result(result)["reason"])
        self.assertTrue(self.linked.exists())

    def test_nested_checkout_and_planning_are_preserved(self):
        nested = self.linked / "node_modules/nested"
        nested.mkdir(parents=True)
        self.git(nested, "init", "-b", "main")
        result = self.run_maintenance(True)
        self.assertIn("nested", self.target_result(result)["reason"])
        for path in sorted(nested.rglob("*"), reverse=True):
            if path.is_file():
                path.unlink()
            elif path.is_dir():
                path.rmdir()
        nested.rmdir()
        planning = self.linked / ".planning"
        planning.mkdir()
        result = self.run_maintenance(True)
        self.assertIn("planning", self.target_result(result)["reason"])

    def test_pinned_session_and_open_worktree_are_preserved(self):
        with sqlite3.connect(self.database) as db:
            db.execute("INSERT INTO threads VALUES (?,?,?,?,?)", (str(self.linked), 1, 1, 0, 0))
        result = self.run_maintenance(True)
        self.assertIn("session", self.target_result(result)["reason"])
        with sqlite3.connect(self.database) as db:
            db.execute("DELETE FROM threads")
        self.open_paths = [self.linked / "source.txt"]
        result = self.run_maintenance(True)
        self.assertIn("in use", self.target_result(result)["reason"])

    def test_observation_and_uuid_fail_closed(self):
        self.lsof_bad = True
        result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "skip")
        self.assertTrue(self.linked.exists())
        self.uuid = "wrong-volume"
        result = self.run_maintenance(True)
        self.assertEqual(result["status"], "error")
        self.assertTrue(self.linked.exists())

    def test_unmounted_and_read_only_volumes_are_rejected(self):
        for mount_point, writable in ((None, True), (str(self.mount) + "-other", True),
                                      (str(self.mount), False), (str(self.mount), "true")):
            with self.subTest(mount_point=mount_point, writable=writable):
                self.mount_point, self.writable = mount_point, writable
                result = self.run_maintenance(True)
                self.assertEqual(result["status"], "error", result)
                self.assertTrue(self.linked.exists())
                self.assertEqual((self.linked / "source.txt").read_text(), "source\n")

    def test_unrelated_session_does_not_block_worktrees(self):
        with sqlite3.connect(self.database) as db:
            db.execute("INSERT INTO threads VALUES (?,?,?,?,?)", (str(self.base / "home"), 0, 0, self.now, self.now))
        result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "removed")
        self.assertFalse(self.linked.exists())

    def test_unknown_session_schema_preserves_worktrees(self):
        with sqlite3.connect(self.database) as db:
            db.execute("DROP TABLE threads")
            db.execute("CREATE TABLE threads (cwd TEXT)")
        result = self.run_maintenance(True)
        self.assertEqual(result["worktrees"][0]["action"], "skip_all")
        self.assertTrue(self.linked.exists())

    def test_installer_writes_unloaded_agent_and_private_binary(self):
        home = self.base / "home"
        home.mkdir()
        commands = self.base / "commands"
        commands.mkdir()
        diskutil = commands / "diskutil"
        payload = {"VolumeUUID": "TEST-UUID", "MountPoint": str(self.mount),
                   "Writable": True, "Internal": False}
        diskutil.write_text("#!" + sys.executable + "\nimport plistlib,sys\nsys.stdout.buffer.write(plistlib.dumps(" + repr(payload) + "))\n")
        diskutil.chmod(0o700)
        install = home / "private-maintenance"
        args = [sys.executable, str(PROJECT / "scripts/install-reclaim-space.py"),
                "--wtrm", str(BINARY), "--root", str(self.root), "--mount", str(self.mount),
                "--volume-uuid", "TEST-UUID", "--report-dir", str(self.config.report_dir),
                "--codex-db", str(self.database), "--install-dir", str(install)]
        env = dict(os.environ, HOME=str(home), PATH=str(commands) + os.pathsep + os.environ["PATH"])
        result = subprocess.run(args, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        installed = json.loads(result.stdout)
        self.assertFalse(installed["loaded"])
        self.assertTrue((install / "wtrm").is_file())
        self.assertFalse((home / ".cargo/bin/wtrm").exists())
        agent = plistlib.loads(Path(installed["launch_agent"]).read_bytes())
        self.assertFalse(agent["RunAtLoad"])
        self.assertEqual(agent["StartInterval"], 3600)
        native_entry = [str(install / "wtrm"), "maintain", "--config", str(install / "config.json")]
        self.assertEqual(installed["dry_run"], native_entry)
        self.assertEqual(agent["ProgramArguments"], native_entry + ["--apply"])

    def test_installer_rejects_old_binary_before_writing_files(self):
        home = self.base / "old-binary-home"
        home.mkdir()
        commands = self.base / "old-binary-commands"
        commands.mkdir()
        old = commands / "old-wtrm"
        old.write_text(
            "#!" + sys.executable + "\nimport sys\n"
            "args=sys.argv[1:]\n"
            "if args == ['delete','--help']: print('--expected-head')\n"
            "elif args == ['maintain','--help']: print('top-level help')\n"
            "else: raise SystemExit(2)\n")
        old.chmod(0o700)
        diskutil = commands / "diskutil"
        payload = {"VolumeUUID": "TEST-UUID", "MountPoint": str(self.mount),
                   "Writable": True, "Internal": False}
        diskutil.write_text("#!" + sys.executable + "\nimport plistlib,sys\nsys.stdout.buffer.write(plistlib.dumps(" + repr(payload) + "))\n")
        diskutil.chmod(0o700)
        install = home / "private-maintenance"
        args = [sys.executable, str(PROJECT / "scripts/install-reclaim-space.py"),
                "--wtrm", str(old), "--root", str(self.root), "--mount", str(self.mount),
                "--volume-uuid", "TEST-UUID", "--report-dir", str(self.config.report_dir),
                "--codex-db", str(self.database), "--install-dir", str(install)]
        env = dict(os.environ, HOME=str(home), PATH=str(commands) + os.pathsep + os.environ["PATH"])
        result = subprocess.run(args, env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(install.exists())
        self.assertFalse(self.config.report_dir.exists())
        self.assertFalse((home / "Library/LaunchAgents/com.wtrm.maintenance.plist").exists())

    def test_native_maintenance_entry_waits_and_forwards_only_fixed_arguments(self):
        private = self.base / "native entry"
        private.mkdir()
        binary = private / "wtrm"
        shutil.copy2(BINARY, binary)
        config = private / "config with spaces.json"
        (private / "reclaim-space.py").write_text(
            "import json,os,sys\nfrom pathlib import Path\n"
            "Path(sys.argv[2]).write_text(json.dumps({'args':sys.argv[1:],'parent':os.getppid(),'isolated':sys.flags.isolated}))\n"
            "raise SystemExit(7 if '--apply' in sys.argv else 0)\n")
        args = [str(binary), "maintain", "--config", str(config)]
        process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        _, error = process.communicate()
        self.assertEqual(process.returncode, 0, error)
        observed = json.loads(config.read_text())
        self.assertEqual(observed["args"], ["--config", str(config)])
        self.assertEqual(observed["parent"], process.pid)
        self.assertEqual(observed["isolated"], 1)
        result = subprocess.run(args + ["--apply"], capture_output=True)
        self.assertEqual(result.returncode, 7)
        self.assertEqual(json.loads(config.read_text())["args"], ["--config", str(config), "--apply"])
        config.unlink()
        result = subprocess.run(args + ["--script", str(PROJECT / "scripts/reclaim-space.py")], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(config.exists())
        (private / "reclaim-space.py").unlink()
        result = subprocess.run(args, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"missing next to wtrm", result.stderr)
        self.assertFalse(config.exists())
        outside = self.base / "outside-script.py"
        outside.write_text("raise SystemExit('must not execute')\n")
        (private / "reclaim-space.py").symlink_to(outside)
        result = subprocess.run(args, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"regular file next to wtrm", result.stderr)
        (private / "reclaim-space.py").unlink()
        (private / "reclaim-space.py").write_text("raise SystemExit(0)\n")
        private.chmod(0o777)
        try:
            result = subprocess.run(args, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"must not be writable by other users", result.stderr)
        finally:
            private.chmod(0o700)

    def test_cache_groups_dry_run_and_main_only_repository(self):
        self.git(self.repo, "worktree", "remove", str(self.linked))
        older = self.cache_group("a" * 16, 3 * 86400)
        newer = self.cache_group("b" * 16, 2 * 86400)
        with patch.object(M, "CACHE_LIMIT", 30000):
            result = self.run_maintenance()
            self.assertGreater(result["caches"][0]["would_remove_bytes"], 0, result)
            self.assertTrue(all(path.exists() for path in older + newer))
            result = self.run_maintenance(True)
        self.assertTrue(all(not path.exists() for path in older))
        self.assertTrue(all(path.exists() for path in newer))
        self.assertEqual(result["caches"][0]["protected_over_limit"], 0)

    def test_nested_main_only_repository_cache_is_reclaimed(self):
        nested = self.root / "group/nested"
        nested.parent.mkdir()
        self.git(self.root, "init", "-b", "main", str(nested))
        older = self.cache_group("a" * 16, 3 * 86400, nested)
        newer = self.cache_group("b" * 16, 2 * 86400, nested)
        with patch.object(M, "CACHE_LIMIT", 30000):
            result = self.run_maintenance(True)
        cache = next(row for row in result["caches"] if row["repo"] == str(nested))
        self.assertGreater(cache["removed_bytes"], 0)
        self.assertTrue(all(not path.exists() for path in older))
        self.assertTrue(all(path.exists() for path in newer))

    def test_scanned_nested_repository_and_linked_cache_are_discovered(self):
        nested = self.repo / "embedded"
        self.git(self.repo, "init", "-b", "main", str(nested))
        self.git(nested, "commit", "--allow-empty", "-m", "nested")
        linked = nested / ".worktrees/worker"
        self.git(nested, "worktree", "add", "-b", "worker", str(linked))
        self.cache_group("a" * 16, repo=nested)
        self.cache_group("b" * 16, repo=linked)
        rows = json.loads(self.original_command([str(BINARY), "--json", str(nested)]).stdout)["worktrees"]
        self.config.validate_volume()
        found = M.repository_caches(self.config, rows)
        self.assertEqual(set(found[nested]), {nested / ".turbo/cache", linked / ".turbo/cache"})

    def test_scan_root_itself_can_be_a_main_only_repository(self):
        self.git(self.repo, "worktree", "remove", str(self.linked))
        self.cache_group("a" * 16)
        self.config.root = self.repo
        self.config.validate_volume()
        self.assertEqual(M.repository_caches(self.config, []), {self.repo: [self.repo / ".turbo/cache"]})

    def test_recent_open_and_linked_cache_members_protect_groups(self):
        old = self.cache_group("a" * 16)
        recent = self.cache_group("b" * 16)
        os.utime(recent[1], (self.now - 10, self.now - 10))
        linked = self.cache_group("c" * 16)
        os.link(linked[0], self.base / "shared-cache")
        self.open_paths = [old[1]]
        with patch.object(M, "CACHE_LIMIT", 1):
            result = self.run_maintenance(True)
        self.assertTrue(all(path.exists() for path in old + recent + linked))
        self.assertGreater(result["caches"][0]["protected_over_limit"], 0)

    def test_cache_identity_change_and_symlink_do_not_escape(self):
        paths = self.cache_group("a" * 16)
        outside = self.base / "outside"
        outside.mkdir()
        (outside / "keep").write_text("keep")
        (outside / "cache").mkdir()
        outside_archive = outside / "cache" / ("d" * 16 + ".tar.zst")
        outside_archive.write_bytes(b"x" * 8192)
        os.utime(outside_archive, (self.now - 2 * 86400, self.now - 2 * 86400))
        other = self.root / "other"
        other.mkdir()
        self.git(other, "init", "-b", "main")
        (other / ".turbo").symlink_to(outside, target_is_directory=True)
        capture = M.Activity.capture

        def changed(config):
            activity = capture(config)
            paths[0].unlink()
            paths[0].write_text("new inode")
            return activity

        with patch.object(M, "CACHE_LIMIT", 1), patch.object(M.Activity, "capture", side_effect=changed):
            result = self.run_maintenance(True)
        self.assertTrue(all(path.exists() for path in paths))
        self.assertEqual((outside / "keep").read_text(), "keep")
        self.assertTrue(outside_archive.exists())
        self.assertTrue(any(item.get("error") for item in result["caches"]))

    def test_partial_cache_removal_is_reported(self):
        paths = sorted(self.cache_group("a" * 16))
        first_size = paths[0].stat().st_blocks * 512
        original_unlink = os.unlink
        changed = False

        def change_second(path, **kwargs):
            nonlocal changed
            original_unlink(path, **kwargs)
            if not changed and path == paths[0].name and "dir_fd" in kwargs:
                changed = True
                paths[1].write_text("changed after first unlink")

        with patch.object(M, "CACHE_LIMIT", 1), patch.object(M.os, "unlink", side_effect=change_second):
            result = self.run_maintenance(True)
        cache = next(row for row in result["caches"] if row["repo"] == str(self.repo))
        self.assertEqual(cache["removed_bytes"], first_size)
        self.assertIsNone(cache["remaining_bytes"])
        self.assertEqual(cache["entries"][0]["action"], "partial")
        self.assertEqual(cache["entries"][0]["removed_paths"], [str(paths[0])])
        self.assertFalse(paths[0].exists())
        self.assertTrue(paths[1].exists())
        self.assertTrue(paths[2].exists())

    def test_lock_and_report_rotation(self):
        M.prepare_directory(self.config.report_dir)
        config_path = self.base / "config.json"
        config_path.write_text(json.dumps({key: str(getattr(self.config, key)) for key in
                                           ("root", "mount", "volume_uuid", "wtrm", "codex_db", "report_dir")}))
        with open(self.config.wtrm.parent / "maintenance.lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = subprocess.run([sys.executable, str(PROJECT / "scripts/reclaim-space.py"),
                                     "--config", str(config_path)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["reason"], "another run holds the lock")
        for count in range(23):
            M.save_report(self.config.report_dir, {"count": count})
        reports = sorted(self.config.report_dir.glob("run-*.json"))
        self.assertEqual(len(reports), 20)
        self.assertEqual(json.loads(reports[-1].read_text())["count"], 22)


if __name__ == "__main__":
    if not BINARY.is_file():
        raise SystemExit("Build wtrm in .artifacts/target or set WTRM before these tests")
    unittest.main(verbosity=2)
