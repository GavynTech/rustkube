# RustKube

A Kubernetes-style container orchestration platform written entirely in Rust:
an HTTPS API server over a sqlite store, a scheduler, workload controllers, a
from-scratch OCI container runtime on raw Linux namespaces/cgroups, pod
networking with a VXLAN overlay and nftables service VIPs, an ingress reverse
proxy, and a kubectl-style CLI.

## Architecture

```
                 rk-ctl  (apply · get · scale · drain · logs · pki init)
                    │  HTTPS + bearer token
                    ▼
   ┌──────────────────────────────────────────────────────────┐
   │                      rk-api  ( :6443, TLS )                │
   │    REST + SSE watch · admission · optimistic concurrency   │
   │                          rk-store (sqlite)                 │
   └──────────────────────────────────────────────────────────┘
       ▲             ▲              ▲                  ▲
       │  list + watch (HTTPS + token), write back     │
       │             │              │                  │
  rk-scheduler  rk-controllers   rk-ingress       rk-agent ── per node ──┐
  filter + bin   Deployment/RS    :80 + :443       reconcile pods        │
  pack, bind     Node, GC,        host/path route  pull · run · probes   │
  spec.nodeName  drain-aware      TLS from Secret   logs · report status │
                                       │                    │            │
                                       │ to ClusterIP       │ creates    │
                                       ▼                    ▼            │
                              nftables service VIP   ┌──────────────┐    │
                              (DNAT + numgen LB) ───▶│ pods: netns, │    │
                                       ▲             │ cgroups v2,  │    │
                              VXLAN overlay (VNI 42) │ veth@rk-br0  │◀───┘
                              + embedded DNS    ────▶└──────────────┘
```

Every component but the API server is a watcher: it lists and watches the
objects it cares about over the same HTTPS+token client and writes its decisions
back. The control plane is pure-function reconcilers around one store; the data
plane — service VIPs, the pod overlay, DNS — is programmed by the per-node
agents.

## Crates

| Crate | Kind | Role |
|---|---|---|
| `crates/rk-types` | lib | resource model (Pod, Deployment, …), quantities, config/tracing bootstrap |
| `crates/rk-store` | lib | `Store` trait + sqlite implementation (revisions, change log, watch fan-out) |
| `crates/rk-api` | bin | control-plane API server (axum) |
| `crates/rk-scheduler` | bin | pod → node placement (filter + most-allocated bin packing, nodeSelector) |
| `crates/rk-controllers` | bin | Deployment/ReplicaSet/Node controllers + owner-ref GC |
| `crates/rk-runtime` | lib | OCI image pull + container lifecycle |
| `crates/rk-net` | lib | VXLAN overlay, nftables service VIPs, embedded DNS server |
| `crates/rk-agent` | bin | per-node kubelet equivalent |
| `crates/rk-ctl` | bin | CLI |

## Getting started

Building and image pulls work anywhere. **Running containers needs root,
cgroups v2 and overlayfs** — use the Ubuntu 24.04 VM, never the dev workstation.

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
```

`rk-ctl` is `./target/debug/rk-ctl`; `alias rk-ctl=$PWD/target/debug/rk-ctl`
makes the commands read as written. `RUST_LOG` controls verbosity (e.g.
`RUST_LOG=debug make dev-up`). Component config files live in
`config/<component>.yaml`; an explicit `--config <path>` or `RK_CONFIG=<path>`
overrides the default search.

## Capabilities

### Container runtime

`rkrt` is the development CLI over `rk-runtime` — a from-scratch OCI runtime: a
digest-verified image pull from Docker Hub, then containers isolated with Linux
namespaces, cgroups v2 limits, and an overlayfs rootfs.

```sh
cargo build --release -p rk-runtime          # builds target/release/rkrt
rkrt pull nginx                              # digest-verified pull from Docker Hub
rkrt run --name web --memory 256Mi --cpus 500m nginx
curl http://10.42.0.6/                       # nginx, from the VM host
rkrt ps && rkrt logs web
rkrt stop web && rkrt rm web
```

### Control plane, store, and CLI

`rk-api` is an axum REST server with SSE watch streams, admission, and
optimistic concurrency, backed by `rk-store` over sqlite (monotonic revisions, a
change log, and watch fan-out). `rk-ctl` is the kubectl-style client:
`apply`/`get`/`describe`/`delete`/`scale`/`drain`/`logs`. See the quickstart
above.

### Node agent

`rk-agent` is the per-node kubelet equivalent: it registers its node, reconciles
the pods bound to it (pull, run, report status, self-heal per restart policy),
and tears them down on delete.

```sh
./target/release/rk-api  &       # control plane
./target/release/rk-agent &      # registers the node, reconciles pods
rk-ctl get nodes                 # NAME      STATUS   AGE
rk-ctl apply -f mypod.yaml       # pod with spec.nodeName set
rk-ctl get pods                  # web-0  Running  0  10.42.0.6  rustkube  12s
curl http://10.42.0.6/
rk-ctl delete pod web-0
```

### Scheduling

`rk-scheduler` watches for unbound pods and places them by free CPU/memory
(most-allocated bin packing) and `nodeSelector`, writing the binding back. A pod
that fits nowhere stays Pending with the reason in its status.

```sh
rk-ctl get nodes                 # rk-node1 Ready / rk-node2 Ready
rk-ctl apply -f some-pods.yaml   # no nodeName — the scheduler assigns one
rk-ctl get pods                  # NODE column shows the placement
rk-ctl describe pod huge         # status.message: 0/2 nodes available: 2 Insufficient cpu
```

### Workload controllers

`rk-controllers` reconciles the workload resources: a Deployment owns a
ReplicaSet per template revision, a ReplicaSet keeps N pods alive, and a Node
controller marks dead nodes and reschedules their pods. Owner-reference garbage
collection makes deletes cascade.

```sh
rk-ctl apply -f examples/deployment.yaml   # Deployment → ReplicaSet → 3 Pods
rk-ctl get deployments                     # web   3/3   3   3
rk-ctl delete pod <one>                    # the ReplicaSet recreates it
rk-ctl scale deployment web --replicas=5   # two more pods appear
# edit the image/template and re-apply → a rolling update (maxUnavailable/maxSurge)
rk-ctl delete deployment web               # GC removes its ReplicaSets, then Pods
```

### Pod networking

Each node runs a Linux bridge (`rk-br0`) that its pods' veths land on, so
same-node pods share an L2 segment. Cross-node traffic rides a VXLAN overlay
(`rk-vxlan0`): every agent advertises its tunnel endpoint (VTEP MAC) on its Node
and programs route/neighbour/FDB entries for each peer from the Node list —
flannel's unicast model, no flooding.

### Services and DNS

A Service gets a stable ClusterIP (allocated from 10.96.0.0/16 at admission);
every node's agent watches Services and Pods, computes the ready endpoints, and
programs an nftables table that DNATs the VIP to a backing pod (load-balanced
with `numgen random`). Each agent also runs a small embedded DNS server on its
bridge gateway — written into every pod's `/etc/resolv.conf` by the runtime —
resolving `service.namespace` to its ClusterIP.

```sh
rk-ctl apply -f app-and-service.yaml   # a Deployment + a ClusterIP Service
rk-ctl get services                    # whoami  ClusterIP  10.96.0.10  80/TCP
curl http://whoami/                     # from any pod: name → DNS → VIP → nftables → backend
```

### Probes and logs

The agent runs liveness and readiness probes (`httpGet`, `tcpSocket`, `exec`). A
readiness probe gates the container's `ready` status — and since services key
endpoints on readiness, a not-ready pod receives no traffic; a liveness failure
restarts the container. `rk-ctl logs <pod>` fetches a pod's output from the node
running it.

```sh
rk-ctl get pods            # READY column reflects readiness probes (1/1, 0/1)
rk-ctl logs web-0          # container stdout/stderr from the node
```

### Ingress

`rk-ingress` watches Ingress and Service objects, builds a `(host, path) →
ClusterIP:port` routing table, and reverse-proxies external HTTP on :80 (and
HTTPS on :443, terminating TLS from the Ingress's `tls` Secret by SNI) onto the
matched service VIP, where nftables load-balances across ready pods.
Most-specific match wins (named host over wildcard, longer prefix over shorter).
It must run on a node that carries the service rules.

```sh
rk-ingress --api-url https://127.0.0.1:6443 --http-port 80 &
rk-ctl apply -f ingress.yaml                       # host app.example.com → web:80
curl -H "Host: app.example.com" http://<node>/     # → load-balanced across pods
curl -H "Host: unknown" http://<node>/             # → 404 (no matching route)
# with a tls section referencing a kubernetes.io/tls Secret → :443
curl --resolve shop.example.com:443:127.0.0.1 --cacert ca.crt https://shop.example.com/
```

### TLS and token auth

`rk-ctl pki init` mints a CA, an API-server certificate, and a bearer token
offline (rcgen + `/dev/urandom`, no openssl). With those in place rk-api serves
HTTPS and requires `Authorization: Bearer <token>` on every request except
`/healthz`; the agent, scheduler, controllers, ingress and CLI all carry the
token and trust the CA. Both are opt-in — unset, the cluster stays plaintext and
open.

```sh
rk-ctl pki init --dir pki --api-name 10.0.0.5      # ca.crt, server.crt/key, token
# rk-api config: authTokenFile + tls.certFile/keyFile
# client config: tokenFile + caFile  (rk-ctl: --token-file/--ca-file)
rk-ctl --token-file pki/token --ca-file pki/ca.crt get pods
```

### Node drain

`rk-ctl drain` cordons a node (the scheduler stops placing pods there) and
evicts its pods so the controllers reschedule them elsewhere. `cordon` and
`uncordon` are the halves.

```sh
rk-ctl drain rk-node1        # cordons, evicts pods (get nodes shows SchedulingDisabled)
rk-ctl uncordon rk-node1     # node takes pods again
```

## Install (single-node, systemd)

`scripts/install.sh` installs a secured single-node cluster as systemd services:
it builds the binaries, generates the PKI, writes TLS+token configs, and drops
the units from `deploy/systemd/` into place.

```sh
sudo scripts/install.sh --api-name 127.0.0.1
sudo systemctl enable --now rustkube.target     # rk-api + scheduler + controllers + agent + ingress
rk-ctl get nodes                                # Ready, over HTTPS + token
```

## Demos and tests

Each capability has a runnable demo script and design notes:

| Script | Shows | Notes |
|---|---|---|
| `scripts/phase1-demo.sh` | container pull/run/logs/stop loop | [phase-1](docs/phase-1-notes.md) |
| `scripts/phase3-demo.sh` | agent registration, run, self-heal, teardown | [phase-3](docs/phase-3-notes.md) |
| `scripts/phase4-demo.sh` | two-node bin packing, nodeSelector, unschedulable pod | [phase-4](docs/phase-4-notes.md) |
| `scripts/phase5-demo.sh` | self-heal, scale, rolling update, node-death reschedule, cascade delete | [phase-5](docs/phase-5-notes.md) |
| `scripts/phase6a-demo.sh` | cross-node pod connectivity over the VXLAN overlay | [phase-6a](docs/phase-6a-notes.md) |
| `scripts/phase6b-demo.sh` | ClusterIP Service, DNS resolution, VIP load-balancing | [phase-6b](docs/phase-6b-notes.md) |
| `scripts/phase7a-demo.sh` | readiness gating endpoints, liveness restart, logs | [phase-7a](docs/phase-7a-notes.md) |
| `scripts/phase7b-demo.sh` | Ingress host routing + 404 on unknown host | [phase-7b](docs/phase-7b-notes.md) |
| `scripts/website-demo.sh` | static site behind Service + Ingress, end to end | — |
| `scripts/phase8a-demo.sh` | TLS+token auth boundary, full control plane over HTTPS | [phase-8a](docs/phase-8a-notes.md) |
| `scripts/phase8b-demo.sh` | ingress TLS termination, node drain/uncordon | [phase-8b](docs/phase-8b-notes.md) |

End-to-end tests:

- `scripts/e2e.sh` — fast, root-free Phase-2 API smoke test (`make e2e`).
- `scripts/e2e-cluster.sh` — the capstone: a TLS+token cluster asserting the auth
  boundary, a Deployment behind a ClusterIP Service and TLS Ingress, VIP
  load-balancing, scale, rolling update, self-heal, logs, and drain/uncordon —
  every major feature at once ([phase-8c](docs/phase-8c-notes.md)).

## Build status

- [x] Phase 0 — workspace scaffold: crates, shared types, config, logging, Makefile
- [x] Phase 1 — single-node container runner (pull + namespaces + cgroups + rootfs)
- [x] Phase 2 — API server + sqlite store + CLI (apply/get/delete/watch)
- [x] Phase 3 — node agent (reconcile assigned pods, report status)
- [x] Phase 4 — multi-node + scheduler (bin packing, nodeSelector)
- [x] Phase 5 — controllers (Deployment/ReplicaSet, self-healing, rolling updates, scale)
- [x] Phase 6 — networking: bridge/veth + VXLAN overlay, service VIPs + DNS
- [x] Phase 7 — probes + logs, ingress reverse proxy
- [x] Phase 8 — hardening: TLS + token auth, ingress TLS termination, node drain, install + e2e
