#!/usr/bin/env bash
# Keep target/ from growing without bound while incremental compilation is on.
#
# Usage:
#   scripts/clean-target.sh          remove target/debug/incremental
#   scripts/clean-target.sh --all    remove the whole target/debug
#
# Prints the size of target/ before and after. Only touches target/ inside this
# repository (or CARGO_TARGET_DIR when set). The next build is slower: the
# incremental cache (or, with --all, every debug artifact) is rebuilt.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

ALL=0
case "${1:-}" in
    "") ;;
    --all) ALL=1 ;;
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "error: unknown argument '$1' (expected --all)" >&2
        exit 2
        ;;
esac

if [ ! -d "$TARGET_DIR" ]; then
    echo "nothing to do: $TARGET_DIR does not exist"
    exit 0
fi

size() { du -sh "$1" 2> /dev/null | cut -f1; }

echo "target size before: $(size "$TARGET_DIR") ($TARGET_DIR)"
if [ "$ALL" -eq 1 ]; then
    echo "removing $TARGET_DIR/debug"
    rm -rf "$TARGET_DIR/debug"
else
    echo "removing $TARGET_DIR/debug/incremental ($(size "$TARGET_DIR/debug/incremental" || true))"
    rm -rf "$TARGET_DIR/debug/incremental"
fi
echo "target size after:  $(size "$TARGET_DIR")"
