#!/usr/bin/env bash
# Decide which agent packages a change affects, and what CI runs for them.
#
# Used by the `detect` job of .github/workflows/test-agent.yml. Inputs are
# environment variables ("true"/"false"), normally the outputs of
# dorny/paths-filter; outputs are written to $GITHUB_OUTPUT (stdout if unset).
#
#   EVENT_NAME       github.event_name. workflow_dispatch tests everything; a push
#                    tests everything when F_ANY is true; a pull_request tests only
#                    the packages its changes reach
#   F_SHARED         a shared input changed (querymt, utils, Cargo.lock, CI config, ...)
#   F_PROTOCOL       crates/agent-protocol-types
#   F_CORE           crates/agent-core
#   F_INDEX          crates/agent-index
#   F_KNOWLEDGE      crates/agent-knowledge
#   F_SQLITE         crates/agent-sqlite
#   F_SERVICES       crates/agent-services
#   F_RUNTIME        crates/agent-runtime
#   F_AGENT          crates/agent (anything)
#   F_AGENT_READ     crates/agent/{src,examples}, which runtime tests read by path
#   F_CORPUS         the repository knowledge corpus (.qmt, crates/*/knowledge, specs)
#   F_DOCS           docs and test data that services/runtime tests read by path
#
# Dependency edges (Cargo.toml, dev-dependencies included):
#   protocol  <- everything (core re-exports it)
#   core      <- everything
#   index     <- knowledge, services, runtime, agent
#   knowledge <- services, runtime, agent
#   sqlite    <- services (dev), runtime (dev), agent
#   services  <- runtime, agent
#   runtime   <- agent
#
# Portable to bash 3.2 (macOS) so it can be tested locally.
# Keep the feature lists in sync with .cargo/config.toml (t-agent, t-runtime).
set -euo pipefail

AGENT_FEATURES="dashboard-server,oauth,zbus-secret-service,systemd-credentials,remote"
RUNTIME_FEATURES="remote,oauth,plugin-loaders,api"

PKGS="protocol core index knowledge sqlite services runtime agent"

# flag NAME -> true when the F_NAME input is "true"
flag() {
  local v="F_$1"
  [[ "${!v:-false}" == "true" ]]
}
hit() {
  local v="H_$1"
  [[ "${!v:-false}" == "true" ]]
}
mark() {
  local p
  for p in "$@"; do printf -v "H_$p" '%s' true; done
}

event="${EVENT_NAME:-pull_request}"
anything=false
for n in SHARED PROTOCOL CORE INDEX KNOWLEDGE SQLITE SERVICES RUNTIME AGENT AGENT_READ CORPUS DOCS; do
  if flag "$n"; then anything=true; fi
done
everything=false
if [[ $event == workflow_dispatch ]]; then everything=true; fi
if [[ $event != pull_request && $anything == true ]]; then everything=true; fi
if flag SHARED || flag PROTOCOL || flag CORE; then everything=true; fi
if [[ $everything == true ]]; then
  # shellcheck disable=SC2086
  mark $PKGS
fi
if flag INDEX; then mark index knowledge services runtime agent; fi
if flag KNOWLEDGE; then mark knowledge services runtime agent; fi
if flag SQLITE; then mark sqlite services runtime agent; fi
if flag SERVICES; then mark services runtime agent; fi
if flag RUNTIME; then mark runtime agent; fi
if flag AGENT; then mark agent; fi
if flag AGENT_READ; then mark runtime agent; fi
if flag CORPUS; then mark agent; fi
if flag DOCS; then mark services runtime agent; fi

pkg_name() {
  case $1 in
    agent) echo querymt-agent ;;
    protocol) echo querymt-agent-protocol-types ;;
    *) echo "querymt-agent-$1" ;;
  esac
}
pkg_dir() {
  case $1 in
    agent) echo crates/agent/ ;;
    protocol) echo crates/agent-protocol-types/ ;;
    *) echo "crates/agent-$1/" ;;
  esac
}

# "a,b,c" -> "pkg/a,pkg/b,pkg/c"
qualify() {
  local pkg="$1" list="$2" out="" f
  local IFS=,
  for f in $list; do out+="${out:+,}$pkg/$f"; done
  echo "$out"
}

any=false
test_args=""
codecov_specs=""
lower_args=""
for p in $PKGS; do
  hit "$p" || continue
  any=true
  test_args+=" -p $(pkg_name "$p")"
  codecov_specs+=" $(pkg_name "$p")=$(pkg_dir "$p")"
  case $p in protocol | core | index | knowledge | sqlite | services) lower_args+=" -p $(pkg_name "$p")" ;; esac
done

# `pkg/feature` keeps --features valid for a multi-package selection.
features=""
if hit agent; then features="$(qualify querymt-agent "$AGENT_FEATURES")"; fi
if hit runtime; then features+="${features:+,}$(qualify querymt-agent-runtime "$RUNTIME_FEATURES")"; fi
feature_flag=""
if [[ -n $features ]]; then feature_flag="--features $features"; fi

clippy='[]'
add() { clippy="$(jq -c --arg n "$1" --arg f "$2" '. + [{name: $n, flags: $f}]' <<<"$clippy")"; }
if [[ -n $lower_args ]]; then add lower "${lower_args# }"; fi
if hit runtime; then add runtime "-p querymt-agent-runtime --features $RUNTIME_FEATURES"; fi
if hit agent; then add agent "-p querymt-agent --features $AGENT_FEATURES"; fi

# Standalone builds: each package must compile on its own, with only the features
# it declares itself (run with the workspace-hack disabled, which would otherwise
# unify them). One matrix entry per affected package; `checks` is a JSON array of
# `cargo check` argument strings joined with "|".
standalone='[]'
sa() {
  hit "$1" || return 0
  local joined="" c
  for c in "${@:2}"; do joined+="${joined:+|}$c"; done
  standalone="$(jq -c --arg n "$1" --arg c "$joined" '. + [{name: $n, checks: $c}]' <<<"$standalone")"
}
# The protocol types also build for the browser (wasm32-unknown-unknown).
sa protocol "-p querymt-agent-protocol-types --no-default-features" "-p querymt-agent-protocol-types --features test-support" \
  "-p querymt-agent-protocol-types --target wasm32-unknown-unknown"
sa core "-p querymt-agent-core --no-default-features" "-p querymt-agent-core --features test-support"
sa index "-p querymt-agent-index --no-default-features"
sa knowledge "-p querymt-agent-knowledge --no-default-features"
sa sqlite "-p querymt-agent-sqlite --no-default-features" "-p querymt-agent-sqlite --features test-support"
sa services "-p querymt-agent-services --no-default-features" "-p querymt-agent-services --features test-support"
sa runtime "-p querymt-agent-runtime --no-default-features" "-p querymt-agent-runtime --features test-support" \
  "-p querymt-agent-runtime --features $RUNTIME_FEATURES"
sa agent "-p querymt-agent --lib --no-default-features"

emit() { echo "$1=$2" >>"${GITHUB_OUTPUT:-/dev/stdout}"; }
emit any "$any"
for p in $PKGS; do
  if hit "$p"; then emit "$p" true; else emit "$p" false; fi
done
emit test_args "${test_args# }"
emit features "$feature_flag"
emit codecov_specs "${codecov_specs# }"
emit clippy "$clippy"
emit standalone "$standalone"
