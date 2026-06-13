#!/usr/bin/env bash
# Phase 5 demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# The full control loop: api + scheduler + controller-manager + two node
# agents (node1 = host, node2 = netns, as in Phase 4). A Deployment flows all
# the way down — Deployment -> ReplicaSet -> Pods -> scheduled -> running —
# and we exercise self-healing, scale, a rolling update, node-death
# rescheduling, and cascade delete.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube
D2=/var/lib/rustkube-node2
NS=rk-node2
NS_IP=10.200.0.2

[[ $EUID -eq 0 ]] || { echo "phase5-demo: run as root" >&2; exit 1; }

cleanup() {
    pkill -9 -f "target/release/rk-agent" 2>/dev/null || true
    pkill -9 -f "target/release/rk-scheduler" 2>/dev/null || true
    pkill -9 -f "target/release/rk-controllers" 2>/dev/null || true
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

wait_for() { # <description> <timeout-secs> <command...>
    local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    echo "phase5-demo: timed out waiting for $what" >&2
    "$CTL" get deployments 2>/dev/null || true
    "$CTL" get rs 2>/dev/null || true
    "$CTL" get pods 2>/dev/null || true
    return 1
}
running_pods() { "$CTL" get pods 2>/dev/null | grep -c " Running " || true; }
jget() { "$CTL" get "$1" "$2" -o json 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin)$3)"; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> control plane: api + scheduler + controllers (fast node-death grace for the demo)"
mkdir -p .dev
rm -f .dev/phase5.db*
printf 'listenAddr: "0.0.0.0:6443"\ndbPath: ".dev/phase5.db"\n' > .dev/p5-api.yaml
printf 'apiUrl: "%s"\nnodeNotReadySecs: 8\npodEvictionSecs: 15\nresyncIntervalSecs: 3\n' "$API_URL" > .dev/p5-ctrl.yaml
RK_CONFIG="$PWD/.dev/p5-api.yaml"  ./target/release/rk-api         > .dev/p5-api.log 2>&1 &
RK_CONFIG="$PWD/config/scheduler.yaml" ./target/release/rk-scheduler > .dev/p5-sched.log 2>&1 &
RK_CONFIG="$PWD/.dev/p5-ctrl.yaml" ./target/release/rk-controllers  > .dev/p5-ctrl.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"

echo "==> node1 (host, 2cpu/1Gi) + node2 (netns, 1cpu/512Mi)"
# heartbeatIntervalSecs (3) must stay well under the controller's
# nodeNotReadySecs (8) or healthy nodes flap NotReady between beats.
cat > .dev/p5-node1.yaml <<EOF
apiUrl: "$API_URL"
nodeName: rk-node1
dataDir: "$D1"
podCidr: "10.42.0.0/24"
capacityCpu: "2"
capacityMemory: "1Gi"
heartbeatIntervalSecs: 3
EOF
./target/release/rk-agent --config "$PWD/.dev/p5-node1.yaml" > .dev/p5-node1.log 2>&1 &
./scripts/sim-node.sh up "$NS" "$NS_IP"
cat > .dev/p5-node2.yaml <<EOF
apiUrl: "http://10.200.0.1:6443"
nodeName: rk-node2
dataDir: "$D2"
podCidr: "10.42.1.0/24"
capacityCpu: "1"
capacityMemory: "512Mi"
heartbeatIntervalSecs: 3
EOF
ip netns exec "$NS" sh -c \
  "mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true; \
   exec ./target/release/rk-agent --config '$PWD/.dev/p5-node2.yaml'" \
  > .dev/p5-node2.log 2>&1 &
wait_for "both nodes Ready" 30 sh -c "[ \$('$CTL' get nodes | grep -c Ready) -eq 2 ]"
"$CTL" get nodes

echo
echo "==> apply a Deployment (replicas=3): Deployment -> ReplicaSet -> Pods -> running"
cat > .dev/p5-web.yaml <<EOF
apiVersion: v1
kind: Deployment
metadata: { name: web, labels: { app: web } }
spec:
  replicas: 3
  selector: { matchLabels: { app: web } }
  strategy: { type: RollingUpdate, rollingUpdate: { maxUnavailable: 1, maxSurge: 1 } }
  template:
    metadata: { labels: { app: web } }
    spec:
      containers:
        - name: nginx
          image: nginx:1.27
          env: [ { name: COLOR, value: blue } ]
          resources: { requests: { cpu: 300m, memory: 200Mi } }
EOF
"$CTL" apply -f .dev/p5-web.yaml
wait_for "3 pods running" 150 sh -c "[ \$($CTL get pods | grep -c ' Running ') -eq 3 ]"
"$CTL" get deployments
"$CTL" get rs
"$CTL" get pods

echo
echo "==> self-healing: delete a pod, the ReplicaSet recreates it"
VICTIM=$("$CTL" get pods | awk '/ Running /{print $1; exit}')
"$CTL" delete pod "$VICTIM"
wait_for "the deleted pod to be replaced (3 running again)" 150 sh -c "[ \$($CTL get pods | grep -c ' Running ') -eq 3 ]"
echo "replaced $VICTIM; still 3 running"
"$CTL" get pods | grep -v "$VICTIM" | head -5 || true

echo
echo "==> scale up to 5"
"$CTL" scale deployment web --replicas=5
wait_for "5 pods running" 150 sh -c "[ \$($CTL get pods | grep -c ' Running ') -eq 5 ]"
[ "$(jget deployment web "['status']['readyReplicas']")" = "5" ]
echo "deployment ready: $(jget deployment web "['status']['readyReplicas']")/5"

echo "==> scale down to 2"
"$CTL" scale deployment web --replicas=2
wait_for "exactly 2 pods" 60 sh -c "[ \$($CTL get pods | grep -c '^web-') -eq 2 ]"
"$CTL" get pods

echo
echo "==> rolling update: change the pod template (COLOR blue->green) at replicas=2"
echo "    (an image change rolls out identically; an env change avoids a re-pull)"
# Keep replicas at 2 so this is a pure template roll, not a re-scale.
sed -e 's/value: blue/value: green/' -e 's/replicas: 3/replicas: 2/' .dev/p5-web.yaml | "$CTL" apply -f -
# A second ReplicaSet appears; the rollout converges to it with the old at 0.
wait_for "two ReplicaSets (rollout started)" 60 sh -c "[ \$($CTL get rs | grep -c '^web-') -ge 2 ]"
# Complete when the deployment reports every replica updated and ready, and
# exactly one ReplicaSet is still non-empty.
wait_for "rollout complete (all replicas on the new revision, old RS at 0)" 150 sh -c "
  $CTL get deployment web -o json | python3 -c 'import json,sys; d=json.load(sys.stdin); s=d[\"status\"]; r=d[\"spec\"][\"replicas\"]; sys.exit(0 if s.get(\"readyReplicas\")==r and s.get(\"updatedReplicas\")==r else 1)' &&
  [ \$($CTL get rs | awk '\$1!=\"NAME\" && \$2+0>0' | wc -l) -eq 1 ]"
"$CTL" get rs
"$CTL" get pods
# Confirm the live pods carry the new template.
RUNNING=$("$CTL" get pods | awk '/ Running /{print $1; exit}')
[ "$(jget pod "$RUNNING" "['spec']['containers'][0]['env'][0]['value']")" = "green" ] && echo "live pods run the new revision (COLOR=green)"

echo
echo "==> node death: stop node2's agent; controllers mark it NotReady and reschedule its pods"
N2_PODS_BEFORE=$("$CTL" get pods | awk '$5=="rk-node2"' | wc -l)
echo "pods on rk-node2 before: $N2_PODS_BEFORE"
# Kill everything in node2's namespace (agent + its container shims) — the
# node is now truly dead, so its heartbeats stop for good.
ip netns pids "$NS" 2>/dev/null | xargs -r kill -9 || true
wait_for "rk-node2 marked NotReady" 40 sh -c "$CTL get nodes | grep -q 'rk-node2  *NotReady'"
echo "rk-node2 is NotReady; waiting for its pods to be evicted and rescheduled..."
wait_for "2 pods running again, none on rk-node2" 150 sh -c "
  [ \$($CTL get pods | grep -c ' Running ') -eq 2 ] && [ \$($CTL get pods | awk '\$5==\"rk-node2\"' | wc -l) -eq 0 ]"
"$CTL" get nodes
"$CTL" get pods
echo "all replicas rescheduled onto the live node"

echo
echo "==> cascade delete: delete the Deployment, GC collects ReplicaSets then Pods"
"$CTL" delete deployment web
wait_for "ReplicaSets and Pods garbage-collected" 60 sh -c "
  ! $CTL get rs | grep -q '^web-' && ! $CTL get pods | grep -q '^web-'"
"$CTL" get deployments
"$CTL" get rs
"$CTL" get pods

echo
echo "phase5-demo: OK"
