#!/usr/bin/env python3
"""Install the private maintenance program and an unloaded hourly LaunchAgent."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True


SOURCE = Path(__file__).with_name("reclaim-space.py")
SPEC = importlib.util.spec_from_file_location("wtrm_maintenance", SOURCE)
MAINTENANCE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MAINTENANCE
SPEC.loader.exec_module(MAINTENANCE)
LABEL = "com.wtrm.maintenance"


def replace_file(path, data, mode):
    MAINTENANCE.prepare_directory(path.parent)
    if path.is_symlink():
        raise MAINTENANCE.Unsafe("refusing to replace a symbolic link")
    fd, temporary = tempfile.mkstemp(prefix=".install-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            os.fchmod(handle.fileno(), mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wtrm", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--mount", type=Path, required=True)
    parser.add_argument("--volume-uuid", required=True)
    parser.add_argument("--report-dir", type=Path, required=True)
    parser.add_argument("--codex-db", type=Path, default=Path.home() / ".codex/state_5.sqlite")
    parser.add_argument("--install-dir", type=Path,
                        default=Path.home() / ".local/lib/wtrm-maintenance")
    args = parser.parse_args()
    binary = MAINTENANCE.safe_path(args.wtrm)
    help_text = subprocess.run([str(binary), "delete", "--help"], check=True,
                               capture_output=True, text=True).stdout
    if "--expected-head" not in help_text:
        raise MAINTENANCE.Unsafe("wtrm binary lacks --expected-head")
    subprocess.run([str(binary), "help", "maintain"], check=True, capture_output=True)
    config = MAINTENANCE.Config(args.root, args.mount, args.volume_uuid, binary,
                                args.codex_db, args.report_dir)
    config.validate_volume()
    MAINTENANCE.safe_path(args.codex_db)
    install = MAINTENANCE.prepare_directory(args.install_dir)
    MAINTENANCE.prepare_directory(args.report_dir)
    installed_binary = install / "wtrm"
    installed_script = install / SOURCE.name
    config_path = install / "config.json"
    replace_file(installed_binary, binary.read_bytes(), 0o700)
    replace_file(installed_script, SOURCE.read_bytes(), 0o700)
    data = {"root": str(args.root), "mount": str(args.mount), "volume_uuid": args.volume_uuid,
            "wtrm": str(installed_binary), "codex_db": str(args.codex_db),
            "report_dir": str(args.report_dir)}
    replace_file(config_path, (json.dumps(data, indent=2) + "\n").encode(), 0o600)
    agent = Path.home() / "Library/LaunchAgents" / (LABEL + ".plist")
    plist = {"Label": LABEL,
             "ProgramArguments": [str(installed_binary), "maintain", "--config", str(config_path), "--apply"],
             "StartInterval": 3600, "RunAtLoad": False, "ProcessType": "Background",
             "LowPriorityIO": True, "Nice": 10,
             "EnvironmentVariables": {"PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"},
             "StandardOutPath": "/dev/null", "StandardErrorPath": "/dev/null"}
    replace_file(agent, plistlib.dumps(plist), 0o600)
    print(json.dumps({"installed": str(install), "config": str(config_path),
                      "launch_agent": str(agent), "loaded": False,
                      "dry_run": [str(installed_binary), "maintain", "--config", str(config_path)]},
                     indent=2))


if __name__ == "__main__":
    main()
