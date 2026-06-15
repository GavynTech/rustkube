#!/usr/bin/env bash
# install.sh — install a single-node RustKube cluster as systemd services,
# secured with TLS + a bearer token. Run as root.
#
#   sudo scripts/install.sh [--prefix DIR] [--confdir DIR] [--datadir DIR]
#                           [--node-name NAME] [--api-name HOST]... [--pod-cidr CIDR]
#                           [--no-build] [--force-pki]
#
# Then:  systemctl enable --now rustkube.target
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX=/usr/local
CONFDIR=/etc/rustkube
DATADIR=/var/lib/rustkube
NODE_NAME=$(hostname -s 2>/dev/null || hostname)
POD_CIDR=10.42.0.0/24
API_NAMES=()
BUILD=1
FORCE_PKI=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)    PREFIX=$2; shift 2 ;;
        --confdir)   CONFDIR=$2; shift 2 ;;
        --datadir)   DATADIR=$2; shift 2 ;;
        --node-name) NODE_NAME=$2; shift 2 ;;
        --api-name)  API_NAMES+=("$2"); shift 2 ;;
        --pod-cidr)  POD_CIDR=$2; shift 2 ;;
        --no-build)  BUILD=0; shift ;;
        --force-pki) FORCE_PKI=1; shift ;;
        *) echo "install.sh: unknown option $1" >&2; exit 1 ;;
    esac
done
BINDIR="$PREFIX/bin"

[[ $EUID -eq 0 ]] || { echo "install.sh: run as root" >&2; exit 1; }

echo "==> RustKube install"
echo "    binaries -> $BINDIR"
echo "    config   -> $CONFDIR"
echo "    data     -> $DATADIR"
echo "    node     -> $NODE_NAME, pod CIDR $POD_CIDR"

if [[ $BUILD -eq 1 ]]; then
    echo "==> building release binaries"
    cargo build --workspace --release 2>&1 | tail -1
fi

echo "==> installing binaries"
install -d "$BINDIR" "$CONFDIR" "$DATADIR"
for bin in rk-api rk-scheduler rk-controllers rk-agent rk-ingress rk-ctl; do
    install -m 0755 "target/release/$bin" "$BINDIR/$bin"
done

echo "==> PKI ($CONFDIR/pki)"
if [[ -f "$CONFDIR/pki/token" && $FORCE_PKI -eq 0 ]]; then
    echo "    keeping existing PKI (pass --force-pki to regenerate)"
else
    name_args=()
    for n in "${API_NAMES[@]}"; do name_args+=(--api-name "$n"); done
    "$BINDIR/rk-ctl" pki init --dir "$CONFDIR/pki" --force "${name_args[@]}" >/dev/null
    echo "    generated CA, server cert and token"
fi

echo "==> writing configs to $CONFDIR"
cat > "$CONFDIR/api.yaml" <<EOF
# rk-api: HTTPS + bearer-token auth. Loopback only by default.
listenAddr: "127.0.0.1:6443"
dbPath: "$DATADIR/rustkube.db"
authTokenFile: "$CONFDIR/pki/token"
tls:
  certFile: "$CONFDIR/pki/server.crt"
  keyFile: "$CONFDIR/pki/server.key"
EOF

write_client_config() {
    cat > "$CONFDIR/$1.yaml" <<EOF
apiUrl: "https://127.0.0.1:6443"
tokenFile: "$CONFDIR/pki/token"
caFile: "$CONFDIR/pki/ca.crt"
EOF
}
write_client_config scheduler
write_client_config controllers

cat > "$CONFDIR/agent.yaml" <<EOF
apiUrl: "https://127.0.0.1:6443"
tokenFile: "$CONFDIR/pki/token"
caFile: "$CONFDIR/pki/ca.crt"
nodeName: "$NODE_NAME"
dataDir: "$DATADIR"
podCidr: "$POD_CIDR"
nodeIp: "127.0.0.1"
EOF
chmod 0644 "$CONFDIR"/*.yaml

echo "==> installing systemd units"
for unit in deploy/systemd/*.service deploy/systemd/*.target; do
    name=$(basename "$unit")
    sed "s|@BINDIR@|$BINDIR|g; s|@CONFDIR@|$CONFDIR|g" "$unit" > "/etc/systemd/system/$name"
done
systemctl daemon-reload

echo
echo "install.sh: done. Start the cluster with:"
echo "    systemctl enable --now rustkube.target"
echo
echo "Then talk to it (the CLI reads these like a kubeconfig):"
echo "    export RK_API_URL=https://127.0.0.1:6443"
echo "    export RK_TOKEN_FILE=$CONFDIR/pki/token RK_CA_FILE=$CONFDIR/pki/ca.crt"
echo "    rk-ctl get nodes"
