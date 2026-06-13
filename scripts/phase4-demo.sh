#!/usr/bin/env bash
# Phase 4 demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# A two-node cluster on one machine: node "rk-node1" is the host itself,
# node "rk-node2" is a second agent in its own network namespace (the spec's
# dev-mode simulation). The scheduler then places pods by free resources
# (bin packing) and nodeSelector labels, and parks ones that fit nowhere.
# Each node really runs its pods, in its own pod /24.
#
#   node1: host netns, /var/lib/rustkube,        10.42.0.0/24, disk=ssd, 2cpu/1Gi
#   node2: netns rk-node2, /var/lib/rustkube-node2, 10.42.1.0/24, disk=hdd, 1cpu/512Mi
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube
D2=/var/lib/rustkube-node2
NS=rk-node2
NS_IP=10.200.0.2

[[ $EUID -eq 0 ]] || { echo "phase4-demo: run as root" >&2; exit 1; }

# ---- cleanup any previous run -------------------------------------------------
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
    for l in $(ip -brief link | awk '/^rk/{print $1}' | cut -d@ -f1); do ip link del "$l" 2>/dev/null || true; done
    for cg in /sys/fs/cgroup/rustkube/*/; do rmdir "$cg" 2>/dev/null || true; done
}
trap cleanup EXIT
cleanup
if curl -fsS "$API_URL/healthz" >/dev/null 2>&1; then
    echo "phase4-demo: $API_URL still serving after cleanup; aborting" >&2
    exit 1
fi

wait_for() { # <description> <timeout-secs> <command...>
    local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    echo "phase4-demo: timed out waiting for $what" >&2
    return 1
}
pod_field() { "$CTL" get pod "$1" -o json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)$2)"; }
pod_node()  { pod_field "$1" "['spec'].get('nodeName','<none>')"; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> control plane: rk-api (0.0.0.0:6443) + rk-scheduler"
mkdir -p .dev
rm -f .dev/phase4.db*
printf 'listenAddr: "0.0.0.0:6443"\ndbPath: ".dev/phase4.db"\n' > .dev/p4-api.yaml
RK_CONFIG="$PWD/.dev/p4-api.yaml" ./target/release/rk-api > .dev/p4-api.log 2>&1 &
RK_CONFIG="$PWD/config/scheduler.yaml" ./target/release/rk-scheduler > .dev/p4-sched.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"

echo "==> node1 = the host (disk=ssd, 2cpu/1Gi)"
cat > .dev/p4-node1.yaml <<EOF
apiUrl: "$API_URL"
nodeName: rk-node1
dataDir: "$D1"
podCidr: "10.42.0.0/24"
nodeLabels: { disk: ssd }
capacityCpu: "2"
capacityMemory: "1Gi"
EOF
RK_CONFIG="$PWD/.dev/p4-node1.yaml" ./target/release/rk-agent > .dev/p4-node1.log 2>&1 &

echo "==> node2 = a second agent in netns $NS (disk=hdd, 1cpu/512Mi)"
./scripts/sim-node.sh up "$NS" "$NS_IP"
cat > .dev/p4-node2.yaml <<EOF
apiUrl: "http://10.200.0.1:6443"
nodeName: rk-node2
dataDir: "$D2"
podCidr: "10.42.1.0/24"
nodeLabels: { disk: hdd }
capacityCpu: "1"
capacityMemory: "512Mi"
EOF
# `ip netns exec` runs in a fresh mount namespace with /sys re-mounted, which
# shadows the cgroup2 hierarchy at /sys/fs/cgroup. Re-mount cgroup2 (one
# global hierarchy — there's no cgroup namespace here) so the agent's cgroup
# limits work, then exec the agent into node2's network namespace.
ip netns exec "$NS" sh -c \
  "mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true; \
   exec env RK_CONFIG='$PWD/.dev/p4-node2.yaml' ./target/release/rk-agent" \
  > .dev/p4-node2.log 2>&1 &

echo "==> both nodes register Ready"
wait_for "rk-node1 Ready" 30 sh -c "'$CTL' get nodes | grep -q '^rk-node1  *Ready'"
wait_for "rk-node2 Ready" 30 sh -c "'$CTL' get nodes | grep -q '^rk-node2  *Ready'"
"$CTL" get nodes

echo
echo "==> SCENARIO A — resource bin packing (3 pods, 500m/256Mi each, no selector)"
for n in 1 2 3; do
cat <<EOF | "$CTL" apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: pack-$n }
spec:
  containers:
    - name: app
      image: busybox:stable
      command: ["sleep", "3600"]
      resources: { requests: { cpu: 500m, memory: 256Mi } }
EOF
done
# node2 (1cpu/512Mi) fills first under most-allocated: pack-1,pack-2 there; pack-3 overflows to node1.
wait_for "all pack-* scheduled" 30 sh -c "[ \$('$CTL' get pods | grep -c '^pack-.*rk-node') -eq 3 ]"
echo "placement:"
for n in 1 2 3; do echo "  pack-$n -> $(pod_node pack-$n)"; done
[ "$(pod_node pack-1)" = "rk-node2" ] && [ "$(pod_node pack-2)" = "rk-node2" ] && [ "$(pod_node pack-3)" = "rk-node1" ] \
    || { echo "unexpected packing"; "$CTL" get pods; exit 1; }
echo "bin packing OK: node2 filled first (2 pods), overflow to node1 (1 pod)"

echo
echo "==> SCENARIO B — nodeSelector disk=ssd steers nginx to node1, which serves it"
cat <<EOF | "$CTL" apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: web, labels: { app: web } }
spec:
  nodeSelector: { disk: ssd }
  containers:
    - name: nginx
      image: nginx:1.27
      resources: { requests: { cpu: 100m, memory: 64Mi } }
EOF
wait_for "web bound to rk-node1" 20 sh -c "[ \"\$('$CTL' get pod web -o json | python3 -c 'import json,sys;print(json.load(sys.stdin)[\"spec\"].get(\"nodeName\"))')\" = rk-node1 ]"
wait_for "web Running" 120 sh -c "'$CTL' get pods | grep -q '^web  *Running'"
WEB_IP=$(pod_field web "['status']['podIP']")
echo "web -> rk-node1, podIP $WEB_IP (node1's 10.42.0.0/24)"
[[ $WEB_IP == 10.42.0.* ]] || { echo "web IP not in node1 CIDR"; exit 1; }
curl -fsS "http://$WEB_IP/" | grep -o "<title>.*</title>"

echo
echo "==> SCENARIO C — a pod too big for any node stays Pending with a reason"
cat <<EOF | "$CTL" apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: huge }
spec:
  containers:
    - name: app
      image: busybox:stable
      resources: { requests: { cpu: 8, memory: 256Mi } }
EOF
wait_for "huge marked unschedulable" 20 sh -c "'$CTL' get pod huge -o json | grep -q 'Insufficient cpu'"
echo "huge: node=$(pod_node huge), message=$(pod_field huge "['status'].get('message','')")"
[ "$(pod_node huge)" = "<none>" ] || { echo "huge should be unscheduled"; exit 1; }

echo
echo "==> node2 really ran its pods (status reported by node2's agent, IPs in 10.42.1.0/24)"
wait_for "pack-1 Running on node2" 120 sh -c "'$CTL' get pods | grep -q '^pack-1  *Running'"
P1_IP=$(pod_field pack-1 "['status']['podIP']")
echo "pack-1 -> rk-node2, podIP $P1_IP"
[[ $P1_IP == 10.42.1.* ]] || { echo "pack-1 IP not in node2 CIDR"; exit 1; }
ip netns exec "$NS" ping -c1 -W2 "$P1_IP" >/dev/null && echo "node2 reaches its pod $P1_IP"

echo
echo "==> final cluster view"
"$CTL" get nodes
"$CTL" get pods

echo
echo "==> teardown"
for p in pack-1 pack-2 pack-3 web huge; do "$CTL" delete pod "$p" >/dev/null; done
wait_for "containers gone on both nodes" 40 sh -c "! ls -d $D1/containers/p* $D2/containers/p* 2>/dev/null | grep -q ."
"$CTL" get pods | grep -qx 'No pods found'

echo "phase4-demo: OK"
