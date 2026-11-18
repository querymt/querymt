#!/usr/bin/env bash
# Sync root-level files of the gh-pages branch that mike does not manage:
#
#   _redirects  - legacy unversioned URLs -> /latest/... (Cloudflare Pages)
#   404.html    - copy of <version>/404.html. Without a root 404.html,
#                 Cloudflare Pages treats the site as an SPA and serves the
#                 root index.html (a redirect page) for every unknown path.
#
# Usage: sync-root.sh [--push] [source-version]   (default version: latest)
#
# Run after `mike deploy`, from anywhere inside the repository. Commits to the
# local gh-pages branch (like mike does) and pushes with --push.
set -euo pipefail

push=0
if [[ "${1:-}" == "--push" ]]; then
  push=1
  shift
fi
source_version="${1:-latest}"
branch="${BRANCH:-gh-pages}"
remote="${REMOTE:-origin}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"

wt="$(mktemp -d)"
cleanup() { git -C "$repo" worktree remove --force "$wt" >/dev/null 2>&1 || true; }
trap cleanup EXIT

git -C "$repo" worktree add --quiet "$wt" "$branch"

cp "$here/_redirects" "$wt/_redirects"
if [[ -f "$wt/$source_version/404.html" ]]; then
  cp "$wt/$source_version/404.html" "$wt/404.html"
else
  echo "warning: $branch:$source_version/404.html not found; leaving root 404.html untouched" >&2
fi

git -C "$wt" add -A -- _redirects $( [[ -f "$wt/404.html" ]] && echo 404.html )
if git -C "$wt" diff --cached --quiet; then
  echo "gh-pages root files already up to date"
else
  git -C "$wt" commit --quiet -m "Sync root 404.html and _redirects from $source_version"
  echo "committed gh-pages root files (from $source_version)"
fi

if [[ "$push" == 1 ]]; then
  git -C "$wt" push "$remote" "$branch"
fi
