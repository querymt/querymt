#!/usr/bin/env bash
# Helpers shared by the packaging scripts. Source it; it defines functions only.

# resolve_qmtcode
#
# Prints the path of the qmtcode binary to bundle, taken from QMTCODE_BINARY.
#   - QMTCODE_BINARY set and a file: prints it.
#   - QMTCODE_BINARY set but not a file: error, returns 1.
#   - unset under CI (CI is non-empty): error naming QMTCODE_BINARY, returns 1.
#   - unset elsewhere: warns on stderr and prints nothing, returns 0, so the
#     caller packages without qmtcode.
# Diagnostics go to stderr; only the path goes to stdout.
resolve_qmtcode() {
  if [ -n "${QMTCODE_BINARY:-}" ]; then
    if [ ! -f "$QMTCODE_BINARY" ]; then
      echo "error: QMTCODE_BINARY does not point to a file: $QMTCODE_BINARY" >&2
      return 1
    fi
    printf '%s\n' "$QMTCODE_BINARY"
    return 0
  fi
  if [ -n "${CI:-}" ]; then
    echo "error: QMTCODE_BINARY is not set; CI packaging must bundle qmtcode" >&2
    return 1
  fi
  echo "warning: QMTCODE_BINARY is not set; the installer will have no bundled qmtcode" >&2
  return 0
}
