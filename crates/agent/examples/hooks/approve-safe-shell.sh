#!/bin/sh
input="$(cat)"

case "$input" in
  *"cargo test"*|*"cargo check"*)
    printf '{"hook_specific_output":{"hook_event_name":"permission_request","decision":{"behavior":"allow"}}}'
    ;;
  *)
    printf '{}'
    ;;
esac
