#!/usr/bin/env bash
# RustKube VPS bootstrap — run as root on a fresh Ubuntu 24.04 box.
# (Claude runs this over SSH; kept in the repo's orbit for reproducibility.)
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y build-essential pkg-config git curl make
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile default
grep -q cgroup2 /proc/filesystems && echo "cgroups v2: present"
echo "bootstrap: OK ($(. /etc/os-release && echo "$PRETTY_NAME"), $(nproc) cpu, $(free -h | awk '/^Mem:/{print $2}') ram)"
