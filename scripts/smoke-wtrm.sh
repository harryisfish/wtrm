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
pass "help"

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
echo "$list" | grep -q 'clean,merged' || fail "feature should be merged and clean"
pass "list fixture"

json=$("$WTRM" --json "$HOME_FAKE/project")
python3 - "$json" <<'PY' || fail "json schema"
import json, sys
text = sys.argv[1]
rows = json.loads(text)
assert len(rows) == 3, rows
by = {r["branch"]: r for r in rows}
assert by["main"]["main"] is True
assert by["feature"]["main"] is False
assert by["feature"]["merged"] is True
assert by["feature"]["dirty"] is False
assert by["idle"]["merged"] is False
print("json rows ok", sorted(by))
PY
pass "json"

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

echo "== TUI quit =="
python3 - "$WTRM" "$HOME_FAKE/project" <<'PY' || fail "TUI did not quit cleanly"
import os, pty, select, sys, time
wtrm, root = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(wtrm, [wtrm, root])
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
        if b"worktrees" in buf or b"help" in buf or b"j/k" in buf:
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

echo "== cargo tests =="
cargo test --locked
pass "cargo test"

echo "ALL CHECKS PASSED with $WTRM"
