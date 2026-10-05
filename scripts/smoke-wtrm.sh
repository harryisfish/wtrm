#!/usr/bin/env bash
# Smoke plus end-to-end checks against a real wtrm binary.
# Does not delete anything outside a disposable temp directory.
set -euo pipefail

WTRM="${WTRM:-$(command -v wtrm || true)}"
if [[ -z "$WTRM" ]]; then
  if [[ -x /usr/local/bin/wtrm ]]; then
    WTRM=/usr/local/bin/wtrm
  elif [[ -x target/release/wtrm ]]; then
    WTRM=target/release/wtrm
  else
    echo "wtrm binary not found" >&2
    exit 1
  fi
fi

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

pass() {
  echo "PASS: $*"
}

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

git_in() {
  local dir=$1
  shift
  git -C "$dir" -c user.email=wtrm@example.com -c user.name=wtrm "$@"
}

echo "== smoke =="
ver=$("$WTRM" --version)
[[ "$ver" == wtrm\ * ]] || fail "unexpected version: $ver"
pass "version $ver"

help=$("$WTRM" --help)
[[ "$help" == *"Scan git worktrees"* ]] || fail "help missing about text"
[[ "$help" == *"--merged"* ]] || fail "help missing --merged"
[[ "$help" == *"--inactive"* ]] || fail "help missing --inactive"
[[ "$help" == *"plan"* ]] || fail "help missing plan"
[[ "$help" == *"delete"* ]] || fail "help missing delete"
pass "help"

plan_help=$("$WTRM" plan --help)
[[ "$plan_help" == *"dry-run"* ]] || fail "plan help missing dry-run wording"
pass "plan help"

del_help=$("$WTRM" delete --help)
[[ "$del_help" == *"--yes"* ]] || fail "delete help missing --yes"
[[ "$del_help" == *"--path"* ]] || fail "delete help missing --path"
[[ "$del_help" == *"--id"* ]] || fail "delete help missing --id"
pass "delete help"

if "$WTRM" --list --inactive not-a-duration "$ROOT" >/dev/null 2>"$ROOT/err"; then
  fail "invalid duration should fail"
fi
grep -q "cannot parse --inactive" "$ROOT/err" || fail "missing parse error"
pass "invalid duration"

empty=$("$WTRM" --list "$ROOT")
[[ "$empty" == *$'\n'"no repos with linked worktrees" ]] || [[ "$empty" == *"no repos with linked worktrees" ]] || fail "empty scan: $empty"
pass "empty scan"

echo "== fixture =="
HOME_FAKE="$ROOT/home"
mkdir -p "$HOME_FAKE/project" "$HOME_FAKE/other"
REPO="$HOME_FAKE/project/demo"
LINKED="$HOME_FAKE/project/demo-feature"
IDLE="$HOME_FAKE/project/demo-idle"
git init -b main "$REPO" >/dev/null
git_in "$REPO" commit --allow-empty -m init >/dev/null
git_in "$REPO" worktree add -b feature "$LINKED" >/dev/null
git_in "$REPO" worktree add -b idle "$IDLE" >/dev/null
git_in "$IDLE" commit --allow-empty -m idle >/dev/null
# Idle worktree is unmerged after its extra commit.

list=$("$WTRM" --list "$HOME_FAKE/project")
echo "$list"
echo "$list" | grep -q $'\tfeature\t' || fail "list missing feature"
echo "$list" | grep -q $'\tmain\t' || fail "list missing main"
echo "$list" | grep -q $'\tidle\t' || fail "list missing idle"
echo "$list" | grep -q '\[clean\] \[merged\]' || fail "feature should be merged and clean: $list"
pass "list fixture"

json=$("$WTRM" --json "$HOME_FAKE/project")
python3 - "$json" <<'PY' || fail "json schema"
import json, sys
text = sys.argv[1]
data = json.loads(text)
assert data["schema_version"] == 1, data
assert data["complete"] is True, data
assert isinstance(data["scan_ms"], int), data
assert data["scan_ms"] >= 0, data
assert isinstance(data["roots"], list), data
assert isinstance(data["errors"], list), data
assert isinstance(data["items"], list), data
rows = data["worktrees"]
assert len(rows) == 3, rows
by = {r["branch"]: r for r in rows}
assert by["main"]["main"] is True
assert by["feature"]["main"] is False
assert by["feature"]["merged"] is True
assert by["feature"]["dirty"] is False
assert by["idle"]["merged"] is False
assert all("id" in r and r["id"] for r in rows), rows
print("json rows ok", sorted(by))
PY
pass "json"

echo "== bulky untracked dirty check =="
mkdir -p "$LINKED/untracked-blob"
for i in $(seq 1 800); do
  printf x >"$LINKED/untracked-blob/f$i"
done
start_ms=$(date +%s%3N)
json_dirty=$("$WTRM" --json "$HOME_FAKE/project")
end_ms=$(date +%s%3N)
python3 - "$json_dirty" "$start_ms" "$end_ms" <<'PY' || fail "bulky dirty scan"
import json, sys
data = json.loads(sys.argv[1])
wall = int(sys.argv[3]) - int(sys.argv[2])
assert data["scan_ms"] < 2500, data
assert wall < 4000, wall
feat = next(r for r in data["worktrees"] if r["branch"] == "feature")
assert feat["dirty"] is True, feat
print("bulky dirty scan_ms", data["scan_ms"], "wall_ms", wall)
PY
rm -rf "$LINKED/untracked-blob"
pass "bulky untracked dirty check"

plan=$("$WTRM" plan --json --merged "$HOME_FAKE/project")
python3 - "$plan" <<'PY' || fail "plan json"
import json, sys
data = json.loads(sys.argv[1])
assert data["schema_version"] == 1
items = data["items"]
assert items, data
assert all(i["action"] == "delete" for i in items), items
assert all(k in items[0] for k in ("action", "path", "reason", "blocked_by", "result", "id"))
feat = next(i for i in items if i["path"].endswith("demo-feature"))
assert feat["result"] == "dry-run", feat
assert feat["blocked_by"] is None, feat
print("plan ok", feat["id"])
PY
pass "plan"

FEATURE_ID=$(python3 - "$plan" <<'PY'
import json, sys
data = json.loads(sys.argv[1])
print(next(i["id"] for i in data["items"] if i["path"].endswith("demo-feature")))
PY
)
dry=$("$WTRM" delete --id "$FEATURE_ID" --dry-run --json "$HOME_FAKE/project")
python3 - "$dry" <<'PY' || fail "delete dry-run"
import json, sys
data = json.loads(sys.argv[1])
assert data["items"][0]["result"] == "dry-run", data
assert data["items"][0]["action"] == "delete"
PY
[[ -d "$LINKED" ]] || fail "dry-run deleted worktree"
pass "delete dry-run"

mkdir -p "$LINKED/node_modules/pkg"
echo x >"$LINKED/node_modules/pkg/a.js"
clean_dry=$("$WTRM" clean --path "$LINKED" --json "$HOME_FAKE/project")
python3 - "$clean_dry" <<'PY' || fail "clean dry-run"
import json, sys
data = json.loads(sys.argv[1])
item = data["items"][0]
assert item["action"] == "clean", item
assert item["result"] == "dry-run", item
assert "node_modules" in item["reason"], item
PY
[[ -d "$LINKED/node_modules" ]] || fail "clean dry-run removed node_modules"
"$WTRM" clean --path "$LINKED" --yes --json "$HOME_FAKE/project" >/dev/null
[[ ! -d "$LINKED/node_modules" ]] || fail "clean --yes left node_modules"
pass "clean"

merged=$("$WTRM" --list --merged "$HOME_FAKE/project")
echo "$merged" | grep -q $'\tfeature\t' || fail "merged filter dropped feature"
echo "$merged" | grep -q $'\tmain\t' || fail "merged filter dropped main context"
if echo "$merged" | grep -q $'\tidle\t'; then
  fail "merged filter kept unmerged idle"
fi
pass "--merged"

# --inactive 0s keeps everything idle for at least 0 seconds.
inactive=$("$WTRM" --list --inactive 0s "$HOME_FAKE/project")
echo "$inactive" | grep -q $'\tfeature\t' || fail "inactive 0s dropped feature"
pass "--inactive 0s"

created=$("$WTRM" --list --created-before 8w "$HOME_FAKE/project")
if echo "$created" | grep -q $'\tfeature\t'; then
  fail "created-before 8w should hide new worktrees: $created"
fi
echo "$created" | grep -q "no repos with linked worktrees" || fail "created-before 8w should be empty: $created"
pass "--created-before 8w hides new trees"

older=$("$WTRM" --list --older-than 0s "$HOME_FAKE/project")
echo "$older" | grep -q $'\tfeature\t' || fail "--older-than alias failed"
pass "--older-than alias"

echo "== HOME defaults and --all =="
HOME="$HOME_FAKE" "$WTRM" --list >"$ROOT/default.list"
grep -q $'\tfeature\t' "$ROOT/default.list" || fail "HOME/project default root missed fixture"
pass "default roots from HOME/project"

HOME="$HOME_FAKE" "$WTRM" --list --all --max-depth 6 >"$ROOT/all.list"
grep -q $'\tfeature\t' "$ROOT/all.list" || fail "--all missed fixture"
pass "--all"

echo "== mutate execute =="
"$WTRM" delete --id "$FEATURE_ID" --yes --json "$HOME_FAKE/project" >/dev/null
[[ ! -d "$LINKED" ]] || fail "delete --yes left worktree"
pass "delete --yes"

echo "== TUI quit =="
python3 - "$WTRM" "$HOME_FAKE/project" <<'PY' || fail "TUI did not quit cleanly"
import fcntl, os, pty, select, signal, struct, sys, termios, time
wtrm, root = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.environ["COLUMNS"] = "120"
    os.environ["LINES"] = "40"
    os.execvp(wtrm, [wtrm, root])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
os.kill(pid, signal.SIGWINCH)
deadline = time.time() + 8
buf = b""
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        if not chunk:
            break
        buf += chunk
        if b"j/k" in buf or b"help" in buf:
            os.write(fd, b"q")
            break
os.write(fd, b"q")
for _ in range(20):
    wpid, status = os.waitpid(pid, os.WNOHANG)
    if wpid:
        if os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0:
            sys.exit(0)
        sys.exit(f"TUI exit {status}")
    time.sleep(0.1)
    try:
        os.write(fd, b"q")
    except OSError:
        pass
sys.exit("TUI hung")
PY
pass "TUI quit"

echo "== TUI delete =="
python3 - "$WTRM" "$HOME_FAKE/project" "$IDLE" <<'PY' || fail "TUI delete hung or failed"
import fcntl, os, pty, re, select, signal, struct, sys, termios, time
wtrm, root, linked = sys.argv[1], sys.argv[2], sys.argv[3]
pid, fd = pty.fork()
if pid == 0:
    os.environ["COLUMNS"] = "120"
    os.environ["LINES"] = "40"
    os.execvp(wtrm, [wtrm, root])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
os.kill(pid, signal.SIGWINCH)

def plain(raw: bytes) -> bytes:
    s = re.sub(rb"\x1b\[[0-9;?=]*[A-Za-z]", b"", raw)
    return re.sub(rb"\s+", b"", s)

deadline = time.time() + 15
buf = b""
sent_d = False
sent_y = False
saw_result = False
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            chunk = os.read(fd, 8192)
        except OSError:
            break
        if not chunk:
            break
        buf += chunk
    text = plain(buf).lower()
    if not sent_d and b"j/kmove" in text:
        os.write(fd, b"d")
        sent_d = True
        continue
    if sent_d and not sent_y and (b"confirmdelete" in text or b"typeyordelete" in text):
        os.write(fd, b"y")
        sent_y = True
        continue
    if sent_y and (b"succeeded" in text or b"lastresult" in text or b"finished:" in text):
        saw_result = True
        os.write(fd, b"q")
        break
os.write(fd, b"q")
ok = False
for _ in range(20):
    wpid, status = os.waitpid(pid, os.WNOHANG)
    if wpid:
        ok = os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0
        break
    time.sleep(0.1)
    try:
        os.write(fd, b"q")
    except OSError:
        pass
visible = plain(buf)[-1200:]
if not saw_result:
    sys.exit(f"no delete result in TUI: {visible!r}")
if os.path.isdir(linked):
    sys.exit(f"TUI delete left worktree in place; ui={visible!r}")
if not ok:
    sys.exit("TUI delete did not exit cleanly")
sys.exit(0)
PY
[[ ! -d "$IDLE" ]] || fail "TUI delete left $IDLE"
pass "TUI delete"

echo "== cargo tests =="
cargo test --locked
pass "cargo test"

echo "ALL CHECKS PASSED with $WTRM"
