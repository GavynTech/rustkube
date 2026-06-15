#!/usr/bin/env bash
# Phase 8B demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Hardening, part 2:
#   A. the ingress terminates TLS on :443 using a cert from the Ingress's tls
#      Secret (SNI), then proxies cleartext to the backend.
#   B. `rk-ctl drain` cordons a node and evicts its pods; `cordon`/`uncordon`
#      gate scheduling.
set -euo pipefail
cd "$(dirname "$0")/.."

CTL=./target/release/rk-ctl
API_URL="http://127.0.0.1:6443"
SITEPKI="$PWD/.dev/sitepki"
HOST=shop.example.com
D1=/var/lib/rustkube

[[ $EUID -eq 0 ]] || { echo "phase8b-demo: run as root" >&2; exit 1; }

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
    echo "phase8b-demo: timed out waiting for $what" >&2; "$CTL" get pods 2>/dev/null || true; return 1; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1
mkdir -p .dev; rm -f .dev/phase8b.db*

echo
echo "==> mint a TLS cert for $HOST (reusing rk-ctl pki)"
rm -rf "$SITEPKI"
"$CTL" pki init --dir "$SITEPKI" --api-name "$HOST" >/dev/null
echo "  cert SANs include $HOST; CA at $SITEPKI/ca.crt"

echo
echo "==> start api + scheduler + controllers + agent + ingress (:80 and :443)"
printf 'listenAddr: "127.0.0.1:6443"\ndbPath: ".dev/phase8b.db"\n' > .dev/p8b-api.yaml
RK_CONFIG="$PWD/.dev/p8b-api.yaml" ./target/release/rk-api > .dev/p8b-api.log 2>&1 &
RK_CONFIG="$PWD/config/scheduler.yaml"   ./target/release/rk-scheduler   > .dev/p8b-sched.log 2>&1 &
RK_CONFIG="$PWD/config/controllers.yaml" ./target/release/rk-controllers > .dev/p8b-ctrl.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"
printf 'apiUrl: "%s"\nnodeName: rk-node1\ndataDir: "%s"\npodCidr: "10.42.0.0/24"\nnodeIp: "127.0.0.1"\nheartbeatIntervalSecs: 3\n' "$API_URL" "$D1" > .dev/p8b-node1.yaml
./target/release/rk-agent --config "$PWD/.dev/p8b-node1.yaml" > .dev/p8b-node1.log 2>&1 &
./target/release/rk-ingress --api-url "$API_URL" --http-port 80 --https-port 443 > .dev/p8b-ingress.log 2>&1 &
wait_for "node Ready" 30 sh -c "'$CTL' get nodes | grep -q Ready"

echo
echo "==> deploy nginx + Service + a TLS Secret + an Ingress (tls: $HOST)"
CRT_B64=$(base64 -w0 "$SITEPKI/server.crt"); KEY_B64=$(base64 -w0 "$SITEPKI/server.key")
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Secret
metadata: { name: shop-tls }
data:
  tls.crt: "$CRT_B64"
  tls.key: "$KEY_B64"
---
apiVersion: v1
kind: Deployment
metadata: { name: shop, labels: { app: shop } }
spec:
  replicas: 2
  selector: { matchLabels: { app: shop } }
  template:
    metadata: { labels: { app: shop } }
    spec:
      nodeName: rk-node1
      containers: [ { name: web, image: nginx:alpine, resources: { requests: { cpu: 50m, memory: 16Mi } } } ]
---
apiVersion: v1
kind: Service
metadata: { name: shop }
spec: { selector: { app: shop }, ports: [ { port: 80, targetPort: 80 } ] }
---
apiVersion: v1
kind: Ingress
metadata: { name: shop }
spec:
  tls:
    - hosts: [ $HOST ]
      secretName: shop-tls
  rules:
    - host: $HOST
      http:
        paths:
          - path: /
            pathType: Prefix
            backend: { service: { name: shop, port: 80 } }
EOF
wait_for "2 shop pods running" 180 sh -c "[ \$($CTL get pods | grep -c '^shop.* Running ') -eq 2 ]"
wait_for "ingress to route on :80" 30 sh -c "curl -fsS -H 'Host: $HOST' http://127.0.0.1/ -o /dev/null"

echo
echo "==> A. TLS termination on :443"
RESOLVE="--resolve $HOST:443:127.0.0.1"
wait_for "the ingress to serve TLS" 30 sh -c "curl -fsS $RESOLVE --cacert '$SITEPKI/ca.crt' https://$HOST/ -o /dev/null"
BODY=$(curl -fsS $RESOLVE --cacert "$SITEPKI/ca.crt" "https://$HOST/")
printf '%s\n' "$BODY" | grep -qi "nginx" || { echo "TLS request did not reach the nginx backend"; printf '%s\n' "$BODY" | head; exit 1; }
echo "  https://$HOST/ -> 200, reached nginx over TLS"
# curl -v's TLS facts go to stderr; capture fully (no early-closed pipe) first.
TLSINFO=$(curl -sv $RESOLVE --cacert "$SITEPKI/ca.crt" "https://$HOST/" -o /dev/null 2>&1 || true)
printf '%s\n' "$TLSINFO" | grep -m1 "SSL connection using" | sed 's/^\* */  /' || true
printf '%s\n' "$TLSINFO" | grep -m1 "subjectAltName" | sed 's/^\* */  /' || true
if curl -fsS $RESOLVE "https://$HOST/" >/dev/null 2>&1; then
    echo "  cert accepted without the CA — unexpected"; exit 1
else
    echo "  https without the CA: refused (real cert, validated)"
fi
HITS=$(curl -fsS -H "Host: $HOST" http://127.0.0.1/ | grep -ci nginx || true)
echo "  http://$HOST/ (cleartext :80) still served (nginx markers: $HITS)"

echo
echo "==> B. drain the node"
"$CTL" get nodes
"$CTL" drain rk-node1
echo "  nodes after drain:"; "$CTL" get nodes
wait_for "evicted pods to come back as Pending (node cordoned)" 30 \
    sh -c "$CTL get pods | grep -q '^shop.* Pending'"
echo "  pods (recreated, but unschedulable while cordoned):"
"$CTL" get pods | grep -E "NAME|shop"

echo
echo "==> uncordon — the pods schedule again"
"$CTL" uncordon rk-node1
wait_for "pods running again after uncordon" 180 sh -c "[ \$($CTL get pods | grep -c '^shop.* Running ') -ge 1 ]"
"$CTL" get nodes
"$CTL" get pods | grep -E "NAME|shop"

echo
echo "==> teardown"
"$CTL" delete ingress shop >/dev/null
"$CTL" delete service shop >/dev/null
"$CTL" delete deployment shop >/dev/null
"$CTL" delete secret shop-tls >/dev/null

echo
echo "phase8b-demo: OK"
