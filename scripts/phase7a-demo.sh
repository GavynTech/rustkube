#!/usr/bin/env bash
# Phase 7A demo — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Probes + logs (single node is enough). A readiness probe gates whether a
# pod receives service traffic; a liveness probe restarts an unhealthy
# container; and `rk-ctl logs` streams a pod's output from the node.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube

[[ $EUID -eq 0 ]] || { echo "phase7a-demo: run as root" >&2; exit 1; }

cleanup() {
    for p in rk-agent rk-api; do pkill -9 -f "target/release/$p" 2>/dev/null || true; done
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
    echo "phase7a-demo: timed out waiting for $what" >&2; "$CTL" get pods 2>/dev/null || true; return 1; }
pod_ip()    { "$CTL" get pod "$1" -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['status'].get('podIP',''))"; }
pod_ready() { "$CTL" get pod "$1" -o json | python3 -c "import json,sys; cs=json.load(sys.stdin)['status'].get('containerStatuses',[{}]); print(cs[0].get('ready',False))"; }
pod_restarts() { "$CTL" get pod "$1" -o json | python3 -c "import json,sys; cs=json.load(sys.stdin)['status'].get('containerStatuses',[{}]); print(cs[0].get('restartCount',0))"; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> api + a single node agent"
mkdir -p .dev; rm -f .dev/phase7.db*
printf 'listenAddr: "127.0.0.1:6443"\ndbPath: ".dev/phase7.db"\n' > .dev/p7-api.yaml
RK_CONFIG="$PWD/.dev/p7-api.yaml" ./target/release/rk-api > .dev/p7-api.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"
printf 'apiUrl: "%s"\nnodeName: rk-node1\ndataDir: "%s"\npodCidr: "10.42.0.0/24"\nnodeIp: "127.0.0.1"\nheartbeatIntervalSecs: 3\n' "$API_URL" "$D1" > .dev/p7-node1.yaml
./target/release/rk-agent --config "$PWD/.dev/p7-node1.yaml" > .dev/p7-node1.log 2>&1 &
wait_for "node Ready" 30 sh -c "'$CTL' get nodes | grep -q Ready"

echo
echo "==> two nginx pods behind a Service; one's readiness probe will fail"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Service
metadata: { name: web }
spec: { selector: { app: web }, ports: [ { port: 80, targetPort: 80 } ] }
---
apiVersion: v1
kind: Pod
metadata: { name: web-ok, labels: { app: web } }
spec:
  nodeName: rk-node1
  containers:
    - name: nginx
      image: nginx:1.27
      readinessProbe: { httpGet: { path: /, port: 80 }, initialDelaySeconds: 1, periodSeconds: 2 }
---
apiVersion: v1
kind: Pod
metadata: { name: web-bad, labels: { app: web } }
spec:
  nodeName: rk-node1
  containers:
    - name: nginx
      image: nginx:1.27
      # Probes a port nothing listens on, so this pod never becomes ready.
      readinessProbe: { httpGet: { path: /, port: 9999 }, initialDelaySeconds: 1, periodSeconds: 2, failureThreshold: 1 }
EOF
wait_for "both pods running" 150 sh -c "[ \$($CTL get pods | grep -c ' Running ') -eq 2 ]"
wait_for "web-ok to pass its readiness probe" 30 sh -c "[ \"\$($CTL get pod web-ok -o json | python3 -c 'import json,sys;print(json.load(sys.stdin)[\"status\"][\"containerStatuses\"][0].get(\"ready\"))')\" = True ]"
sleep 3
echo "pod readiness:"
"$CTL" get pods
echo "  web-ok  ready=$(pod_ready web-ok)   web-bad ready=$(pod_ready web-bad)"
[ "$(pod_ready web-ok)" = "True" ] && [ "$(pod_ready web-bad)" = "False" ] || { echo "unexpected readiness"; exit 1; }

echo
echo "==> readiness gates service endpoints: only the ready pod is a backend"
OK_IP=$(pod_ip web-ok); BAD_IP=$(pod_ip web-bad)
echo "  web-ok=$OK_IP (ready)   web-bad=$BAD_IP (not ready)"
wait_for "service rules" 20 sh -c "nft list table ip rk-services 2>/dev/null | grep -q dnat"
nft list table ip rk-services | grep dnat
nft list table ip rk-services | grep -q "$OK_IP"     && echo "  endpoint includes the ready pod ($OK_IP)"
nft list table ip rk-services | grep -q "$BAD_IP"    && { echo "  BUG: not-ready pod $BAD_IP is an endpoint"; exit 1; } || echo "  not-ready pod excluded from endpoints"

echo
echo "==> liveness probe restarts an unhealthy container"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: flaky }
spec:
  nodeName: rk-node1
  restartPolicy: Always
  containers:
    - name: app
      image: busybox:stable
      command: ["sleep", "3600"]
      livenessProbe: { exec: { command: ["false"] }, initialDelaySeconds: 1, periodSeconds: 2, failureThreshold: 1 }
EOF
wait_for "flaky to restart at least once" 90 sh -c "[ \"\$($CTL get pod flaky -o json | python3 -c 'import json,sys;cs=json.load(sys.stdin)[\"status\"].get(\"containerStatuses\",[{}]);print(cs[0].get(\"restartCount\",0))')\" -ge 1 ]"
echo "  flaky restartCount=$(pod_restarts flaky) (liveness probe 'false' keeps failing)"
"$CTL" get pods | grep '^flaky'

echo
echo "==> rk-ctl logs: fetch web-ok's container log from its node"
"$CTL" logs web-ok | grep -m1 -E "start worker process|ready for start up" && echo "  (logs streamed from the node's agent)"

echo
echo "==> teardown"
for p in web-ok web-bad flaky; do "$CTL" delete pod "$p" >/dev/null; done
"$CTL" delete service web >/dev/null

echo
echo "phase7a-demo: OK"
