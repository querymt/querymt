#!/usr/bin/env bash
# Measure one edit-build cycle: apply a probe edit to a source file, run a
# command, report wall time and memory, and restore the file.
#
# Usage:
#   scripts/measure-agent-edit-loop.sh <probe-file> <shift|literal|blank> <label> <command...>
#
#   shift    insert one new line at the start of a function body (line numbers
#            of everything below move; this is the expensive case for spans).
#   literal  append the probe to the existing function line (same line count).
#   blank    insert one empty line after line PROBE_LINE (required; any line, not
#            only a function). Use it to test items such as trait definitions.
#
#   <command...> is either one quoted string (run with `bash -c`) or a plain
#   argument vector, e.g.
#     scripts/measure-agent-edit-loop.sh crates/agent-runtime/src/session/compaction.rs \
#       shift runtime-shift cargo test -p querymt-agent --features "$AGENT_FEATURES" --lib --tests --no-run
#
# Environment:
#   PROBE_LINE=<n>|last  1-based line of the `fn ... {` to probe; default is the
#                        first single-line function signature in the file.
#                        `last` picks the last one (later spans do not move).
#   ALLOW_DIRTY=1        probe a file that has uncommitted changes. The file is still
#                        restored to its content from before the probe, so the
#                        uncommitted work is kept; only use it on your own edits.
#   MEASURE_WARM=0       skip the unmodified warm-up run. By default the command
#                        runs once on the unmodified file first, so the measured
#                        build is an incremental rebuild of one probe edit against
#                        the original, not a cold build or a diff against the
#                        previous probe.
#
# Output: one line with date, rustc, label, mode, file, wall time, peak summed
# RSS over the command's process tree, largest single process, maximum process
# count and exit status. Run the cycle twice before drawing conclusions.
#
# The file must be tracked and have no uncommitted changes; it is always
# restored on exit. Needs bash, git and Python 3.9+ (standard library only).
set -euo pipefail

if [ "$#" -lt 4 ]; then
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

PROBE_FILE="$1"
MODE="$2"
LABEL="$3"
shift 3

case "$MODE" in
    shift | literal | blank) ;;
    *)
        echo "error: mode must be 'shift', 'literal' or 'blank', got '$MODE'" >&2
        exit 2
        ;;
esac

if [ ! -f "$PROBE_FILE" ]; then
    echo "error: probe file not found: $PROBE_FILE" >&2
    exit 2
fi
if [ "${ALLOW_DIRTY:-0}" != "1" ] && ! git ls-files --error-unmatch -- "$PROBE_FILE" > /dev/null 2>&1; then
    echo "error: probe file is not tracked by git: $PROBE_FILE" >&2
    exit 2
fi
if [ "${ALLOW_DIRTY:-0}" != "1" ] && { ! git diff --quiet -- "$PROBE_FILE" || ! git diff --cached --quiet -- "$PROBE_FILE"; }; then
    echo "error: probe file has uncommitted changes, refusing to edit (ALLOW_DIRTY=1 to override): $PROBE_FILE" >&2
    exit 2
fi

BACKUP="$(mktemp "${TMPDIR:-/tmp}/qmt-probe.XXXXXX")"
cp -p "$PROBE_FILE" "$BACKUP"
restore() {
    # Not `cp -p`: a fresh mtime makes cargo rebuild the original, otherwise it
    # keeps the artifacts of the probe and the next run starts from them.
    cp "$BACKUP" "$PROBE_FILE"
    touch "$PROBE_FILE"
    rm -f "$BACKUP"
}
trap restore EXIT
trap 'exit 130' INT TERM

if [ "${MEASURE_WARM:-1}" != "0" ]; then
    echo "warm-up run (unmodified file)..." >&2
    if [ "$#" -eq 1 ]; then bash -c "$1" > /dev/null 2>&1 || true; else "$@" > /dev/null 2>&1 || true; fi
fi

python3 - "$PROBE_FILE" "$MODE" "$LABEL" "${PROBE_LINE:-}" "$@" << 'PY'
import datetime
import os
import re
import signal
import subprocess
import sys
import time

path, mode, label, probe_line = sys.argv[1:5]
cmd = sys.argv[5:]
shell = len(cmd) == 1

# --- probe edit -----------------------------------------------------------
with open(path, encoding="utf-8") as fh:
    lines = fh.read().split("\n")

fn_re = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?(const\s+)?(async\s+)?(unsafe\s+)?fn\s+\w+.*\)[^;]*\{\s*$")
candidates = [i for i, l in enumerate(lines) if fn_re.match(l) and " where" not in l]
if mode == "blank":
    if not probe_line.isdigit() or not 1 <= int(probe_line) <= len(lines):
        sys.exit("error: blank mode needs PROBE_LINE=<1-based line> within the file")
    lines.insert(int(probe_line), "")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))
    idx = None
elif probe_line == "last":
    if not candidates:
        sys.exit(f"error: no single-line fn signature found in {path}")
    idx = candidates[-1]
elif probe_line:
    idx = int(probe_line) - 1
    if idx < 0 or idx >= len(lines) or not fn_re.match(lines[idx]):
        sys.exit(f"error: PROBE_LINE {probe_line} is not a single-line `fn ... {{` in {path}")
else:
    if not candidates:
        sys.exit(f"error: no single-line fn signature found in {path}")
    idx = candidates[0]

stamp = int(time.time() * 1000) % 1_000_000_000
probe = f"let _qmt_probe = {stamp}u64;"
if mode == "blank":
    pass
elif mode == "shift":
    indent = re.match(r"\s*", lines[idx]).group(0) + "    "
    lines.insert(idx + 1, indent + probe)
else:
    lines[idx] = lines[idx] + " " + probe
if mode != "blank":
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines))

# --- run and sample the process tree ----------------------------------------
def process_table():
    out = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss="], capture_output=True, text=True
    ).stdout
    table = {}
    for row in out.splitlines():
        parts = row.split()
        if len(parts) == 3:
            pid, ppid, rss = map(int, parts)
            table[pid] = (ppid, rss)
    return table

def descendants(table, root):
    children = {}
    for pid, (ppid, _) in table.items():
        children.setdefault(ppid, []).append(pid)
    found, stack = [], [root]
    while stack:
        pid = stack.pop()
        for child in children.get(pid, []):
            found.append(child)
            stack.append(child)
    return found

start = time.monotonic()
proc = subprocess.Popen(
    cmd[0] if shell else cmd,
    shell=shell,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
peak_sum = peak_one = peak_count = 0
try:
    while proc.poll() is None:
        table = process_table()
        tree = descendants(table, proc.pid)
        rss = [table[p][1] for p in tree]
        if rss:
            peak_sum = max(peak_sum, sum(rss))
            peak_one = max(peak_one, max(rss))
            peak_count = max(peak_count, len(rss))
        time.sleep(0.25)
except BaseException:
    os.killpg(proc.pid, signal.SIGTERM)
    raise
wall = time.monotonic() - start

rustc = subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip()
gb = lambda kb: kb / 1024 / 1024
print(
    f"{datetime.date.today()} | {rustc} | {label} | {mode} | {path} | "
    f"wall={wall:.1f}s peak_rss={gb(peak_sum):.1f}GB max_proc_rss={gb(peak_one):.1f}GB "
    f"max_procs={peak_count} exit={proc.returncode} | "
    f"{cmd[0] if shell else ' '.join(cmd)}"
)
sys.exit(0 if proc.returncode == 0 else 1)
PY
