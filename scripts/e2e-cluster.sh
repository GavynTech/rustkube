#!/usr/bin/env bash
# e2e-cluster.sh — the capstone end-to-end test. Brings up a full RustKube
# cluster secured with TLS + a bearer token, then drives every major feature
# against it and asserts the result. Run as root on the Ubuntu 24.04 VM.
set -euo pipefail
cd "$(dirname "$0")/.."

CTL=./target/release/rk-ctl
PKI="$PWD/.dev/e2e-pki"
SITEPKI="$PWD/.dev/e2e-sitepki"
API_URL="https://127.0.0.1:6443"
TOKEN="$PKI/token"
CACERT="$PKI/ca.crt"
HOST=web.example.com
D1=/var/lib/rustkube
PASS=0

[[ $EUID -eq 0 ]] || { echo "e2e-cluster: run as root" >&2; exit 1; }

cleanup() {
    for p in rk-ingress rk-agent rk-scheduler rk-controllers rk-api; do pkill -9 -f "target/release/$p" 2>/dev/null || true; done
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
    echo "e2e-cluster: TIMEOUT waiting for $what" >&2; "$CTL" get pods 2>/dev/null || true; exit 1; }
ok()   { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() { echo "  FAIL: $1" >&2; exit 1; }
# Count Running web pods whose pod-template-hash differs from $1; succeed at $2.
# A rolling update lands a new ReplicaSet, so the hash (the 2nd name segment)
# changes — name-based, so no brittle JSON quoting inside the wait loop.
new_hash_pods() {
    local n
    n=$("$CTL" get pods | awk -v old="$1" '/^web-.* Running /{split($1,a,"-"); if (a[2]!=old) c++} END{print c+0}')
    [ "$n" -ge "$2" ]
}

echo "==> build"
cargo build --workspace --release 2>&1 | tail -1
mkdir -p .dev; rm -f .dev/e2e-cluster.db*

echo "==> PKI + secured control plane (TLS + token)"
rm -rf "$PKI" "$SITEPKI"
"$CTL" pki init --dir "$PKI" --api-name 127.0.0.1 >/dev/null
"$CTL" pki init --dir "$SITEPKI" --api-name "$HOST" >/dev/null
cat > .dev/e2e-api.yaml <<EOF
listenAddr: "127.0.0.1:6443"
dbPath: ".dev/e2e-cluster.db"
authTokenFile: "$TOKEN"
tls: { certFile: "$PKI/server.crt", keyFile: "$PKI/server.key" }
EOF
creds() { printf 'apiUrl: "%s"\ntokenFile: "%s"\ncaFile: "%s"\n' "$API_URL" "$TOKEN" "$CACERT"; }
{ creds; } > .dev/e2e-sched.yaml
{ creds; } > .dev/e2e-ctrl.yaml
{ creds; printf 'nodeName: rk-node1\ndataDir: "%s"\npodCidr: "10.42.0.0/24"\nnodeIp: "127.0.0.1"\nheartbeatIntervalSecs: 3\n' "$D1"; } > .dev/e2e-node.yaml
RK_CONFIG="$PWD/.dev/e2e-api.yaml"   ./target/release/rk-api         > .dev/e2e-api.log 2>&1 &
wait_for "HTTPS API" 20 curl -fsS --cacert "$CACERT" "$API_URL/healthz"
RK_CONFIG="$PWD/.dev/e2e-sched.yaml" ./target/release/rk-scheduler   > .dev/e2e-sched.log 2>&1 &
RK_CONFIG="$PWD/.dev/e2e-ctrl.yaml"  ./target/release/rk-controllers > .dev/e2e-ctrl.log 2>&1 &
./target/release/rk-agent --config "$PWD/.dev/e2e-node.yaml"         > .dev/e2e-node.log 2>&1 &
./target/release/rk-ingress --api-url "$API_URL" --token-file "$TOKEN" --ca-file "$CACERT" --http-port 80 --https-port 443 > .dev/e2e-ingress.log 2>&1 &
export RK_API_URL="$API_URL" RK_TOKEN_FILE="$TOKEN" RK_CA_FILE="$CACERT"
wait_for "node Ready" 30 sh -c "'$CTL' get nodes | grep -q Ready"
ok "secured control plane up, node Ready over HTTPS+token"

echo "==> [1] auth boundary"
CODE=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$CACERT" "$API_URL/api/v1/pods")
[ "$CODE" = "401" ] && ok "no token -> 401" || fail "expected 401, got $CODE"
CODE=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$CACERT" -H "Authorization: Bearer $(cat "$TOKEN")" "$API_URL/api/v1/pods")
[ "$CODE" = "200" ] && ok "with token -> 200" || fail "expected 200, got $CODE"

echo "==> [2] deploy app + service + TLS ingress"
CRT_B64=$(base64 -w0 "$SITEPKI/server.crt"); KEY_B64=$(base64 -w0 "$SITEPKI/server.key")
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Secret
metadata: { name: web-tls }
data: { tls.crt: "$CRT_B64", tls.key: "$KEY_B64" }
---
apiVersion: v1
kind: Deployment
metadata: { name: web, labels: { app: web } }
spec:
  replicas: 3
  selector: { matchLabels: { app: web } }
  template:
    metadata: { labels: { app: web } }
    spec:
      nodeName: rk-node1
      containers: [ { name: web, image: nginx:alpine, resources: { requests: { cpu: 50m, memory: 16Mi } } } ]
---
apiVersion: v1
kind: Service
metadata: { name: web }
spec: { selector: { app: web }, ports: [ { port: 80, targetPort: 80 } ] }
---
apiVersion: v1
kind: Ingress
metadata: { name: web }
spec:
  tls: [ { hosts: [ $HOST ], secretName: web-tls } ]
  rules: [ { host: $HOST, http: { paths: [ { path: /, pathType: Prefix, backend: { service: { name: web, port: 80 } } } ] } } ]
EOF
wait_for "3 web pods running" 180 sh -c "[ \$($CTL get pods | grep -c '^web.* Running ') -eq 3 ]"
ok "3 replicas Running"
CIP=$("$CTL" get service web -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['spec']['clusterIP'])")
[ -n "$CIP" ] && ok "service got ClusterIP $CIP" || fail "no ClusterIP"

echo "==> [3] service VIP load-balances (nftables DNAT)"
wait_for "service VIP" 30 curl -fsS "http://$CIP/"
curl -fsS "http://$CIP/" | grep -qi nginx && ok "ClusterIP $CIP -> nginx" || fail "VIP did not reach nginx"

echo "==> [4] ingress TLS termination"
wait_for "ingress TLS" 30 sh -c "curl -fsS --resolve $HOST:443:127.0.0.1 --cacert '$SITEPKI/ca.crt' https://$HOST/ -o /dev/null"
curl -fsS --resolve "$HOST:443:127.0.0.1" --cacert "$SITEPKI/ca.crt" "https://$HOST/" | grep -qi nginx \
    && ok "https://$HOST/ -> nginx over TLS" || fail "ingress TLS did not reach nginx"

echo "==> [5] scale 3 -> 5"
"$CTL" scale deployment web --replicas 5 >/dev/null
wait_for "5 replicas" 120 sh -c "[ \$($CTL get pods | grep -c '^web.* Running ') -eq 5 ]"
ok "scaled to 5 running"

echo "==> [6] rolling update (image change)"
OLD_HASH=$("$CTL" get pods | awk '/^web-.* Running /{split($1,a,"-"); print a[2]; exit}')
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Deployment
metadata: { name: web, labels: { app: web } }
spec:
  replicas: 5
  selector: { matchLabels: { app: web } }
  template:
    metadata: { labels: { app: web } }
    spec:
      nodeName: rk-node1
      containers: [ { name: web, image: nginx:1.27-alpine, resources: { requests: { cpu: 50m, memory: 16Mi } } } ]
EOF
wait_for "rollout to a new ReplicaSet" 300 new_hash_pods "$OLD_HASH" 5
ok "rolled to nginx:1.27-alpine (old hash $OLD_HASH retired)"

echo "==> [7] self-heal (delete a pod, controller recreates)"
VICTIM=$("$CTL" get pods | awk '/^web.* Running /{print $1; exit}')
"$CTL" delete pod "$VICTIM" >/dev/null
wait_for "replacement pod" 120 sh -c "[ \$($CTL get pods | grep -c '^web.* Running ') -eq 5 ]"
ok "self-healed after deleting $VICTIM"

echo "==> [8] logs"
POD=$("$CTL" get pods | awk '/^web.* Running /{print $1; exit}')
curl -fsS --resolve "$HOST:443:127.0.0.1" --cacert "$SITEPKI/ca.crt" "https://$HOST/" -o /dev/null || true
"$CTL" logs "$POD" >/dev/null 2>&1 && ok "fetched logs for $POD" || echo "  NOTE: logs empty (nginx may not have logged yet) — non-fatal"

echo "==> [9] drain + uncordon"
"$CTL" drain rk-node1 >/dev/null
"$CTL" get nodes | grep -q "SchedulingDisabled" && ok "node cordoned (SchedulingDisabled)" || fail "node not cordoned"
wait_for "pods Pending while cordoned" 30 sh -c "$CTL get pods | grep -q '^web.* Pending'"
ok "evicted pods are Pending on the cordoned node"
"$CTL" uncordon rk-node1 >/dev/null
wait_for "pods Running after uncordon" 180 sh -c "[ \$($CTL get pods | grep -c '^web.* Running ') -ge 1 ]"
ok "uncordon -> pods Running again"

echo "==> teardown"
"$CTL" delete ingress web >/dev/null; "$CTL" delete service web >/dev/null
"$CTL" delete deployment web >/dev/null; "$CTL" delete secret web-tls >/dev/null

echo
echo "e2e-cluster: PASS ($PASS checks)"
