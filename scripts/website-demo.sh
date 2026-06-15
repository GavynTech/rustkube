#!/usr/bin/env bash
# Website demo (Phase 7) — run as root on the Ubuntu 24.04 VM, from the repo root.
#
# Deploys a real static site end to end on RustKube: a 2-replica nginx
# Deployment serving examples/website/index.html, a ClusterIP Service, and an
# Ingress for host rustkube.local. Then it reaches the page the way the outside
# world would — through the ingress reverse proxy. The runtime has no volume
# mounts yet, so the page is injected into nginx's web root at container start
# (base64 -> /usr/share/nginx/html) before exec'ing nginx.
set -euo pipefail
cd "$(dirname "$0")/.."

API_URL="http://127.0.0.1:6443"
CTL=./target/release/rk-ctl
D1=/var/lib/rustkube
HOST=rustkube.local

[[ $EUID -eq 0 ]] || { echo "website-demo: run as root" >&2; exit 1; }

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
    echo "website-demo: timed out waiting for $what" >&2; "$CTL" get pods 2>/dev/null || true; return 1; }

echo "==> build (release)"
cargo build --workspace --release 2>&1 | tail -1

echo "==> api + scheduler + controllers + agent + ingress"
mkdir -p .dev; rm -f .dev/website.db*
printf 'listenAddr: "127.0.0.1:6443"\ndbPath: ".dev/website.db"\n' > .dev/web-api.yaml
RK_CONFIG="$PWD/.dev/web-api.yaml" ./target/release/rk-api > .dev/web-api.log 2>&1 &
RK_CONFIG="$PWD/config/scheduler.yaml"   ./target/release/rk-scheduler   > .dev/web-sched.log 2>&1 &
RK_CONFIG="$PWD/config/controllers.yaml" ./target/release/rk-controllers > .dev/web-ctrl.log 2>&1 &
wait_for "the API server" 20 curl -fsS "$API_URL/healthz"
printf 'apiUrl: "%s"\nnodeName: rk-node1\ndataDir: "%s"\npodCidr: "10.42.0.0/24"\nnodeIp: "127.0.0.1"\nheartbeatIntervalSecs: 3\n' "$API_URL" "$D1" > .dev/web-node1.yaml
./target/release/rk-agent --config "$PWD/.dev/web-node1.yaml" > .dev/web-node1.log 2>&1 &
./target/release/rk-ingress --api-url "$API_URL" --http-port 80 > .dev/web-ingress.log 2>&1 &
wait_for "node Ready" 30 sh -c "'$CTL' get nodes | grep -q Ready"

# Inject the page at container start: the runtime has no volume mounts yet, so
# nginx writes index.html from a base64 blob, then execs (becomes pid 1).
B64=$(base64 -w0 examples/website/index.html)

echo
echo "==> deploy the site: Deployment(nginx x2) + Service + Ingress(host $HOST)"
"$CTL" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Deployment
metadata: { name: rustkube-site, labels: { app: rustkube-site } }
spec:
  replicas: 2
  selector: { matchLabels: { app: rustkube-site } }
  template:
    metadata: { labels: { app: rustkube-site } }
    spec:
      nodeName: rk-node1
      containers:
        - name: web
          image: nginx:alpine
          command: ["/bin/sh", "-c"]
          args: ["echo $B64 | base64 -d > /usr/share/nginx/html/index.html && exec nginx -g 'daemon off;'"]
          resources: { requests: { cpu: 50m, memory: 16Mi } }
---
apiVersion: v1
kind: Service
metadata: { name: rustkube-site }
spec: { selector: { app: rustkube-site }, ports: [ { port: 80, targetPort: 80 } ] }
---
apiVersion: v1
kind: Ingress
metadata: { name: rustkube-site }
spec:
  rules:
    - host: $HOST
      http:
        paths:
          - path: /
            pathType: Prefix
            backend: { service: { name: rustkube-site, port: 80 } }
EOF
wait_for "2 site pods running" 180 sh -c "[ \$($CTL get pods | grep -c '^rustkube-site.* Running ') -eq 2 ]"
"$CTL" get pods | grep -E "NAME|rustkube-site"
CIP=$("$CTL" get service rustkube-site -o json | python3 -c "import json,sys; print(json.load(sys.stdin)['spec']['clusterIP'])")
echo "service ClusterIP $CIP, ingress host $HOST"
wait_for "the site to answer through the ingress" 40 sh -c "curl -fsS -H 'Host: $HOST' http://127.0.0.1/ -o /dev/null"

echo
echo "==> fetch the homepage through the ingress (curl -H \"Host: $HOST\")"
PAGE=$(curl -fsS -H "Host: $HOST" http://127.0.0.1/)
echo "$PAGE" | grep -qE "Served by .*RustKube|<title>RustKube</title>" || { echo "page content not served"; echo "$PAGE" | head; exit 1; }
TITLE=$(echo "$PAGE" | grep -oE "<title>[^<]*</title>")
echo "  HTTP 200, $TITLE, $(echo "$PAGE" | wc -c) bytes"
echo "  served line: $(echo "$PAGE" | grep -oE 'Served by <span class="rust">RustKube</span>' | sed 's/<[^>]*>//g')"

echo
echo "==> the same page is served by either replica (ingress -> service -> both pods)"
"$CTL" get pods | awk '/rustkube-site.* Running /{print "  endpoint: "$1}'

echo
echo "==> an unknown host still gets a 404 from the ingress"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -H "Host: nope.example.com" http://127.0.0.1/)
echo "  Host: nope.example.com -> HTTP $CODE"
[ "$CODE" = "404" ] || { echo "expected 404 for unmatched host"; exit 1; }

echo
echo "==> teardown"
"$CTL" delete ingress rustkube-site >/dev/null
"$CTL" delete service rustkube-site >/dev/null
"$CTL" delete deployment rustkube-site >/dev/null

echo
echo "website-demo: OK  (point a browser's Host/DNS for $HOST at the ingress node to view it)"
