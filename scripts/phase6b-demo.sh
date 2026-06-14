#!/usr/bin/env bash
# Phase 6B demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Services + DNS: a Deployment of whoami pods (spread across both nodes) is
# fronted by a ClusterIP Service. From inside a client pod we resolve the
# service name through the embedded DNS and curl its VIP repeatedly, watching
# nftables load-balance across backends on both nodes.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube
D2=/var/lib/rustkube-node2
NS=rk-node2

[[ $EUID -eq 0 ]] || { echo "phase6b-demo: run as root" >&2; exit 1; }
command -v nsenter >/dev/null || { echo "needs nsenter (util-linux)" >&2; exit 1; }

cleanup() {
    for p in rk-agent rk-scheduler rk-controllers rk-api; do pkill -9 -f "target/release/$p" 2>/dev/null || true; done
    for dir in "$D1" "$D2"; do
        for pidfile in "$dir"/containers/p*/pid1; do [[ -f $pidfile ]] && kill -9 "$(cat "$pidfile")" 2>/dev/null || true; done
    done
    pkill -9 -f "shim $D1" 2>/dev/null || true; pkill -9 -f "shim $D2" 2>/dev/null || true
    sleep 0.5
    ./scripts/sim-node.sh down "$NS" >/dev/null 2>&1 || true
    for dir in "$D1" "$D2"; do
        for c in "$dir"/containers/p*; do [[ -d $c ]] && umount -l "$c/rootfs" 2>/dev/null || true; done
        rm -rf "$dir"/containers/p* "$dir"/net/ipam.json 2>/dev/null || true
    done
    ip link del rk-br0 2>/dev/null || true; ip link del rk-vxlan0 2>/dev/null || true
    for l in $(ip -brief link | awk '/^rk/{print $1}' | cut -d@ -f1); do ip link del "$l" 2>/dev/null || true; done
    for cg in /sys/fs/cgroup/rustkube/*/; do rmdir "$cg" 2>/dev/null || true; done
    nft delete table ip rk-services 2>/dev/null || true
}
trap cleanup EXIT
cleanup

wait_for() { local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
    echo "phase6b-demo: timed out waiting for $what" >&2; return 1; }
pod_pid() { local uid12; uid12=$("$CTL" get pod "$2" -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['metadata']['uid'][:12])"); cat "$1"/containers/p"${uid12}"-*/pid1; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> control plane: api + scheduler + controllers"
mkdir -p .dev; rm -f .dev/phase6b.db*
printf 'listenAddr: "0.0.0.0:6443"\ndbPath: ".dev/phase6b.db"\n' > .dev/p6b-api.yaml
RK_CONFIG="$PWD/.dev/p6b-api.yaml" ./target/release/rk-api > .dev/p6b-api.log 2>&1 &
RK_CONFIG="$PWD/config/scheduler.yaml"   ./target/release/rk-scheduler   > .dev/p6b-sched.log 2>&1 &
RK_CONFIG="$PWD/config/controllers.yaml" ./target/release/rk-controllers > .dev/p6b-ctrl.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"

echo "==> two node agents"
# Capacity overrides (as in Phase 4/5) so the 4 whoami pods spread across
# both nodes — exercising service routing to backends over the VXLAN overlay.
cat > .dev/p6b-node1.yaml <<EOF
apiUrl: "$API_URL"
nodeName: rk-node1
dataDir: "$D1"
podCidr: "10.42.0.0/24"
nodeIp: "10.200.0.1"
capacityCpu: "2"
capacityMemory: "1Gi"
heartbeatIntervalSecs: 3
EOF
./scripts/sim-node.sh up "$NS" "10.200.0.2"
./target/release/rk-agent --config "$PWD/.dev/p6b-node1.yaml" > .dev/p6b-node1.log 2>&1 &
cat > .dev/p6b-node2.yaml <<EOF
apiUrl: "http://10.200.0.1:6443"
nodeName: rk-node2
dataDir: "$D2"
podCidr: "10.42.1.0/24"
nodeIp: "10.200.0.2"
capacityCpu: "1"
capacityMemory: "512Mi"
heartbeatIntervalSecs: 3
EOF
ip netns exec "$NS" sh -c "mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true; exec ./target/release/rk-agent --config '$PWD/.dev/p6b-node2.yaml'" > .dev/p6b-node2.log 2>&1 &
wait_for "both nodes Ready" 30 sh -c "[ \$('$CTL' get nodes | grep -c Ready) -eq 2 ]"

echo
echo "==> Deployment of 4 whoami pods + a ClusterIP Service"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Deployment
metadata: { name: whoami, labels: { app: whoami } }
spec:
  replicas: 4
  selector: { matchLabels: { app: whoami } }
  template:
    metadata: { labels: { app: whoami } }
    spec:
      containers: [ { name: whoami, image: traefik/whoami, resources: { requests: { cpu: 300m, memory: 64Mi } } } ]
---
apiVersion: v1
kind: Service
metadata: { name: whoami }
spec:
  selector: { app: whoami }
  ports: [ { port: 80, targetPort: 80 } ]
EOF
wait_for "4 whoami pods running" 180 sh -c "[ \$($CTL get pods | grep -c '^whoami.* Running ') -eq 4 ]"
CIP=$("$CTL" get service whoami -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['spec']['clusterIP'])")
echo "service whoami got ClusterIP $CIP; pods:"
"$CTL" get pods -o wide 2>/dev/null || "$CTL" get pods
"$CTL" get service whoami

echo
echo "==> nftables service rules on node1 (programmed by the agent's data plane)"
wait_for "service rules to appear" 30 sh -c "nft list table ip rk-services 2>/dev/null | grep -q $CIP"
nft list table ip rk-services 2>/dev/null | grep -E "daddr $CIP|dnat" | head || true

echo
echo "==> a busybox client pod on node1 to query from inside the cluster"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: client }
spec:
  nodeName: rk-node1
  containers: [ { name: shell, image: busybox:stable, command: ["sleep","3600"] } ]
EOF
wait_for "client running" 120 sh -c "'$CTL' get pods | grep -q '^client  *Running'"
CLIENT=$(pod_pid "$D1" client)

# Enter the pod's net+mount+uts+ipc+pid namespaces (its resolv.conf + busybox).
ENTER="nsenter -t $CLIENT -n -m -u -i -p --"
echo "==> DNS: resolve the service name from inside the client pod"
$ENTER nslookup whoami.default.svc.cluster.local 2>/dev/null | tail -3
echo "    (the pod's /etc/resolv.conf points at the node gateway's embedded DNS)"

echo
echo "==> ClusterIP load-balancing: curl http://whoami/ ten times from the client pod"
echo "    (bare name 'whoami' -> DNS search -> $CIP -> nftables DNAT to a backend)"
> .dev/p6b-hits.txt
for i in $(seq 1 10); do
    $ENTER wget -q -T 5 -O- "http://whoami/" 2>/dev/null | awk -F': ' '/^Hostname/{print $2}' >> .dev/p6b-hits.txt || true
done
echo "backends that served the requests:"
sort .dev/p6b-hits.txt | uniq -c
DISTINCT=$(sort -u .dev/p6b-hits.txt | grep -c . || true)
[ "$DISTINCT" -ge 2 ] || { echo "expected load-balancing across >=2 backends, got $DISTINCT"; exit 1; }
echo "    load-balanced across $DISTINCT distinct backends (DNS name -> VIP -> nftables DNAT)"

echo
echo "==> teardown"
"$CTL" delete deployment whoami >/dev/null
"$CTL" delete service whoami >/dev/null
"$CTL" delete pod client >/dev/null
wait_for "service nft rules cleared" 20 sh -c "! nft list table ip rk-services 2>/dev/null | grep -q $CIP"

echo
echo "phase6b-demo: OK"
