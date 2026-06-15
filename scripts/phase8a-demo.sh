#!/usr/bin/env bash
# Phase 8A demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Hardening, part 1: TLS + bearer-token auth across the control plane.
#   1. `rk-ctl pki init` mints a CA, an API-server cert, and a token offline.
#   2. rk-api serves HTTPS and rejects any request without the token (except
#      /healthz). Plaintext HTTP is refused.
#   3. the scheduler, controllers, agent and CLI all talk to it over HTTPS with
#      the token — a pod is scheduled and runs to prove the whole plane works
#      authenticated and encrypted.
set -euo pipefail
cd "$(dirname "$0")/.."

CTL=./target/release/rk-ctl
PKI="$PWD/.dev/pki"
API_URL="https://127.0.0.1:6443"
TOKEN="$PKI/token"
CACERT="$PKI/ca.crt"
D1=/var/lib/rustkube

[[ $EUID -eq 0 ]] || { echo "phase8a-demo: run as root" >&2; exit 1; }

cleanup() {
    for p in rk-agent rk-scheduler rk-controllers rk-api; do pkill -9 -f "target/release/$p" 2>/dev/null || true; done
    for pidfile in "$D1"/containers/p*/pid1; do [[ -f $pidfile ]] && kill -9 "$(cat "$pidfile")" 2>/dev/null || true; done
    pkill -9 -f "shim $D1" 2>/dev/null || true
    sleep 0.5
    for c in "$D1"/containers/p*; do [[ -d $c ]] && umount -l "$c/rootfs" 2>/dev/null || true; done
    rm -rf "$D1"/containers/p* "$D1"/net/ipam.json 2>/dev/null || true
    ip link del rk-br0 2>/dev/null || true
    for l in $(ip -brief link | awk '/^rk/{print $1}' | cut -d@ -f1); do ip link del "$l" 2>/dev/null || true; done
    for cg in /sys/fs/cgroup/rustkube/*/; do rmdir "$cg" 2>/dev/null || true; done
    nft delete table ip rk-services 2>/dev/null || true
}
trap cleanup EXIT
cleanup

wait_for() { local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
    echo "phase8a-demo: timed out waiting for $what" >&2; return 1; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1
mkdir -p .dev; rm -f .dev/phase8a.db*

echo
echo "==> 1. mint PKI offline: rk-ctl pki init"
rm -rf "$PKI"
"$CTL" pki init --dir "$PKI" --api-name 127.0.0.1 --api-name localhost
echo "  files:"; ls -l "$PKI" | awk 'NR>1{print "    "$1, $NF}'

echo
echo "==> 2. start rk-api with TLS + token auth"
cat > .dev/p8a-api.yaml <<EOF
listenAddr: "127.0.0.1:6443"
dbPath: ".dev/phase8a.db"
authTokenFile: "$TOKEN"
tls:
  certFile: "$PKI/server.crt"
  keyFile: "$PKI/server.key"
EOF
RK_CONFIG="$PWD/.dev/p8a-api.yaml" ./target/release/rk-api > .dev/p8a-api.log 2>&1 &
wait_for "HTTPS API up" 20 curl -fsS --cacert "$CACERT" "$API_URL/healthz"

echo
echo "==> 3. the TLS + auth boundary"
echo -n "  https /healthz (open, no token):           "; curl -fsS --cacert "$CACERT" "$API_URL/healthz" && echo
CODE=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$CACERT" "$API_URL/api/v1/pods")
echo    "  https /api/v1/pods without token:          HTTP $CODE (expect 401)"
[ "$CODE" = "401" ] || { echo "expected 401 without token"; exit 1; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$CACERT" -H "Authorization: Bearer $(cat "$TOKEN")" "$API_URL/api/v1/pods")
echo    "  https /api/v1/pods with token:             HTTP $CODE (expect 200)"
[ "$CODE" = "200" ] || { echo "expected 200 with token"; exit 1; }
if curl -s --max-time 5 "http://127.0.0.1:6443/healthz" >/dev/null 2>&1; then
    echo "  plaintext HTTP unexpectedly accepted"; exit 1
else
    echo "  plaintext http://… refused:                TLS required"
fi
# An untrusted client (no CA) must reject the self-signed cert.
if curl -s --max-time 5 "$API_URL/healthz" >/dev/null 2>&1; then
    echo "  cert accepted without the CA — unexpected"; exit 1
else
    echo "  https without --cacert refused:            cert not trusted"
fi

echo
echo "==> 4. the rest of the control plane, all over HTTPS + token"
# Components point at the HTTPS URL and carry the token + CA.
creds() { printf 'apiUrl: "%s"\ntokenFile: "%s"\ncaFile: "%s"\n' "$API_URL" "$TOKEN" "$CACERT"; }
{ creds; } > .dev/p8a-sched.yaml
{ creds; echo "nodeNotReadySecs: 30"; } > .dev/p8a-ctrl.yaml
{ creds; printf 'nodeName: rk-node1\ndataDir: "%s"\npodCidr: "10.42.0.0/24"\nnodeIp: "127.0.0.1"\nheartbeatIntervalSecs: 3\n' "$D1"; } > .dev/p8a-node1.yaml
RK_CONFIG="$PWD/.dev/p8a-sched.yaml" ./target/release/rk-scheduler   > .dev/p8a-sched.log 2>&1 &
RK_CONFIG="$PWD/.dev/p8a-ctrl.yaml"  ./target/release/rk-controllers > .dev/p8a-ctrl.log 2>&1 &
RK_CONFIG="$PWD/.dev/p8a-node1.yaml" ./target/release/rk-agent       > .dev/p8a-node1.log 2>&1 &

# The CLI reads its creds from the environment like a kubeconfig would.
export RK_API_URL="$API_URL" RK_TOKEN_FILE="$TOKEN" RK_CA_FILE="$CACERT"
wait_for "node Ready (agent authenticated over TLS)" 30 sh -c "'$CTL' get nodes | grep -q Ready"
"$CTL" get nodes

echo
echo "==> 5. schedule a pod through the authenticated plane"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Deployment
metadata: { name: tls-demo, labels: { app: tls-demo } }
spec:
  replicas: 1
  selector: { matchLabels: { app: tls-demo } }
  template:
    metadata: { labels: { app: tls-demo } }
    spec:
      nodeName: rk-node1
      containers: [ { name: web, image: nginx:alpine, resources: { requests: { cpu: 50m, memory: 16Mi } } } ]
EOF
wait_for "the pod to run" 180 sh -c "$CTL get pods | grep -q '^tls-demo.* Running '"
"$CTL" get pods

echo
echo "==> 6. the CLI without the token is rejected too"
# Drop only the token (keep the CA, so TLS still completes) — the server
# answers 401, and the CLI exits non-zero.
if env -u RK_TOKEN_FILE "$CTL" get pods >/dev/null 2>&1; then
    echo "  CLI without token unexpectedly succeeded"; exit 1
else
    echo "  rk-ctl get pods without token: refused by the server (401)"
fi

echo
echo "==> teardown"
"$CTL" delete deployment tls-demo >/dev/null

echo
echo "phase8a-demo: OK"
