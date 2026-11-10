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
        self.cwd_paths = []
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

    def fake_command(self, args, check=True, timeout=60):
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
            for fd, paths in (("cwd", self.cwd_paths), ("2", self.open_paths)):
                for path in paths:
                    info = path.stat()
                    data += ("f%s\0n%s\0D0x%x\0i%d\0\n" %
                             (fd, path, info.st_dev, info.st_ino)).encode()
            return subprocess.CompletedProcess(args, 0, data, b"")
        if args[0] == "gh":
            row = {"state": self.pr_state, "mergedAt": "2026-01-01T00:00:00Z",
                   "headRefOid": self.git(self.linked, "rev-parse", "HEAD"), "headRefName": "topic",
                   "headRepository": {"name": "demo"}, "headRepositoryOwner": {"login": "test-owner"}}
            return subprocess.CompletedProcess(args, 0, json.dumps([row]).encode(), b"")
        return self.original_command(args, check=check, timeout=timeout)

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

    def test_scan_can_complete_after_the_default_command_deadline(self):
        original_run = subprocess.run
        scan_args = [str(self.config.wtrm), "--json", str(self.root)]

        def delayed_scan(args, **kwargs):
            if args == scan_args and kwargs["timeout"] < 61:
                raise subprocess.TimeoutExpired(args, kwargs["timeout"])
            return original_run(args, **kwargs)

        with patch.object(M.subprocess, "run", side_effect=delayed_scan):
            result = self.run_maintenance()
        self.assertEqual(result["status"], "complete", result)
        self.assertEqual(self.target_result(result)["action"], "would_remove", result)
        self.assertTrue(self.linked.exists())

    def test_scan_timeout_stops_maintenance_before_reclamation(self):
        scan_args = [str(self.config.wtrm), "--json", str(self.root)]

        def delayed_scan(args, **kwargs):
            self.assertEqual(args, scan_args)
            if kwargs["timeout"] < 301:
                raise subprocess.TimeoutExpired(args, kwargs["timeout"], output=b"partial scan")
            self.fail("scan exceeded its deadline")

        with patch.object(M.subprocess, "run", side_effect=delayed_scan), \
                patch.object(M, "reclaim_worktrees") as reclaim, \
                patch.object(M, "repository_caches") as caches:
            result = self.run_maintenance(True)
        self.assertEqual(result["status"], "error", result)
        self.assertEqual(result["errors"], ["command timed out after 300s: " + " ".join(scan_args)])
        self.assertEqual(result["worktrees"], [])
        self.assertEqual(result["caches"], [])
        reclaim.assert_not_called()
        caches.assert_not_called()
        self.assertTrue(self.linked.exists())
        self.assertEqual((self.linked / "source.txt").read_text(), "source\n")

    def test_delete_rescan_can_complete_after_the_scan_deadline(self):
        original_run = subprocess.run
        for elapsed in (61, 301):
            with self.subTest(elapsed=elapsed):
                delete_calls = []

                def delayed_delete(args, **kwargs):
                    if args[:2] == [str(self.config.wtrm), "delete"]:
                        delete_calls.append(args)
                        if kwargs["timeout"] < elapsed:
                            raise subprocess.TimeoutExpired(args, kwargs["timeout"])
                    return original_run(args, **kwargs)

                with patch.object(M.subprocess, "run", side_effect=delayed_delete):
                    result = self.run_maintenance()
                self.assertEqual(result["status"], "complete", result)
                self.assertEqual(self.target_result(result)["action"], "would_remove", result)
                self.assertEqual(len(delete_calls), 1)
                self.assertIn("--dry-run", delete_calls[0])
                self.assertTrue(self.linked.exists())

    def test_delete_timeout_keeps_the_worktree_and_reports_the_error(self):
        original_run = subprocess.run
        delete_calls = []

        def delayed_delete(args, **kwargs):
            if args[:2] == [str(self.config.wtrm), "delete"]:
                delete_calls.append(args)
                self.assertIn("--yes", args)
                if kwargs["timeout"] < 601:
                    raise subprocess.TimeoutExpired(args, kwargs["timeout"], output=b"partial delete")
                self.fail("delete exceeded its deadline")
            return original_run(args, **kwargs)

        with patch.object(M.subprocess, "run", side_effect=delayed_delete):
            result = self.run_maintenance(True)
        item = self.target_result(result)
        self.assertEqual(len(delete_calls), 1)
        self.assertEqual(item["action"], "skip", result)
        self.assertEqual(item["reason"], "command timed out after 600s: " + " ".join(delete_calls[0]))
        self.assertFalse(any(row["action"] == "removed" for row in result["worktrees"]))
        self.assertTrue(self.linked.exists())
        self.assertEqual((self.linked / "source.txt").read_text(), "source\n")

    def test_other_commands_keep_the_default_deadline(self):
        args = ["git", "-C", str(self.repo), "status"]

        def delayed_command(args, **kwargs):
            if kwargs["timeout"] < 61:
                raise subprocess.TimeoutExpired(args, kwargs["timeout"])
            self.fail("ordinary command exceeded its deadline")

        with patch.object(M.subprocess, "run", side_effect=delayed_command):
            with self.assertRaises(M.Unsafe) as raised:
                self.original_command(args, check=False)
        self.assertEqual(str(raised.exception), "command timed out after 60s: " + " ".join(args))

    def test_short_subprocess_deadline_and_unavailable_command_are_distinct(self):
        args = [sys.executable, "-c", "import time; time.sleep(1)"]
        with self.assertRaises(M.Unsafe) as raised:
            self.original_command(args, timeout=0.01)
        self.assertEqual(str(raised.exception), "command timed out after 0.01s: " + " ".join(args))
        self.assertIsInstance(raised.exception.__cause__, subprocess.TimeoutExpired)
        with patch.object(M.subprocess, "run", side_effect=FileNotFoundError("missing")):
            with self.assertRaises(M.Unsafe) as raised:
                self.original_command(["missing-command"])
        self.assertEqual(str(raised.exception), "command unavailable: missing-command")
        self.assertIsInstance(raised.exception.__cause__, OSError)

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

    def test_discard_stale_removes_local_content_without_a_merged_pr(self):
        self.config.discard_stale = True
        self.config.reclaim_cache = False
        old = self.git(self.linked, "rev-parse", "HEAD")
        self.git(self.repo, "update-ref", "refs/remotes/origin/topic", old)
        self.git(self.linked, "branch", "--set-upstream-to=origin/topic")
        self.git(self.linked, "commit", "--allow-empty", "-m", "unpushed")
        self.git(self.repo, "remote", "remove", "origin")
        self.ignore(".env.local")
        files = {"source.txt": "discard tracked edit\n", "untracked.txt": "discard untracked\n",
                 ".env.local": "discard ignored fixture\n", "planning/notes.md": "discard notes\n",
                 ".planning/notes.md": "discard ignored notes\n"}
        for name, content in files.items():
            (self.linked / name).parent.mkdir(parents=True, exist_ok=True)
            (self.linked / name).write_text(content)
        with patch.object(M, "merged_pr", side_effect=AssertionError("GitHub is not required")):
            result = self.run_maintenance()
            self.assertEqual(self.target_result(result)["action"], "would_remove", result)
            self.assertTrue(result["worktree_policy"]["discard_stale"])
            for name, content in files.items():
                self.assertEqual((self.linked / name).read_text(), content)
            result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "removed", result)
        self.assertIn("content loss", self.target_result(result)["reason"])
        self.assertFalse(self.linked.exists())
        self.assertTrue(self.git(self.repo, "show-ref", "--verify", "refs/heads/topic"))
        self.assertEqual((self.repo / "source.txt").read_text(), "source\n")

    def test_discard_stale_keeps_age_and_safety_guards(self):
        self.config.discard_stale = True
        self.config.reclaim_cache = False
        (self.linked / "source.txt").write_text("discardable edit\n")
        self.config.created_before = None
        self.config.inactive = 30 * 86400
        result = self.run_maintenance(True)
        self.assertIn("no enabled age threshold", self.target_result(result)["reason"])
        self.config.inactive = 7 * 86400
        self.git(self.repo, "worktree", "lock", str(self.linked))
        result = self.run_maintenance(True)
        self.assertIn("locked", self.target_result(result)["reason"])
        self.git(self.repo, "worktree", "unlock", str(self.linked))
        self.open_paths = [self.linked / "source.txt"]
        result = self.run_maintenance(True)
        self.assertIn("in use", self.target_result(result)["reason"])
        self.open_paths = []
        nested = self.linked / ".planning/nested"
        nested.mkdir(parents=True)
        self.git(nested, "init", "-b", "main")
        result = self.run_maintenance(True)
        self.assertIn("nested", self.target_result(result)["reason"])
        self.assertTrue(self.linked.exists())

    def test_discard_stale_skips_session_metadata(self):
        self.config.created_before = None
        self.config.inactive = 5 * 86400
        recent = self.now - 6 * 86400
        for cwd in (self.root, self.linked):
            for pinned, archived, stamp in ((0, 0, recent), (1, 1, 0)):
                with self.subTest(cwd=cwd, pinned=pinned):
                    with sqlite3.connect(self.database) as db:
                        db.execute("DELETE FROM threads")
                        db.execute("INSERT INTO threads VALUES (?,?,?,?,?)",
                                   (str(cwd), pinned, archived, stamp, stamp))
                    self.config.discard_stale = False
                    result = self.run_maintenance()
                    self.assertIn("session", self.target_result(result)["reason"])
                    self.config.discard_stale = True
                    with patch.object(M.sqlite3, "connect", side_effect=AssertionError("session metadata is not required")):
                        result = self.run_maintenance()
                    self.assertEqual(self.target_result(result)["action"], "would_remove", result)
                    self.assertEqual(result["activity_scope"],
                                     "live process cwd and open files; Codex session metadata is not checked")
                    self.assertTrue(self.linked.exists())
        result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "removed", result)
        self.assertFalse(self.linked.exists())
        self.assertTrue(self.git(self.repo, "show-ref", "--verify", "refs/heads/topic"))

    def test_discard_stale_does_not_require_session_database(self):
        self.database.write_bytes(b"not a SQLite database")
        for missing in (False, True):
            with self.subTest(missing=missing):
                if missing:
                    self.database.unlink()
                self.config.discard_stale = False
                result = self.run_maintenance()
                self.assertEqual(result["worktrees"][0]["action"], "skip_all", result)
                self.config.discard_stale = True
                result = self.run_maintenance()
                self.assertEqual(self.target_result(result)["action"], "would_remove", result)
                self.assertTrue(self.linked.exists())

    def test_age_policy_matches_either_threshold_and_rejects_unknown_times(self):
        self.config.created_before = 30 * 86400
        self.config.inactive = 7 * 86400
        young, old_created, idle = self.now - 86400, self.now - 30 * 86400, self.now - 7 * 86400
        for created, active, expected in ((old_created, young, True), (young, idle, True),
                                           (young, young, False), (None, idle, True),
                                           (old_created, None, True), (None, None, False)):
            with self.subTest(created=created, active=active):
                self.assertEqual(self.config.matches_age({"created_at": created, "last_active": active},
                                                          self.now), expected)
        for stamp in (None, True, "1", 0, -1, self.now + 1, float("nan"), float("inf"), -float("inf")):
            with self.subTest(stamp=stamp):
                self.assertFalse(self.config.matches_age({"created_at": stamp, "last_active": stamp}, self.now))
        self.config.created_before = None
        self.assertFalse(self.config.matches_age({"created_at": old_created, "last_active": young}, self.now))
        self.config.inactive = None
        self.config.created_before = 30 * 86400
        self.assertFalse(self.config.matches_age({"created_at": young, "last_active": idle}, self.now))

    def test_inactive_cleanup_rescans_each_run_and_preserves_cache_by_configuration(self):
        self.config.created_before = 30 * 86400
        self.config.inactive = 7 * 86400
        self.config.reclaim_cache = False
        cached = self.cache_group("a" * 16)
        with patch.object(M, "repository_caches", side_effect=AssertionError("cache discovery disabled")):
            result = self.run_maintenance()
            self.assertEqual(self.target_result(result)["action"], "would_remove", result)
            self.assertTrue(self.linked.exists())
            (self.linked / "source.txt").write_text("new work\n")
            result = self.run_maintenance(True)
            self.assertEqual(self.target_result(result)["action"], "skip", result)
            self.assertTrue(self.linked.exists())
            self.git(self.linked, "restore", "source.txt")
            result = self.run_maintenance(True)
        self.assertEqual(self.target_result(result)["action"], "removed", result)
        self.assertEqual(result["worktree_policy"]["match"], "any")
        self.assertEqual(result["caches"], [])
        self.assertTrue(all(path.exists() for path in cached))
        self.assertTrue(self.git(self.repo, "show-ref", "--verify", "refs/heads/topic"))

    def test_maintenance_paths_and_unknown_status_are_preserved(self):
        self.config.validate_volume()
        rows = M.scan(self.config)
        for discard in (False, True):
            self.config.discard_stale = discard
            for attribute in ("wtrm", "report_dir", "config_path"):
                with self.subTest(discard_stale=discard, attribute=attribute), \
                        patch.object(self.config, attribute, self.linked / ".planning/local"):
                    result = M.reclaim_worktrees(self.config, rows, self.now, True)
                    self.assertIn("running maintenance", next(row for row in result if row["path"] == str(self.linked))["reason"])
            with patch.object(M.Path, "cwd", return_value=self.linked):
                result = self.run_maintenance(True)
            self.assertIn("cwd", self.target_result(result)["reason"])
        rows = M.scan(self.config)
        next(row for row in rows if row["path"] == str(self.linked))["dirty"] = None
        for discard in (False, True):
            self.config.discard_stale = discard
            result = M.reclaim_worktrees(self.config, rows, self.now, True)
            self.assertIn("unknown", next(row for row in result if row["path"] == str(self.linked))["reason"])
        self.assertTrue(self.linked.exists())

    def test_configuration_defaults_validation_and_optional_thresholds(self):
        path = self.base / "configuration.json"
        data = {key: str(getattr(self.config, key)) for key in
                ("root", "mount", "volume_uuid", "wtrm", "codex_db", "report_dir")}
        path.write_text(json.dumps(data))
        legacy = M.Config.read(path)
        self.assertEqual(legacy.created_before, 7 * 86400)
        self.assertIsNone(legacy.inactive)
        self.assertTrue(legacy.reclaim_cache)
        self.assertFalse(legacy.discard_stale)
        data.update(created_before=None, inactive="14d", reclaim_cache=False, discard_stale=True)
        path.write_text(json.dumps(data))
        current = M.Config.read(path)
        self.assertIsNone(current.created_before)
        self.assertEqual(current.inactive, 14 * 86400)
        self.assertFalse(current.reclaim_cache)
        self.assertTrue(current.discard_stale)
        for field, value in (("inactive", None), ("inactive", "0d"), ("inactive", "-1d"),
                             ("inactive", "9999999999999w"), ("inactive", True),
                             ("inactive", 7), ("reclaim_cache", "false"),
                             ("discard_stale", "true"), ("discard_stale", 1),
                             ("discard_stale", None), ("extra", "unknown")):
            with self.subTest(field=field, value=value):
                path.write_text(json.dumps(dict(data, **{field: value})))
                with self.assertRaises(M.Unsafe):
                    M.Config.read(path)
        for raw in ("0", "0s", "-1h", "1.5d", "1x", "9999999999999w", "", True):
            with self.subTest(raw=raw), self.assertRaises(M.Unsafe):
                M.duration(raw)
        self.assertEqual(M.duration("6h"), 21600)
        self.assertEqual(M.duration("2147483647s"), M.MAX_DURATION)

    def test_expected_head_refuses_changed_commit_and_multiple_targets(self):
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

    def test_expected_force_protects_main_locked_and_renamed_worktrees(self):
        rows = json.loads(self.original_command([str(BINARY), "--json", str(self.root)]).stdout)["worktrees"]
        row = next(row for row in rows if row["path"] == str(self.linked))
        main = next(row for row in rows if row["main"])
        self.git(self.repo, "worktree", "lock", str(self.linked))
        for target, reason in ((row, b"locked"), (main, b"main checkout")):
            for mode in ("--dry-run", "--yes"):
                with self.subTest(target=target["branch"], mode=mode):
                    args = [str(BINARY), "delete", "--id", target["id"], "--expected-head", target["head"],
                            "--force", mode, "--json", str(self.root)]
                    result = self.original_command(args, check=False)
                    item = json.loads(result.stdout)["items"][0]
                    self.assertNotEqual(item["result"], "ok", item)
                    self.assertIn(reason, result.stdout)
                    self.assertTrue(self.linked.exists())
                    self.assertTrue(self.repo.exists())
        self.git(self.repo, "worktree", "unlock", str(self.linked))
        self.git(self.linked, "branch", "-m", "renamed")
        args = [str(BINARY), "delete", "--id", row["id"], "--expected-head", row["head"],
                "--force", "--yes", "--json", str(self.root)]
        result = self.original_command(args, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.linked.exists())

    def test_expected_force_preserves_registered_nested_worktree(self):
        nested = self.linked / "node_modules/worker"
        nested.parent.mkdir()
        self.git(self.repo, "worktree", "add", "-b", "worker", str(nested))
        self.git(self.repo, "worktree", "lock", str(nested))
        self.config.discard_stale = True
        result = self.run_maintenance(True)
        self.assertIn("registered nested", self.target_result(result)["reason"])
        rows = json.loads(self.original_command([str(BINARY), "--json", str(self.root)]).stdout)["worktrees"]
        row = next(row for row in rows if row["path"] == str(self.linked))
        for mode in ("--dry-run", "--yes"):
            args = [str(BINARY), "delete", "--id", row["id"], "--expected-head", row["head"],
                    "--force", mode, "--json", str(self.root)]
            result = self.original_command(args, check=False)
            self.assertIn(b"registered nested", result.stdout)
            self.assertTrue(nested.exists())
            self.assertTrue(self.linked.exists())

    def test_expected_force_refuses_a_lock_created_during_removal(self):
        rows = json.loads(self.original_command([str(BINARY), "--json", str(self.root)]).stdout)["worktrees"]
        row = next(row for row in rows if row["path"] == str(self.linked))
        commands = self.base / "locking-commands"
        commands.mkdir()
        real_git = shutil.which("git")
        recorded = self.base / "remove-args.json"
        wrapper = commands / "git"
        wrapper.write_text(
            "#!" + sys.executable + "\nimport json,os,subprocess,sys\nfrom pathlib import Path\n"
            "args=sys.argv[1:]\n"
            "if 'worktree' in args and args[args.index('worktree'):][:2] == ['worktree','remove']:\n"
            "    Path(" + repr(str(recorded)) + ").write_text(json.dumps(args))\n"
            "    subprocess.run(" + repr([real_git, "-C", str(self.repo), "worktree", "lock", str(self.linked)]) + ",check=True)\n"
            "os.execv(" + repr(real_git) + ",[" + repr(real_git) + ",*args])\n")
        wrapper.chmod(0o700)
        args = [str(BINARY), "delete", "--id", row["id"], "--expected-head", row["head"],
                "--force", "--yes", "--json", str(self.root)]
        with patch.dict(os.environ, {"PATH": str(commands) + os.pathsep + os.environ["PATH"]}):
            result = self.original_command(args, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"locked", result.stdout)
        self.assertEqual(json.loads(recorded.read_text()).count("--force"), 1)
        self.assertTrue(self.linked.exists())

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
        self.config.discard_stale = True
        result = self.run_maintenance(True)
        self.assertIn("nested", self.target_result(result)["reason"])
        self.git(self.linked, "-c", "protocol.file.allow=always", "submodule", "deinit", "--force", "dependency")
        for apply in (False, True):
            result = self.run_maintenance(apply)
            self.assertIn("submodules", self.target_result(result)["reason"])
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
        for name in ("planning", ".planning"):
            planning = self.linked / name
            planning.mkdir()
            result = self.run_maintenance(True)
            self.assertIn("planning", self.target_result(result)["reason"])
            self.assertTrue(planning.exists())
            planning.rmdir()

    def test_ancestor_cwds_and_shared_git_activity_do_not_block_worktree_removal(self):
        sibling = self.repo / ".worktrees/sibling"
        self.git(self.repo, "worktree", "add", "-b", "sibling", str(sibling))
        sibling_git = Path(self.git(sibling, "rev-parse", "--absolute-git-dir"))
        head = self.git(self.repo, "rev-parse", "HEAD")
        common = self.repo / ".git"
        self.cwd_paths = [self.mount, self.root, self.repo, self.linked.parent, sibling]
        self.open_paths = [common / "HEAD", common / "objects" / head[:2] / head[2:],
                           sibling_git / "HEAD", sibling / "source.txt"]
        self.config.reclaim_cache = False
        for discard in (False, True):
            with self.subTest(discard_stale=discard):
                self.config.discard_stale = discard
                result = self.run_maintenance()
                self.assertEqual(self.target_result(result)["action"], "would_remove", result)
                self.assertTrue(self.linked.exists())
                self.assertTrue(sibling.exists())

    def test_target_cwd_open_files_and_git_directory_are_preserved(self):
        nested = self.linked / "node_modules/package"
        nested.mkdir(parents=True)
        git_dir = Path(self.git(self.linked, "rev-parse", "--absolute-git-dir"))
        self.config.reclaim_cache = False
        for discard in (False, True):
            self.config.discard_stale = discard
            for fd, path in (("cwd", self.linked), ("cwd", nested), ("cwd", git_dir),
                             ("2", self.linked / "source.txt"), ("2", git_dir / "HEAD"),
                             ("2", git_dir / "index")):
                with self.subTest(discard_stale=discard, fd=fd, path=path):
                    self.cwd_paths = [path] if fd == "cwd" else []
                    self.open_paths = [path] if fd == "2" else []
                    result = self.run_maintenance(True)
                    self.assertEqual(self.target_result(result)["action"], "skip", result)
                    self.assertIn("in use", self.target_result(result)["reason"])
                    self.assertTrue(self.linked.exists())

    def test_worktree_git_directory_must_be_inside_the_expected_common_directory(self):
        self.config.discard_stale = True
        self.config.reclaim_cache = False
        git_dir = Path(self.git(self.linked, "rev-parse", "--absolute-git-dir"))
        original_git = M.git
        for invalid in (self.root, self.repo / ".git", git_dir / "logs"):
            def unexpected_git(path, *args, **kwargs):
                if path == self.linked and args == ("rev-parse", "--absolute-git-dir"):
                    return subprocess.CompletedProcess([], 0, str(invalid).encode(), b"")
                return original_git(path, *args, **kwargs)

            with self.subTest(git_dir=invalid), patch.object(M, "git", side_effect=unexpected_git):
                result = self.run_maintenance(True)
                self.assertEqual(self.target_result(result)["action"], "skip", result)
                self.assertIn("Git directory", self.target_result(result)["reason"])
                self.assertTrue(self.linked.exists())

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
                "--codex-db", str(self.database), "--install-dir", str(install),
                "--created-before", "30d", "--inactive", "7d", "--interval", "6h"]
        env = dict(os.environ, HOME=str(home), PATH=str(commands) + os.pathsep + os.environ["PATH"])
        for invalid in (["--interval", "0s"], ["--interval", "-1h"],
                        ["--interval", "9999999999999w"], ["--created-before", "0d"],
                        ["--created-before", "none", "--inactive", "none"]):
            with self.subTest(invalid=invalid):
                result = subprocess.run(args + invalid, env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(install.exists())
                self.assertFalse(self.config.report_dir.exists())
                self.assertFalse((home / "Library/LaunchAgents/com.wtrm.maintenance.plist").exists())
        result = subprocess.run(args, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        installed = json.loads(result.stdout)
        self.assertFalse(installed["loaded"])
        self.assertTrue((install / "wtrm").is_file())
        self.assertFalse((home / ".cargo/bin/wtrm").exists())
        agent = plistlib.loads(Path(installed["launch_agent"]).read_bytes())
        self.assertFalse(agent["RunAtLoad"])
        self.assertEqual(agent["StartInterval"], 21600)
        policy = json.loads((install / "config.json").read_text())
        self.assertEqual(policy["created_before"], "30d")
        self.assertEqual(policy["inactive"], "7d")
        self.assertFalse(policy["reclaim_cache"])
        self.assertFalse(policy["discard_stale"])
        self.assertFalse(installed["discard_stale"])
        native_entry = [str(install / "wtrm"), "maintain", "--config", str(install / "config.json")]
        self.assertEqual(installed["dry_run"], native_entry)
        self.assertEqual(agent["ProgramArguments"], native_entry + ["--yes"])
        self.database.unlink()
        result = subprocess.run(args, env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        result = subprocess.run(args + ["--include-cache", "--discard-stale", "--created-before", "none"],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        policy = json.loads((install / "config.json").read_text())
        self.assertIsNone(policy["created_before"])
        self.assertTrue(policy["reclaim_cache"])
        self.assertTrue(policy["discard_stale"])
        self.assertTrue(json.loads(result.stdout)["discard_stale"])

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
        for execute in ("--apply", "--yes"):
            result = subprocess.run(args + [execute], capture_output=True)
            self.assertEqual(result.returncode, 7)
            self.assertEqual(json.loads(config.read_text())["args"], ["--config", str(config), "--apply"])
        config.unlink()
        for extra in (["--inactive", "1d"], ["--created-before", "30d"], ["--all"],
                      ["--merged"], ["--max-depth", "9"], [str(self.root)]):
            with self.subTest(extra=extra):
                result = subprocess.run(args + ["--yes", *extra], capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"do not pass scan options", result.stderr)
                self.assertFalse(config.exists())
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
