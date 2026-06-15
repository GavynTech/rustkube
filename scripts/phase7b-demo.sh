#!/usr/bin/env bash
# Phase 7B demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Ingress: rk-ingress watches Ingress + Service objects and reverse-proxies
# external HTTP (port 80) by host/path to a Service's ClusterIP, where the
# node's nftables rules load-balance onto the backing pods. Single node is
# enough.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube

[[ $EUID -eq 0 ]] || { echo "phase7b-demo: run as root" >&2; exit 1; }

cleanup() {
    for p in rk-ingress rk-agent rk-scheduler rk-controllers rk-api; do pkill -9 -f "target/release/$p" 2>/dev/null || true; done
    for pidfile in "$D1"/containers/p*/pid1; do [[ -f $pidfile ]] && kill -9 "$(cat "$pidfile")" 2>/dev/null || true; done
    pkill -9 -f "shim $D1" 2>/dev/null || true
    sleep 0.5
    for c in "$D1"/containers/p*; do [[ -d $c ]] && umount -l "$c/rootfs" 2>/dev/null || true; done
    rm -rf "$D1"/containers/p* "$D1"/net/ipam.json 2>/dev/null || true
    ip link del rk-br0 2>/dev/null || true; ip link del rk-vxlan0 2>/dev/null || true
    for l in $(ip -brief link | awk '/^rk/{print $1}' | cut -d@ -f1); do ip link del "$l" 2>/dev/null || true; done
    for cg in /sys/fs/cgroup/rustkube/*/; do rmdir "$cg" 2>/dev/null || true; done
    nft delete table ip rk-services 2>/dev/null || true
}
trap cleanup EXIT
cleanup

wait_for() { local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
    echo "phase7b-demo: timed out waiting for $what" >&2; "$CTL" get pods 2>/dev/null || true; return 1; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> api + scheduler + controllers + agent + ingress"
mkdir -p .dev; rm -f .dev/phase7b.db*
printf 'listenAddr: "127.0.0.1:6443"\ndbPath: ".dev/phase7b.db"\n' > .dev/p7b-api.yaml
RK_CONFIG="$PWD/.dev/p7b-api.yaml" ./target/release/rk-api > .dev/p7b-api.log 2>&1 &
RK_CONFIG="$PWD/config/scheduler.yaml"   ./target/release/rk-scheduler   > .dev/p7b-sched.log 2>&1 &
RK_CONFIG="$PWD/config/controllers.yaml" ./target/release/rk-controllers > .dev/p7b-ctrl.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"
printf 'apiUrl: "%s"\nnodeName: rk-node1\ndataDir: "%s"\npodCidr: "10.42.0.0/24"\nnodeIp: "127.0.0.1"\nheartbeatIntervalSecs: 3\n' "$API_URL" "$D1" > .dev/p7b-node1.yaml
./target/release/rk-agent --config "$PWD/.dev/p7b-node1.yaml" > .dev/p7b-node1.log 2>&1 &
./target/release/rk-ingress --api-url "$API_URL" --http-port 80 > .dev/p7b-ingress.log 2>&1 &
wait_for "node Ready" 30 sh -c "'$CTL' get nodes | grep -q Ready"

echo
echo "==> a whoami Deployment, a Service, and an Ingress (host app.example.com)"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Deployment
metadata: { name: whoami, labels: { app: whoami } }
spec:
  replicas: 2
  selector: { matchLabels: { app: whoami } }
  template:
    metadata: { labels: { app: whoami } }
    spec:
      nodeName: rk-node1
      containers: [ { name: whoami, image: traefik/whoami, resources: { requests: { cpu: 50m, memory: 16Mi } } } ]
---
apiVersion: v1
kind: Service
metadata: { name: whoami }
spec: { selector: { app: whoami }, ports: [ { port: 80, targetPort: 80 } ] }
---
apiVersion: v1
kind: Ingress
metadata: { name: whoami }
spec:
  rules:
    - host: app.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend: { service: { name: whoami, port: 80 } }
EOF
wait_for "2 whoami pods running" 180 sh -c "[ \$($CTL get pods | grep -c '^whoami.* Running ') -eq 2 ]"
CIP=$("$CTL" get service whoami -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['spec']['clusterIP'])")
echo "service ClusterIP $CIP; ingress:"
"$CTL" get ingress whoami -o yaml | grep -E "name:|host:|path:|port:" | head
# Let the ingress build its route + the agent program nft endpoints.
wait_for "ingress to route app.example.com" 30 sh -c "curl -fsS -H 'Host: app.example.com' http://127.0.0.1/ -o /dev/null"

echo
echo "==> curl the ingress with the matching Host header (5 times → load-balanced)"
> .dev/p7b-hits.txt
for i in $(seq 1 5); do
    curl -fsS -H "Host: app.example.com" http://127.0.0.1/ 2>/dev/null | awk -F': ' '/^Hostname/{print $2}' >> .dev/p7b-hits.txt || true
done
echo "backends reached through the ingress:"
sort .dev/p7b-hits.txt | uniq -c
[ "$(sort -u .dev/p7b-hits.txt | grep -c .)" -ge 1 ] || { echo "ingress did not reach a backend"; exit 1; }

echo
echo "==> a request for an unknown host gets 404 from the ingress"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: nope.example.com" http://127.0.0.1/)
echo "  Host: nope.example.com -> HTTP $CODE"
[ "$CODE" = "404" ] || { echo "expected 404 for unmatched host"; exit 1; }

echo
echo "==> teardown"
"$CTL" delete ingress whoami >/dev/null
"$CTL" delete service whoami >/dev/null
"$CTL" delete deployment whoami >/dev/null

echo
echo "phase7b-demo: OK"
