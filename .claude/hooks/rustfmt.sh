#!/usr/bin/env bash
# Formats a Rust file right after Claude edits it, so `make qa` never fails on
# formatting alone. Silent and non-blocking: a failure here must never stop an
# edit — `cargo fmt --check` in `make qa` remains the actual gate.
set -uo pipefail
file=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input",{}).get("file_path",""))' 2>/dev/null) || exit 0
[[ $file == *.rs ]] || exit 0
[[ -f $file ]] || exit 0
rustfmt --edition 2024 "$file" >/dev/null 2>&1 || true
exit 0
