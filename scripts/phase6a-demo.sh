#!/usr/bin/env bash
# Phase 6A demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Pod networking: a per-node bridge gives same-node pods L2 connectivity, and
# a VXLAN overlay (programmed from the Node list) carries traffic between
# nodes. The headline test: a pod on node1 curls a pod on node2 over the
# tunnel, and vice versa.
#
#   node1: host netns,      10.42.0.0/24, underlay 10.200.0.1
#   node2: netns rk-node2,  10.42.1.0/24, underlay 10.200.0.2
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube
D2=/var/lib/rustkube-node2
NS=rk-node2

[[ $EUID -eq 0 ]] || { echo "phase6a-demo: run as root" >&2; exit 1; }
command -v nsenter >/dev/null || { echo "phase6a-demo: needs nsenter (util-linux)" >&2; exit 1; }

cleanup() {
    pkill -9 -f "target/release/rk-agent" 2>/dev/null || true
    pkill -9 -f "target/release/rk-scheduler" 2>/dev/null || true
    pkill -9 -f "target/release/rk-api" 2>/dev/null || true
    for dir in "$D1" "$D2"; do
        for pidfile in "$dir"/containers/p*/pid1; do
            [[ -f $pidfile ]] && kill -9 "$(cat "$pidfile")" 2>/dev/null || true
        done
    done
    pkill -9 -f "shim $D1" 2>/dev/null || true
    pkill -9 -f "shim $D2" 2>/dev/null || true
    sleep 0.5
    ./scripts/sim-node.sh down "$NS" >/dev/null 2>&1 || true
    for dir in "$D1" "$D2"; do
        for c in "$dir"/containers/p*; do
            [[ -d $c ]] && umount -l "$c/rootfs" 2>/dev/null || true
        done
        rm -rf "$dir"/containers/p* "$dir"/net/ipam.json 2>/dev/null || true
    done
    # Tear down bridge + vxlan in the host netns (node2's go with its netns).
    ip link del rk-br0 2>/dev/null || true
    ip link del rk-vxlan0 2>/dev/null || true
    for l in $(ip -brief link | awk '/^rk/{print $1}' | cut -d@ -f1); do ip link del "$l" 2>/dev/null || true; done
    for cg in /sys/fs/cgroup/rustkube/*/; do rmdir "$cg" 2>/dev/null || true; done
}
trap cleanup EXIT
cleanup

wait_for() { local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done
    echo "phase6a-demo: timed out waiting for $what" >&2; return 1; }
pod_ip()  { "$CTL" get pod "$1" -o json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)['status']['podIP'])"; }
# pid of a pod's container, found by the pod UID (the runtime names the
# container p<uid12>-<container>), so multiple pods per node stay distinct.
pod_pid() { # <data-dir> <pod-name>
    local uid12; uid12=$("$CTL" get pod "$2" -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['metadata']['uid'][:12])")
    cat "$1"/containers/p"${uid12}"-*/pid1
}

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> api + two node agents (node IPs pinned to the underlay link)"
mkdir -p .dev
rm -f .dev/phase6.db*
printf 'listenAddr: "0.0.0.0:6443"\ndbPath: ".dev/phase6.db"\n' > .dev/p6-api.yaml
RK_CONFIG="$PWD/.dev/p6-api.yaml" ./target/release/rk-api > .dev/p6-api.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"

cat > .dev/p6-node1.yaml <<EOF
apiUrl: "$API_URL"
nodeName: rk-node1
dataDir: "$D1"
podCidr: "10.42.0.0/24"
nodeIp: "10.200.0.1"
heartbeatIntervalSecs: 3
EOF
# node1's underlay address (10.200.0.1) only exists once the link to node2 is
# up, so bring up the sim node first.
./scripts/sim-node.sh up "$NS" "10.200.0.2"
./target/release/rk-agent --config "$PWD/.dev/p6-node1.yaml" > .dev/p6-node1.log 2>&1 &

cat > .dev/p6-node2.yaml <<EOF
apiUrl: "http://10.200.0.1:6443"
nodeName: rk-node2
dataDir: "$D2"
podCidr: "10.42.1.0/24"
nodeIp: "10.200.0.2"
heartbeatIntervalSecs: 3
EOF
ip netns exec "$NS" sh -c \
  "mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true; \
   exec ./target/release/rk-agent --config '$PWD/.dev/p6-node2.yaml'" \
  > .dev/p6-node2.log 2>&1 &

wait_for "both nodes Ready" 30 sh -c "[ \$('$CTL' get nodes | grep -c Ready) -eq 2 ]"
echo "VTEP advertisements:"
"$CTL" get node rk-node1 -o json | python3 -c "import json,sys;n=json.load(sys.stdin);print('  rk-node1 mac', n['metadata']['annotations'].get('rustkube.io/vtep-mac'),'addr',n['status']['address'])"
"$CTL" get node rk-node2 -o json | python3 -c "import json,sys;n=json.load(sys.stdin);print('  rk-node2 mac', n['metadata']['annotations'].get('rustkube.io/vtep-mac'),'addr',n['status']['address'])"

echo
echo "==> one nginx pod pinned to each node"
for spec in "web1 rk-node1" "web2 rk-node2"; do
  set -- $spec
cat <<EOF | "$CTL" apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: $1 }
spec:
  nodeName: $2
  containers: [ { name: nginx, image: nginx:1.27 } ]
EOF
done
wait_for "web1 Running" 150 sh -c "'$CTL' get pods | grep -q '^web1  *Running'"
wait_for "web2 Running" 150 sh -c "'$CTL' get pods | grep -q '^web2  *Running'"
"$CTL" get pods
WEB1=$(pod_ip web1); WEB2=$(pod_ip web2)
echo "web1 = $WEB1 (node1's 10.42.0.0/24), web2 = $WEB2 (node2's 10.42.1.0/24)"
[[ $WEB1 == 10.42.0.* && $WEB2 == 10.42.1.* ]] || { echo "unexpected pod IPs"; exit 1; }

echo
echo "==> overlay state on node1"
ip -d link show rk-vxlan0 | sed -n '1,2p'
ip route | grep "10.42.1.0/24" || true
bridge fdb show dev rk-vxlan0 2>/dev/null | grep -v "00:00:00:00:00:00" || true

echo
echo "==> CROSS-NODE: from web1 (node1) curl web2 (node2) over the VXLAN tunnel"
P1=$(pod_pid "$D1" web1)
nsenter -t "$P1" -n curl -fsS --max-time 5 "http://$WEB2/" | grep -o "<title>.*</title>"
echo "    web1 -> web2 OK"

echo "==> CROSS-NODE (reverse): from web2 (node2) curl web1 (node1)"
P2=$(pod_pid "$D2" web2)
nsenter -t "$P2" -n curl -fsS --max-time 5 "http://$WEB1/" | grep -o "<title>.*</title>"
echo "    web2 -> web1 OK"

echo
echo "==> SAME-NODE: a second pod on node1 reaches web1 over the bridge"
cat <<EOF | "$CTL" apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: web1b }
spec:
  nodeName: rk-node1
  containers: [ { name: nginx, image: nginx:1.27 } ]
EOF
wait_for "web1b Running" 60 sh -c "'$CTL' get pods | grep -q '^web1b  *Running'"
# Enter web1b's netns and curl web1 — both on node1's bridge (no tunnel).
PB=$(pod_pid "$D1" web1b)
nsenter -t "$PB" -n curl -fsS --max-time 5 "http://$WEB1/" | grep -o "<title>.*</title>"
echo "    web1b -> web1 (same node, via bridge) OK"

echo
echo "phase6a-demo: OK"
