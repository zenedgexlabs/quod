#!/bin/sh
# Local CI: the same checks as .github/workflows/ci.yml.
# Usage: scripts/ci.sh        (from the repo root or anywhere inside it)
set -eu

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(git rev-parse --show-toplevel)"

step() { printf '\n==> %s\n' "$1"; }

step "fmt"
cargo fmt --all -- --check

step "clippy"
cargo clippy --all-targets --locked -- -D warnings

step "test"
cargo test --locked

export PATH="$HOME/.local/bin:$PATH"
if command -v uv >/dev/null 2>&1; then
  step "python probe"
  (cd probes/python && uv run --locked -q pytest -q)
else
  printf '\n(skipping python probe: uv not found)\n'
fi

printf '\nlocal CI passed\n'
