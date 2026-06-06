#!/bin/sh
input="$(cat)"

case "$input" in
  *"rm -rf"*|*"git reset --hard"*)
    printf '{"decision":"block","reason":"Dangerous shell command blocked by local policy"}'
    ;;
  *)
    printf '{}'
    ;;
esac
