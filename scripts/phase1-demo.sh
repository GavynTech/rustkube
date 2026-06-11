#!/usr/bin/env bash
# Phase 1 demo — run as root on the Ubuntu 24.04 VM, from the repo root.
# Pulls nginx from Docker Hub, runs it in fresh namespaces with cgroups v2
# limits via rk-runtime, proves it serves HTTP from the VM host, then cleans
# up. Exits non-zero if any step misbehaves.
set -euo pipefail

RKRT=${RKRT:-target/release/rkrt}
NAME=web

if [[ $EUID -ne 0 ]]; then
    echo "phase1-demo: run as root (namespaces, cgroups, mounts)" >&2
    exit 1
fi
[[ -x $RKRT ]] || cargo build --release -p rk-runtime

echo "==> pull nginx (digest-verified)"
"$RKRT" pull nginx

echo "==> run: namespaces + cgroups v2 (256Mi, 500m)"
"$RKRT" run --name "$NAME" --memory 256Mi --cpus 500m nginx

IP=$(python3 -c "import json,sys;print(json.load(open('/var/lib/rustkube/containers/$NAME/state.json'))['ip'])")
PID1=$(cat "/var/lib/rustkube/containers/$NAME/pid1")

echo "==> verify isolation + limits"
ls -l "/proc/$PID1/ns/" | awk '{print $9, $11}' | sed '/^ *$/d'
CG=$(awk -F:: '{print $2}' "/proc/$PID1/cgroup")
echo "cgroup: $CG"
cat "/sys/fs/cgroup$CG/memory.max" "/sys/fs/cgroup$CG/cpu.max"

echo "==> curl from the VM host"
curl -fsS "http://$IP/" | grep -o "<title>.*</title>"

echo "==> ps + logs"
"$RKRT" ps
"$RKRT" logs "$NAME" | tail -3

echo "==> stop + rm"
"$RKRT" stop "$NAME"
"$RKRT" rm "$NAME"
"$RKRT" ps

echo "phase1-demo: OK"
