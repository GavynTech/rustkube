#!/usr/bin/env bash
# End-to-end smoke test, Phase 0 scope: the workspace builds, rk-api serves
# /healthz and /version, and rk-ctl talks to it over HTTP.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"

if curl -fsS "$API_URL/healthz" > /dev/null 2>&1; then
    echo "e2e: something is already listening on $API_URL — run 'make dev-down' first" >&2
    exit 1
fi

echo "==> building workspace"
cargo build --workspace

echo "==> starting rk-api"
mkdir -p .dev
./target/debug/rk-api > .dev/e2e-api.log 2>&1 &
API_PID=$!
trap 'kill "$API_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 40); do
    curl -fsS "$API_URL/healthz" > /dev/null 2>&1 && break
    sleep 0.25
done

echo "==> checking /healthz"
[ "$(curl -fsS "$API_URL/healthz")" = "ok" ]

echo "==> checking /version"
curl -fsS "$API_URL/version" | grep -q '"component":"rk-api"'

echo "==> checking rk-ctl version round-trip"
VERSION_OUTPUT="$(./target/debug/rk-ctl version)"
grep -q '^client: rk-ctl' <<< "$VERSION_OUTPUT"
grep -q '^server: rk-api' <<< "$VERSION_OUTPUT"

echo "e2e: PASS (phase 0 scope: build, healthz, version, rk-ctl round-trip)"
