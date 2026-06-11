#!/usr/bin/env bash
# Phase 3 demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Control plane + node agent, end to end: the agent registers the node and
# heartbeats; an applied pod (bound via spec.nodeName — the scheduler is
# Phase 4) is pulled and started in real namespaces/cgroups; status flows
# back (Running, podIP, restartCount); killing the process self-heals it;
# a restartPolicy:Never pod runs to Succeeded; deletion tears the
# container down. Exits non-zero if any step misbehaves.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
NODE=$(hostname)
DATA_DIR=/var/lib/rustkube

if [[ $EUID -ne 0 ]]; then
    echo "phase3-demo: run as root (the agent drives namespaces, cgroups, mounts)" >&2
    exit 1
fi
if curl -fsS "$API_URL/healthz" > /dev/null 2>&1; then
    echo "phase3-demo: something already listens on $API_URL — stop it first" >&2
    exit 1
fi

# An aborted previous run may have left agent-managed containers behind
# (the agent died before tearing them down). Kill each container's PID 1 —
# that collapses its whole PID namespace (workers included) and releases its
# netns/veth — then the shims, then unmount and clear state.
for pidfile in "$DATA_DIR"/containers/p*/pid1; do
    [ -f "$pidfile" ] && kill -9 "$(cat "$pidfile")" 2>/dev/null || true
done
pkill -9 -f "shim $DATA_DIR" 2>/dev/null || true
sleep 0.5
for dir in "$DATA_DIR"/containers/p*; do
    [ -d "$dir" ] || continue
    umount -l "$dir/rootfs" 2>/dev/null || true
    rm -rf "$dir"
done
for cg in /sys/fs/cgroup/rustkube/*/; do rmdir "$cg" 2>/dev/null || true; done
rm -f "$DATA_DIR/net/ipam.json"

wait_for() { # <description> <timeout-secs> <command...>
    local what=$1 timeout=$2; shift 2
    for _ in $(seq 1 $((timeout * 2))); do
        if "$@" > /dev/null 2>&1; then return 0; fi
        sleep 0.5
    done
    echo "phase3-demo: timed out waiting for $what" >&2
    return 1
}

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> start rk-api + rk-agent"
mkdir -p .dev
rm -f .dev/phase3.db*
printf 'listenAddr: "127.0.0.1:6443"\ndbPath: ".dev/phase3.db"\n' > .dev/phase3-api.yaml
printf 'apiUrl: "%s"\ndataDir: "%s"\n' "$API_URL" "$DATA_DIR" > .dev/phase3-agent.yaml
RK_CONFIG=.dev/phase3-api.yaml ./target/release/rk-api  > .dev/phase3-api.log 2>&1 &
API_PID=$!
RK_CONFIG=.dev/phase3-agent.yaml ./target/release/rk-agent > .dev/phase3-agent.log 2>&1 &
AGENT_PID=$!
trap 'kill "$AGENT_PID" "$API_PID" 2>/dev/null || true' EXIT

wait_for "the API server" 20 curl -fsS "$API_URL/healthz"
echo "==> node registers and heartbeats"
wait_for "node $NODE to be Ready" 20 sh -c "'$CTL' get nodes | grep -q '^$NODE  *Ready'"
"$CTL" get nodes

echo "==> apply a configmap and a pod bound to this node"
cat > .dev/phase3-pod.yaml <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: web-config
data:
  MODE: production
---
apiVersion: v1
kind: Pod
metadata:
  name: web-0
  labels: { app: web }
spec:
  nodeName: $NODE
  containers:
    - name: nginx
      image: nginx:1.27
      env:
        - name: MODE
          valueFrom: { configMapKeyRef: { name: web-config, key: MODE } }
      resources:
        limits: { cpu: 500m, memory: 256Mi }
EOF
"$CTL" apply -f .dev/phase3-pod.yaml

echo "==> agent pulls and starts it; status reports Running + podIP"
wait_for "web-0 to be Running" 120 sh -c "'$CTL' get pods | grep -q '^web-0  *Running'"
"$CTL" get pods
POD_IP=$("$CTL" get pod web-0 -o json \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['status']['podIP'])")
PID1=$(cat "$DATA_DIR"/containers/p*-nginx/pid1)

echo "==> curl the pod IP from the VM host"
curl -fsS "http://$POD_IP/" | grep -o "<title>.*</title>"

echo "==> valueFrom env reached the container"
tr '\0' '\n' < "/proc/$PID1/environ" | grep -qx "MODE=production" && echo "MODE=production"

echo "==> self-healing: kill nginx (pid $PID1), watch it come back"
kill -9 "$PID1"
wait_for "restartCount >= 1 and Running again" 60 sh -c \
    "[ \"\$('$CTL' get pods | awk '/^web-0/{print \$3}')\" -ge 1 ] && '$CTL' get pods | grep -q '^web-0  *Running'"
"$CTL" get pods
NEW_PID1=$(cat "$DATA_DIR"/containers/p*-nginx/pid1)
[ "$NEW_PID1" != "$PID1" ]
curl -fsS "http://$POD_IP/" > /dev/null && echo "serving again (new pid1 $NEW_PID1)"

echo "==> a restartPolicy:Never pod runs to completion"
cat > .dev/phase3-once.yaml <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: once
spec:
  nodeName: $NODE
  restartPolicy: Never
  containers:
    - name: task
      image: busybox:stable
      command: ["/bin/true"]
EOF
"$CTL" apply -f .dev/phase3-once.yaml
wait_for "once to Succeed" 120 sh -c "'$CTL' get pods | grep -q '^once  *Succeeded'"
"$CTL" get pods

echo "==> delete: containers are torn down"
"$CTL" delete pod web-0
"$CTL" delete pod once
wait_for "containers to be gone" 30 sh -c "! ls -d '$DATA_DIR'/containers/p* 2>/dev/null | grep -q ."
"$CTL" get pods | grep -qx 'No pods found'
"$CTL" get nodes | grep -q "^$NODE  *Ready"

echo "phase3-demo: OK"
