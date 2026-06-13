#!/usr/bin/env bash
# Simulate an extra RustKube node in its own network namespace — the spec's
# dev-mode way to run a multi-node cluster on one machine. The namespace is
# the node's "machine"; a veth uplink plus NAT gives an agent inside it reach
# to the host's rk-api (binds 0.0.0.0:6443) and to Docker Hub for image pulls.
#
#   sim-node.sh up   <name> <ns-ip>    e.g. sim-node.sh up rk-node2 10.200.0.2
#   sim-node.sh down <name>
#
# Host end of the uplink is <ns-ip>'s .1 on a /24; the netns end is <ns-ip>.
# Prints the apiUrl an agent in the namespace should target.
set -euo pipefail

[[ $EUID -eq 0 ]] || { echo "sim-node: must run as root" >&2; exit 1; }

STATE_DIR=/run/rk-sim

iface() { printf '%s' "$1-$2" | cut -c1-15; }   # veth names cap at 15 bytes

up() {
    local ns=$1 ns_ip=$2
    local prefix=${ns_ip%.*}
    local host_ip="$prefix.1" subnet="$prefix.0/24"
    local hv nv uplink
    hv=$(iface vh "$ns"); nv=$(iface vn "$ns")
    uplink=$(ip route | awk '/^default/{print $5; exit}')

    if ip netns list | grep -qw "$ns"; then
        echo "sim-node: netns '$ns' already exists; run 'down' first" >&2
        exit 1
    fi

    ip netns add "$ns"
    ip link add "$hv" type veth peer name "$nv"
    ip link set "$nv" netns "$ns"
    ip addr add "$host_ip/24" dev "$hv"
    ip link set "$hv" up
    ip netns exec "$ns" ip addr add "$ns_ip/24" dev "$nv"
    ip netns exec "$ns" ip link set "$nv" up
    ip netns exec "$ns" ip link set lo up
    ip netns exec "$ns" ip route add default via "$host_ip"

    # NAT: the namespace reaches the host's rk-api and the internet.
    sysctl -q -w net.ipv4.ip_forward=1
    iptables -t nat -A POSTROUTING -s "$subnet" -o "$uplink" -j MASQUERADE
    iptables -A FORWARD -i "$hv" -j ACCEPT
    iptables -A FORWARD -o "$hv" -j ACCEPT

    # Resolver inside the namespace (the host's may be a netns-local stub).
    mkdir -p "/etc/netns/$ns"
    echo "nameserver 8.8.8.8" > "/etc/netns/$ns/resolv.conf"

    mkdir -p "$STATE_DIR"
    printf 'HV=%s\nSUBNET=%s\nUPLINK=%s\n' "$hv" "$subnet" "$uplink" > "$STATE_DIR/$ns.env"

    echo "node netns '$ns' up: host=$host_ip ns=$ns_ip apiUrl=http://$host_ip:6443"
}

down() {
    local ns=$1
    local env_file="$STATE_DIR/$ns.env"
    # shellcheck disable=SC1090
    [[ -f $env_file ]] && source "$env_file"
    ip netns del "$ns" 2>/dev/null || true
    [[ -n ${HV:-} ]] && ip link del "$HV" 2>/dev/null || true
    if [[ -n ${SUBNET:-} ]]; then
        iptables -t nat -D POSTROUTING -s "$SUBNET" -o "$UPLINK" -j MASQUERADE 2>/dev/null || true
        iptables -D FORWARD -i "$HV" -j ACCEPT 2>/dev/null || true
        iptables -D FORWARD -o "$HV" -j ACCEPT 2>/dev/null || true
    fi
    rm -rf "/etc/netns/$ns" "$env_file"
    echo "node netns '$ns' down"
}

case "${1:-}" in
    up)   [[ $# -eq 3 ]] || { echo "usage: sim-node.sh up <name> <ns-ip>" >&2; exit 1; }; up "$2" "$3" ;;
    down) [[ $# -eq 2 ]] || { echo "usage: sim-node.sh down <name>" >&2; exit 1; }; down "$2" ;;
    *)    echo "usage: sim-node.sh {up <name> <ns-ip>|down <name>}" >&2; exit 1 ;;
esac
