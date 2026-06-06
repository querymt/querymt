#!/bin/sh
input="$(cat)"

case "$input" in
  *"validation"*|*"tests"*)
    printf '{}'
    ;;
  *)
    printf '{"continue":false,"reason":"The turn ended without describing validation.","hook_specific_output":{"hook_event_name":"stop","additional_context":"Ask the agent to summarize what validation was run or why it was skipped."}}'
    ;;
esac
