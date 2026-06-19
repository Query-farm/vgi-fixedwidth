#!/usr/bin/env bash
# Build the fixedformat VGI worker and run the SQLLogic tests against it using
# the haybarn DuckDB distribution's unittest runner (which ships the `vgi`
# extension via the community repository).
#
# Prerequisites (one-time):
#   uv tool install haybarn-unittest      # the DuckDB unittest binary
#   uv tool install haybarn               # the haybarn runtime
#   echo "INSTALL vgi FROM community;" | uvx haybarn-cli   # install the vgi ext
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

UNITTEST="${VGI_UNITTEST:-$(command -v haybarn-unittest || true)}"
if [[ -z "$UNITTEST" || ! -x "$UNITTEST" ]]; then
    echo "ERROR: haybarn-unittest not found. Install it with:" >&2
    echo "       uv tool install haybarn-unittest" >&2
    exit 1
fi

# Ensure the vgi community extension is installed for this haybarn version.
if ! echo "LOAD vgi;" | uvx haybarn-cli >/dev/null 2>&1; then
    echo "==> Installing vgi extension from community repository"
    echo "INSTALL vgi FROM community;" | uvx haybarn-cli
fi

echo "==> Building fixedformat-worker (release)"
cargo build --release --bin fixedformat-worker

# NOTE: this is a Catch2 test-name filter, not a shell glob. Catch2 only honors
# a trailing `*` wildcard, so use `test/sql/*` (not `test/sql/*.test`).
WORKER="$REPO_ROOT/target/release/fixedformat-worker"
TEST_GLOB="${1:-test/sql/*}"

echo "==> Running SQLLogic tests"
echo "    worker:   $WORKER"
echo "    unittest: $UNITTEST"
echo "    tests:    $TEST_GLOB"

VGI_TEST_WORKER="$WORKER" \
VGI_WORKER_CATALOG_NAME="fixed" \
    "$UNITTEST" --test-dir "$REPO_ROOT" "$TEST_GLOB"
