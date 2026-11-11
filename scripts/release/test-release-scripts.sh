#!/usr/bin/env bash
# Tests for the release helper scripts (nightly version, stable gate,
# release.json, workspace version).
#
#   scripts/release/test-release-scripts.sh
#
# The nightly-version table is shared in spirit with querymt-desktop's
# scripts/release/nightly-version.mjs tests: both must produce the same output
# for the same (version, tags, timestamp) input.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1" >&2; failures=$((failures + 1)); }

expect_eq() { # name expected actual
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1: expected '$2', got '$3'"; fi
}

# --- nightly-version.sh ------------------------------------------------------
TS=20261105111147

nv() { # workspace-version tags...
  local v="$1"; shift
  printf '%s\n' "$@" | "$here/nightly-version.sh" "$v" "$TS"
}

expect_eq "nightly: unreleased workspace version" \
  "0.6.0-nightly.$TS" "$(nv 0.6.0 v0.5.1 v0.5.2)"
expect_eq "nightly: workspace version already tagged -> next patch" \
  "0.6.1-nightly.$TS" "$(nv 0.6.0 v0.5.2 v0.6.0)"
expect_eq "nightly: ls-remote output with peeled tags" \
  "0.6.1-nightly.$TS" \
  "$(printf '%s\n' $'aaaa\trefs/tags/v0.6.0' $'bbbb\trefs/tags/v0.6.0^{}' | "$here/nightly-version.sh" 0.6.0 "$TS")"
expect_eq "nightly: no tags at all" \
  "0.6.0-nightly.$TS" "$(printf '' | "$here/nightly-version.sh" 0.6.0 "$TS")"
expect_eq "nightly: v0.6.0 is not matched by v0.6.00 or v10.6.0" \
  "0.6.0-nightly.$TS" "$(nv 0.6.0 v0.6.00 v10.6.0 v0.6.0-rc1)"
expect_eq "nightly: later tag does not matter, only the workspace version" \
  "0.6.0-nightly.$TS" "$(nv 0.6.0 v0.7.0)"
expect_eq "nightly: two-digit patch bump" \
  "0.6.10-nightly.$TS" "$(nv 0.6.9 v0.6.9)"

if "$here/nightly-version.sh" 0.6 "$TS" </dev/null 2>/dev/null; then
  fail "nightly: rejects non X.Y.Z workspace version"
else pass "nightly: rejects non X.Y.Z workspace version"; fi
if "$here/nightly-version.sh" 0.6.0 2026 </dev/null 2>/dev/null; then
  fail "nightly: rejects short timestamp"
else pass "nightly: rejects short timestamp"; fi

# Later nightly sorts higher: same base, larger timestamp (numeric identifier).
a="$(nv 0.6.0)"
b="$(printf '' | "$here/nightly-version.sh" 0.6.0 20261105111148)"
sorted="$(printf '%s\n%s\n' "$b" "$a" | sort -t. -k4,4n | head -1)"
expect_eq "nightly: later timestamp sorts higher" "$a" "$sorted"

# --- workspace-version.sh ----------------------------------------------------
cat > "$tmp/Cargo.toml" <<'EOF'
[package]
version = "9.9.9"

[workspace.package]
version = "0.6.0"
edition = "2024"

[workspace.dependencies]
version = "1.2.3"
EOF
expect_eq "workspace-version: reads [workspace.package]" \
  "0.6.0" "$("$here/workspace-version.sh" "$tmp/Cargo.toml")"
expect_eq "workspace-version: matches the repo manifest" \
  "$(awk -F'"' '/^\[workspace.package\]/{f=1} f&&/^version/{print $2; exit}' "$here/../../Cargo.toml")" \
  "$("$here/workspace-version.sh" "$here/../../Cargo.toml")"

# --- write-release-json.sh ---------------------------------------------------
sha=0123456789abcdef0123456789abcdef01234567
"$here/write-release-json.sh" "0.6.0-nightly.$TS" nightly "$sha" "$tmp/release.json"
expect_eq "release.json: fields" \
  "0.6.0-nightly.$TS nightly $sha" \
  "$(jq -r '[.version,.channel,.commit]|join(" ")' "$tmp/release.json")"
expect_eq "release.json: published_at is UTC ISO-8601" "true" \
  "$(jq -r '.published_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")' "$tmp/release.json")"
if "$here/write-release-json.sh" 0.6.0 stable abc "$tmp/x.json" 2>/dev/null; then
  fail "release.json: rejects short sha"
else pass "release.json: rejects short sha"; fi
if "$here/write-release-json.sh" 0.6.0 beta "$sha" "$tmp/x.json" 2>/dev/null; then
  fail "release.json: rejects unknown channel"
else pass "release.json: rejects unknown channel"; fi

# --- stable-gate.sh ----------------------------------------------------------
# History:  old --- nightly --- head
repo="$tmp/repo"
git init -q "$repo"
git -C "$repo" config user.email t@example.com
git -C "$repo" config user.name t
git -C "$repo" config commit.gpgsign false
commit() { git -C "$repo" commit -q --allow-empty -m "$1"; git -C "$repo" rev-parse HEAD; }
old="$(commit old)"
nightly="$(commit nightly)"
head="$(commit head)"
nightly_json="$tmp/nightly.json"
"$here/write-release-json.sh" "0.6.0-nightly.$TS" nightly "$nightly" "$nightly_json"

gate() { (cd "$repo" && "$here/stable-gate.sh" "$@" >/dev/null 2>&1); }

if gate v0.6.0 0.6.0 "$old" "$nightly_json"; then
  fail "gate: v0.6.0 on an old commit fails"; else pass "gate: v0.6.0 on an old commit fails"; fi
if gate v0.6.0 0.6.0 "$head" "$nightly_json"; then
  pass "gate: v0.6.0 on a descendant passes"; else fail "gate: v0.6.0 on a descendant passes"; fi
if gate v0.6.0 0.6.0 "$nightly" "$nightly_json"; then
  pass "gate: v0.6.0 on the nightly commit passes"; else fail "gate: v0.6.0 on the nightly commit passes"; fi
if gate v0.5.3 0.5.3 "$old" "$nightly_json"; then
  pass "gate: v0.5.3 (below nightly) passes"; else fail "gate: v0.5.3 (below nightly) passes"; fi
if gate v0.6.0 0.6.1 "$head" "$nightly_json"; then
  fail "gate: tag/workspace mismatch fails"; else pass "gate: tag/workspace mismatch fails"; fi
if gate v0.7.0 0.7.0 "$old" "$nightly_json"; then
  fail "gate: v0.7.0 on an old commit fails"; else pass "gate: v0.7.0 on an old commit fails"; fi
if gate v0.6.0 0.6.0 "$old" none; then
  pass "gate: no published nightly passes"; else fail "gate: no published nightly passes"; fi
if gate v0.6.0 0.6.0 "$old" "$tmp/missing.json"; then
  fail "gate: missing release.json path fails"; else pass "gate: missing release.json path fails"; fi

if [ "$failures" -gt 0 ]; then
  echo "$failures test(s) failed" >&2
  exit 1
fi
echo "all release script tests passed"
