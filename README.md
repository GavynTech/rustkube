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
- [x] Phase 3 — node agent (reconcile assigned pods, report status)
- [x] Phase 4 — multi-node + scheduler (bin packing, nodeSelector)
- [x] Phase 5 — controllers (Deployment/ReplicaSet, self-healing, rolling updates, scale)
- [x] Phase 6 — networking: bridge/veth + VXLAN overlay (6A) + service VIPs + DNS (6B)
- [~] Phase 7 — **7A** probes + logs (done); **7B** ingress + the website workload (next)
- [ ] Phase 8 — hardening (TLS, tokens, drain, e2e suite, install script, systemd)

## Layout

| Crate | Kind | Role |
|---|---|---|
| `crates/rk-types` | lib | resource model (Pod, Deployment, …), quantities, config/tracing bootstrap |
| `crates/rk-store` | lib | `Store` trait + the sqlite implementation (revisions, change log, watch fan-out) |
| `crates/rk-api` | bin | control-plane API server (axum) |
| `crates/rk-scheduler` | bin | pod → node placement (filter + most-allocated bin packing, nodeSelector) |
| `crates/rk-controllers` | bin | Deployment/ReplicaSet/Node controllers + owner-ref GC |
| `crates/rk-runtime` | lib | OCI image + container lifecycle |
| `crates/rk-net` | lib | VXLAN overlay, nftables service VIPs, embedded DNS server |
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

## Node agent (Phase 3 scope)

On the Ubuntu VM, as root: start `rk-api` and `rk-agent`, and pods bound to
the node (via `spec.nodeName` — the scheduler arrives in Phase 4) actually
run, report status, self-heal per their restart policy, and tear down on
delete:

```sh
./target/release/rk-api  &       # control plane
./target/release/rk-agent &      # registers the node, reconciles pods
rk-ctl get nodes                 # NAME      STATUS   AGE
rk-ctl apply -f mypod.yaml       # pod with spec.nodeName set
rk-ctl get pods                  # web-0  Running  0  10.42.0.6  rustkube  12s
curl http://10.42.0.6/
rk-ctl delete pod web-0
```

`scripts/phase3-demo.sh` runs the full loop — registration, pull, run, curl,
kill-and-self-heal, run-to-completion, teardown (design notes in
[docs/phase-3-notes.md](docs/phase-3-notes.md)).

## Multi-node + scheduler (Phase 4 scope)

`rk-scheduler` watches for unbound pods and places them by free CPU/memory
(most-allocated bin packing) and `nodeSelector`, writing the binding back; a
pod that fits nowhere is left Pending with the reason in its status. On the
VM, a second node is simulated as an agent in its own network namespace
(`scripts/sim-node.sh`), each node carving pods from its own /24:

```sh
rk-ctl get nodes                 # rk-node1 Ready / rk-node2 Ready
rk-ctl apply -f some-pods.yaml   # no nodeName — the scheduler assigns one
rk-ctl get pods                  # NODE column shows the placement
# a pod requesting more than any node has stays Pending:
rk-ctl describe pod huge         # status.message: 0/2 nodes available: 2 Insufficient cpu
```

`scripts/phase4-demo.sh` brings up both nodes and demonstrates bin packing,
`nodeSelector` steering, an unschedulable pod, and pods actually running on
each node in its own pod network (design notes in
[docs/phase-4-notes.md](docs/phase-4-notes.md)).

## Controllers (Phase 5 scope)

`rk-controllers` reconciles the workload resources: a Deployment owns a
ReplicaSet per template revision, a ReplicaSet keeps N pods alive, and a Node
controller marks dead nodes and reschedules their pods. Owner-reference
garbage collection makes deletes cascade.

```sh
rk-ctl apply -f examples/deployment.yaml   # Deployment → ReplicaSet → 3 Pods
rk-ctl get deployments                     # web   3/3   3   3
rk-ctl get rs                              # web-<hash>  3  3  3
rk-ctl delete pod <one>                    # the ReplicaSet recreates it
rk-ctl scale deployment web --replicas=5   # two more pods appear
# edit the image/template and re-apply → a rolling update (maxUnavailable/maxSurge)
rk-ctl delete deployment web               # GC removes its ReplicaSets, then Pods
```

`scripts/phase5-demo.sh` runs the lot on the two-node VM cluster: deploy,
self-heal, scale up/down, a rolling update, node-death rescheduling, and
cascade delete (design notes in [docs/phase-5-notes.md](docs/phase-5-notes.md)).

## Pod networking (Phase 6A scope)

Each node runs a Linux bridge (`rk-br0`) that its pods' veths land on, so
same-node pods share an L2 segment. Cross-node traffic rides a VXLAN overlay
(`rk-vxlan0`): every agent advertises its tunnel endpoint (VTEP MAC) on its
Node and programs route/neighbour/FDB entries for each peer from the Node
list — flannel's unicast model, no flooding.

`scripts/phase6a-demo.sh` pins an nginx pod to each of the two nodes and
shows a pod on node1 curling a pod on node2 over the tunnel (both ways), plus
same-node connectivity over the bridge (design notes in
[docs/phase-6a-notes.md](docs/phase-6a-notes.md)).

## Services + DNS (Phase 6B scope)

A Service gets a stable ClusterIP (allocated from 10.96.0.0/16 at admission);
every node's agent watches Services and Pods, computes the ready endpoints,
and programs an nftables table that DNATs the VIP to a backing pod
(load-balanced with `numgen random`). Each agent also runs a small embedded
DNS server on its bridge gateway — which the runtime writes into every pod's
`/etc/resolv.conf` — resolving `service.namespace` to its ClusterIP.

```sh
rk-ctl apply -f app-and-service.yaml   # a Deployment + a ClusterIP Service
rk-ctl get services                    # whoami  ClusterIP  10.96.0.10  80/TCP
# from inside any pod:
curl http://whoami/                     # name → DNS → VIP → nftables → a backend
```

`scripts/phase6b-demo.sh` fronts a Deployment of `whoami` pods (spread across
both nodes) with a ClusterIP Service, resolves it by name from a client pod,
and curls the VIP to show load-balancing across backends on both nodes via
the overlay (design notes in [docs/phase-6b-notes.md](docs/phase-6b-notes.md)).

## Probes + logs (Phase 7A scope)

The agent runs liveness and readiness probes (`httpGet`, `tcpSocket`, `exec`).
A readiness probe gates the pod's container `ready` status — and since
services key endpoints on readiness, a not-ready pod receives no traffic; a
liveness failure restarts the container. `rk-ctl logs <pod>` fetches a pod's
output from the node running it.

```sh
rk-ctl get pods            # READY column reflects readiness probes (1/1, 0/1)
rk-ctl logs web-0          # container stdout/stderr from the node
```

`scripts/phase7a-demo.sh` shows a probe-failing pod excluded from its
service's endpoints, a liveness probe restarting a container, and `rk-ctl
logs` (design notes in [docs/phase-7a-notes.md](docs/phase-7a-notes.md)).

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
