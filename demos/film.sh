#!/usr/bin/env bash
# Film a QueryMT Desktop scene and render it into a finished demo video
# (change add-gpui-demo-videos). See demos/README.md.
#
#   demos/film.sh [--no-film] [--all | <clip>...]
#
#   --no-film   skip filming and re-render from the footage already in $QMT_FILM_OUT
#   --all       every folder under demos/clips/
#
# Environment:
#   HYPERFRAMES   HyperFrames CLI version for npx (default: the pin below)
#   QMT_FILM_OUT  where the film harness writes clips (default: target/film)
set -euo pipefail

HYPERFRAMES="${HYPERFRAMES:-0.9.98}"

cd "$(git rev-parse --show-toplevel)"
ROOT="$PWD"
export QMT_FILM_OUT="${QMT_FILM_OUT:-$ROOT/target/film}"
DEMOS_OUT="$ROOT/target/demos"

usage() {
  cat >&2 <<'EOF'
usage: demos/film.sh [--no-film] [--all | <clip>...]

  <clip>      a folder under demos/clips/ (scene.toml + clip.json)
  --all       every clip
  --no-film   re-render from existing footage; skips the film test
EOF
}

die() {
  echo "film.sh: $*" >&2
  exit 1
}

film=1
all=0
clips=()
for arg in "$@"; do
  case "$arg" in
    --no-film) film=0 ;;
    --all) all=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    -*)
      echo "film.sh: unknown option $arg" >&2
      usage
      exit 2
      ;;
    *) clips+=("$arg") ;;
  esac
done

if [ "$all" = 1 ]; then
  [ "${#clips[@]}" -eq 0 ] || {
    echo "film.sh: --all takes no clip names" >&2
    usage
    exit 2
  }
  for dir in demos/clips/*/; do
    clips+=("$(basename "$dir")")
  done
fi
if [ "${#clips[@]}" -eq 0 ]; then
  usage
  exit 2
fi

for clip in "${clips[@]}"; do
  [ -f "demos/clips/$clip/scene.toml" ] || die "no demos/clips/$clip/scene.toml"
  [ -f "demos/clips/$clip/clip.json" ] || die "no demos/clips/$clip/clip.json"
done

# --- Prerequisites, each with its fix.
command -v node > /dev/null 2>&1 || die "Node 22 or newer is required (https://nodejs.org); node is not on PATH"
node_major="$(node -p 'process.versions.node.split(".")[0]')"
[ "$node_major" -ge 22 ] || die "Node 22 or newer is required (found $(node -p process.versions.node))"
command -v npx > /dev/null 2>&1 || die "npx is required (it comes with Node)"
command -v ffmpeg > /dev/null 2>&1 || die "ffmpeg is required (brew install ffmpeg)"
if [ "$film" = 1 ]; then
  cargo nextest --version > /dev/null 2>&1 || die "cargo-nextest is required (cargo install cargo-nextest --locked)"
fi
for clip in "${clips[@]}"; do
  # The quick panel is filmed as a VP9 alpha track unless the scene composites it.
  if grep -q '^quick_open' "demos/clips/$clip/scene.toml" && ! grep -Eq '^panel *= *"composite"' "demos/clips/$clip/scene.toml"; then
    ffmpeg -hide_banner -encoders 2> /dev/null | grep -q libvpx-vp9 \
      || die "clip $clip films the quick panel, which needs ffmpeg with libvpx-vp9 (brew install ffmpeg)"
  fi
done

mkdir -p "$DEMOS_OUT"
for clip in "${clips[@]}"; do
  echo "==> $clip"
  scene="demos/clips/$clip/scene.toml"

  if [ "$film" = 1 ]; then
    QMT_FILM_SCENE="$scene" cargo nextest run -p querymt-gpui --bins --run-ignored only \
      -E 'test(=film::film_scene)' --no-capture \
      || die "filming $clip failed"
  fi

  QMT_FILM_OUT="$QMT_FILM_OUT" QMT_DEMOS_OUT="$DEMOS_OUT" node demos/build.mjs "$clip" \
    || die "assembling $clip failed"

  npx --yes "hyperframes@$HYPERFRAMES" lint "$DEMOS_OUT/$clip" || die "lint found errors in $clip"

  npx --yes "hyperframes@$HYPERFRAMES" render "$DEMOS_OUT/$clip" \
    --variables-file "$ROOT/demos/clips/$clip/clip.json" \
    --output "$DEMOS_OUT/$clip.mp4" \
    --quality delivery \
    || die "rendering $clip failed"

  echo "==> $DEMOS_OUT/$clip.mp4"
done
