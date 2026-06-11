#!/usr/bin/env bash
# End-to-end smoke test, Phase 2 scope: the API server stores what the CLI
# applies, lists carry resume revisions, the watch stream reports changes,
# and deletes propagate — all over real HTTP against a real sqlite store.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/debug/rk-ctl

if curl -fsS "$API_URL/healthz" > /dev/null 2>&1; then
    echo "e2e: something is already listening on $API_URL — run 'make dev-down' first" >&2
    exit 1
fi

echo "==> building workspace"
cargo build --workspace

echo "==> starting rk-api (fresh store)"
mkdir -p .dev
rm -f .dev/e2e.db .dev/e2e.db-shm .dev/e2e.db-wal
printf 'listenAddr: "127.0.0.1:6443"\ndbPath: ".dev/e2e.db"\n' > .dev/e2e-api.yaml
RK_CONFIG=.dev/e2e-api.yaml ./target/debug/rk-api > .dev/e2e-api.log 2>&1 &
API_PID=$!
trap 'kill "$API_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 40); do
    curl -fsS "$API_URL/healthz" > /dev/null 2>&1 && break
    sleep 0.25
done

echo "==> healthz / version / rk-ctl round-trip"
[ "$(curl -fsS "$API_URL/healthz")" = "ok" ]
curl -fsS "$API_URL/version" | grep -q '"component":"rk-api"'
"$CTL" version | grep -q '^server: rk-api'

echo "==> apply creates"
"$CTL" apply -f examples/pod.yaml | grep -qx 'pod/web-0 created'

echo "==> get shows the pod"
"$CTL" get pods | grep -q '^web-0 '
"$CTL" get pod web-0 -o yaml | grep -q 'name: web-0'

echo "==> re-apply configures (update path, optimistic concurrency)"
"$CTL" apply -f examples/pod.yaml | grep -qx 'pod/web-0 configured'
"$CTL" get pod web-0 -o json | grep -q '"resourceVersion": 2'

echo "==> stale resourceVersion conflicts (409)"
STALE=$(curl -fsS "$API_URL/api/v1/namespaces/default/pods/web-0" \
    | sed 's/"resourceVersion":2/"resourceVersion":1/')
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -H 'Content-Type: application/json' -d "$STALE" \
    "$API_URL/api/v1/namespaces/default/pods/web-0")
[ "$CODE" = "409" ]

echo "==> watch streams live events"
"$CTL" get pods --watch > .dev/e2e-watch.log 2>&1 &
WATCH_PID=$!
sleep 1
sed 's/web-0/web-1/' examples/pod.yaml | "$CTL" apply -f - | grep -qx 'pod/web-1 created'
for _ in $(seq 1 20); do
    grep -q 'ADDED.*web-1' .dev/e2e-watch.log && break
    sleep 0.25
done
kill "$WATCH_PID" 2>/dev/null || true
grep -q 'ADDED.*web-1' .dev/e2e-watch.log

echo "==> delete removes"
"$CTL" delete pod web-1
"$CTL" delete pod web-0
"$CTL" get pods | grep -qx 'No pods found'

echo "==> deleted objects 404"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$API_URL/api/v1/namespaces/default/pods/web-0")
[ "$CODE" = "404" ]

echo "e2e: PASS (phase 2 scope: apply/get/update/conflict/watch/delete over HTTP + sqlite)"
