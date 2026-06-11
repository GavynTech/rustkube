# RustKube

A Kubernetes-style container orchestration platform written entirely in Rust —
API server, scheduler, controllers, a from-scratch OCI container runtime on raw
Linux namespaces/cgroups, pod networking with VXLAN + nftables service VIPs,
ingress, and a kubectl-style CLI. Built phase by phase; see [SPEC.md](SPEC.md)
for the full specification.

> **Status: educational reference implementation.** Do not expose it to the
> internet. TLS and authentication land in Phase 8; even then, this is a
> learning system, not a production orchestrator.

## Phase tracker

- [x] Phase 0 — workspace scaffold: all crates, shared types, config loading, logging, Makefile
- [x] Phase 1 — single-node container runner (pull + namespaces + cgroups + rootfs)
- [x] Phase 2 — API server + sqlite store + CLI (apply/get/delete/watch)
- [ ] Phase 3 — node agent (reconcile assigned pods, report status)
- [ ] Phase 4 — multi-node + scheduler
- [ ] Phase 5 — controllers (Deployment/ReplicaSet, self-healing, rolling updates)
- [ ] Phase 6 — networking (bridge/veth, VXLAN, service VIPs, DNS)
- [ ] Phase 7 — ingress + probes + logs + the website workload
- [ ] Phase 8 — hardening (TLS, tokens, drain, e2e suite, install script, systemd)

## Layout

| Crate | Kind | Role |
|---|---|---|
| `crates/rk-types` | lib | resource model (Pod, Deployment, …), quantities, config/tracing bootstrap |
| `crates/rk-store` | lib | `Store` trait + the sqlite implementation (revisions, change log, watch fan-out) |
| `crates/rk-api` | bin | control-plane API server (axum) |
| `crates/rk-scheduler` | bin | pod → node placement |
| `crates/rk-controllers` | bin | reconciliation loops |
| `crates/rk-runtime` | lib | OCI image + container lifecycle |
| `crates/rk-net` | lib | bridge/veth/VXLAN, IPAM, service VIPs, DNS |
| `crates/rk-agent` | bin | per-node kubelet equivalent |
| `crates/rk-ctl` | bin | CLI |

## Quickstart (Phase 2 scope)

```sh
make build      # compile the whole workspace
make test       # unit tests
make clippy     # lint wall: -D warnings
make dev-up     # start rk-api on 127.0.0.1:6443, wait for /healthz

rk-ctl apply -f examples/pod.yaml      # pod/web-0 created
rk-ctl get pods                        # NAME  STATUS  NODE  AGE
rk-ctl get pods --watch                # streams ADDED/MODIFIED/DELETED rows
rk-ctl describe pod web-0              # full object as YAML
rk-ctl delete pod web-0

make dev-down
make e2e        # scripted end-to-end test of all of the above
```

(`rk-ctl` is `./target/debug/rk-ctl`; `alias rk-ctl=$PWD/target/debug/rk-ctl`
makes the above read as written.) `RUST_LOG` controls verbosity (e.g.
`RUST_LOG=debug make dev-up`). Component config files live in
`config/<component>.yaml`; an explicit `--config <path>` or
`RK_CONFIG=<path>` overrides the default search.

## Container runner (Phase 1 scope)

`rkrt` is the development CLI over `rk-runtime`. Pulling works anywhere;
running containers needs root, cgroups v2 and overlayfs — i.e. the Ubuntu
24.04 VM, never the dev workstation:

```sh
cargo build --release -p rk-runtime          # builds target/release/rkrt
rkrt pull nginx                              # digest-verified pull from Docker Hub
rkrt run --name web --memory 256Mi --cpus 500m nginx
curl http://10.42.0.6/                       # nginx, from the VM host
rkrt ps && rkrt logs web
rkrt stop web && rkrt rm web
```

`scripts/phase1-demo.sh` runs that whole loop end to end (design notes in
[docs/phase-1-notes.md](docs/phase-1-notes.md)).
