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

# desktop_updates: the five self-updatable installers, with size and SHA-256.
dd="$tmp/desktop-assets"
mkdir -p "$dd"
desktop_files=(
  macos-aarch64.app.tar.gz macos-x86_64.app.tar.gz windows-x64-setup.exe
  linux-x86_64.AppImage linux-aarch64.AppImage
)
for f in "${desktop_files[@]}"; do printf 'installer %s\n' "$f" > "$dd/QueryMT-Desktop-nightly-$f"; done
# Files the manifest must not list.
echo dmg > "$dd/QueryMT-Desktop-nightly-macos-aarch64.dmg"
echo deb > "$dd/QueryMT-Desktop-nightly-linux-x86_64.deb"
"$here/write-release-json.sh" "0.6.0-nightly.$TS" nightly "$sha" "$tmp/release-desktop.json" \
  --desktop-dir "$dd" --version-name nightly
expect_eq "manifest lists every self-updatable platform: keys" \
  "darwin-aarch64 darwin-x86_64 linux-aarch64 linux-x86_64 windows-x86_64" \
  "$(jq -r '.desktop_updates | keys | join(" ")' "$tmp/release-desktop.json")"
expect_eq "manifest lists every self-updatable platform: files" \
  "QueryMT-Desktop-nightly-macos-aarch64.app.tar.gz QueryMT-Desktop-nightly-windows-x64-setup.exe QueryMT-Desktop-nightly-linux-x86_64.AppImage" \
  "$(jq -r '.desktop_updates | [."darwin-aarch64".file, ."windows-x86_64".file, ."linux-x86_64".file] | join(" ")' "$tmp/release-desktop.json")"
win="$dd/QueryMT-Desktop-nightly-windows-x64-setup.exe"
expect_eq "manifest lists every self-updatable platform: size and sha256" \
  "$(wc -c < "$win" | tr -d ' ') $(if command -v sha256sum >/dev/null; then sha256sum "$win"; else shasum -a 256 "$win"; fi | cut -d' ' -f1)" \
  "$(jq -r '.desktop_updates."windows-x86_64" | "\(.size) \(.sha256)"' "$tmp/release-desktop.json")"
expect_eq "manifest keeps the existing fields" \
  "$(jq -S 'del(.published_at)' "$tmp/release.json")" \
  "$(jq -S 'del(.published_at, .desktop_updates)' "$tmp/release-desktop.json")"
expect_eq "manifest keeps the existing fields: none added without the flags" "false" \
  "$(jq 'has("desktop_updates")' "$tmp/release.json")"
rm "$dd/QueryMT-Desktop-nightly-linux-aarch64.AppImage"
if "$here/write-release-json.sh" "0.6.0-nightly.$TS" nightly "$sha" "$tmp/x.json" \
  --desktop-dir "$dd" --version-name nightly 2>"$tmp/missing.err"; then
  fail "missing desktop asset fails (expected failure)"
elif grep -q "linux-aarch64" "$tmp/missing.err"; then pass "missing desktop asset fails"
else fail "missing desktop asset fails: error does not name the platform: $(cat "$tmp/missing.err")"; fi
if "$here/write-release-json.sh" "0.6.0-nightly.$TS" nightly "$sha" "$tmp/x.json" --desktop-dir "$dd" 2>/dev/null; then
  fail "desktop flags go together"
else pass "desktop flags go together"; fi

# --- sign-release-json.sh ----------------------------------------------------
# A throwaway key pair; the script verifies with it instead of the app's key.
if ! command -v minisign >/dev/null 2>&1; then
  fail "signature tests need minisign (brew install minisign, or apt-get install minisign)"
else
  keys="$tmp/keys"; mkdir -p "$keys"
  minisign -G -W -f -p "$keys/test.pub" -s "$keys/test.key" >/dev/null
  minisign -G -W -f -p "$keys/other.pub" -s "$keys/other.key" >/dev/null
  cp "$tmp/release-desktop.json" "$tmp/signed.json"
  QMT_UPDATE_SIGNING_KEY="$(cat "$keys/test.key")" QMT_UPDATE_PUBLIC_KEY="$keys/test.pub" \
    "$here/sign-release-json.sh" "$tmp/signed.json" 0.6.0-nightly.$TS nightly >/dev/null
  if out="$(minisign -V -p "$keys/test.pub" -m "$tmp/signed.json" -x "$tmp/signed.json.minisig" 2>&1)" \
    && printf '%s\n' "$out" | grep -qF "Trusted comment: version:0.6.0-nightly.$TS channel:nightly"; then
    pass "signature covers the manifest"
  else fail "signature covers the manifest: $out"; fi
  echo '{"version":"9.9.9"}' > "$tmp/tampered.json"
  cp "$tmp/signed.json.minisig" "$tmp/tampered.json.minisig"
  if minisign -V -p "$keys/test.pub" -m "$tmp/tampered.json" -x "$tmp/tampered.json.minisig" >/dev/null 2>&1; then
    fail "signature covers the manifest: a changed manifest still verifies"
  else pass "signature covers the manifest: a changed manifest does not verify"; fi
  # No key: no signature file, and a failing exit.
  rm -f "$tmp/unsigned.json.minisig"; cp "$tmp/release-desktop.json" "$tmp/unsigned.json"
  if env -u QMT_UPDATE_SIGNING_KEY QMT_UPDATE_PUBLIC_KEY="$keys/test.pub" \
    "$here/sign-release-json.sh" "$tmp/unsigned.json" 0.6.0 stable 2>"$tmp/nokey.err"; then
    fail "missing signing key fails (expected failure)"
  elif [ ! -e "$tmp/unsigned.json.minisig" ] && grep -q QMT_UPDATE_SIGNING_KEY "$tmp/nokey.err"; then
    pass "missing signing key fails"
  else fail "missing signing key fails: $(cat "$tmp/nokey.err")"; fi
  # A secret that is not the pair of the embedded key must fail, not publish.
  if QMT_UPDATE_SIGNING_KEY="$(cat "$keys/other.key")" QMT_UPDATE_PUBLIC_KEY="$keys/test.pub" \
    "$here/sign-release-json.sh" "$tmp/unsigned.json" 0.6.0 stable >/dev/null 2>&1; then
    fail "signing with a key that is not the embedded key's pair fails (expected failure)"
  elif [ ! -e "$tmp/unsigned.json.minisig" ]; then pass "signing with a key that is not the embedded key's pair fails"
  else fail "signing with a key that is not the embedded key's pair leaves a signature"; fi
fi

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

# --- QueryMT Desktop (crates/querymt-gpui) bundle collection -----------------
if bash "$here/../../crates/querymt-gpui/scripts/release/test-release-scripts.sh"; then
  pass "desktop: release script tests"; else fail "desktop: release script tests"; fi

if [ "$failures" -gt 0 ]; then
  echo "$failures test(s) failed" >&2
  exit 1
fi
echo "all release script tests passed"
