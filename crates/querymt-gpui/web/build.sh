#!/usr/bin/env bash
# Build the web (wasm) version into ./dist.
#
#   nix develop -c ./web/build.sh            # debug build (fast)
#   nix develop -c ./web/build.sh --release  # size-optimized build
#   nix develop -c miniserve --index index.html dist   # serve on :8080
set -euo pipefail
cd "$(dirname "$0")/.."

profile=dev
dir=debug
if [[ "${1:-}" == "--release" ]]; then
  profile=release-web
  dir=release-web
fi

# gpui-pre-web pulls `wasm_thread`, which uses a nightly-only feature gate.
# RUSTC_BOOTSTRAP lets the stable toolchain accept it. We still run the app
# single-threaded (see src/main.rs), so no special server headers are needed.
RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown --profile "$profile"

rm -rf dist
mkdir -p dist
wasm-bindgen --target web --no-typescript --out-name querymt_gpui --out-dir dist \
  "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/$dir/querymt-gpui.wasm"

if [[ "$profile" == release-web ]]; then
  # LTO can retain atomic instructions even though the app uses unshared memory.
  wasm-opt -Oz --strip-debug --strip-producers \
    --enable-threads --enable-bulk-memory --enable-nontrapping-float-to-int \
    --enable-sign-ext --enable-mutable-globals --enable-reference-types \
    dist/querymt_gpui_bg.wasm -o dist/querymt_gpui_bg.wasm
fi

cp web/index.html dist/


ls -lh dist/*.wasm
echo "Built dist/. Serve with: miniserve --index index.html dist"
