# RustKube — Project Specification

This is the governing spec for RustKube, captured verbatim from the master prompt
(2026-06-10) plus the blocking-question decisions locked the same day.

## Locked decisions (2026-06-10)

- **Timing:** full send — all nine phases, gated on "next".
- **Authorship:** Claude writes the implementation; this repo is a reference
  implementation to be studied phase by phase.
- **Environment:** development on the Fedora 43 workstation; root-dependent
  phases (1, 6+) run on an Ubuntu 24.04 x86_64 VM/VPS as root. Cluster never
  runs on the daily machine.
- **Workload (Phase 7):** candidate is the Flowdeck site (Vite + Tailwind +
  TypeScript, Supabase backend, currently Vercel-deployed). To be confirmed
  before Phase 7 per the spec; note the database is hosted Supabase, so the
  "single-replica database Deployment" may become a self-hosted Postgres or be
  adapted.
- **Crate substitution:** `oci-distribution` is unmaintained; its continuation
  `oci-client` (same lineage/API) is used instead.

## Mission

Build **RustKube**: a Kubernetes-style container orchestration platform written
entirely in Rust, then deploy the website on it. End state: one CLI command —
containers get pulled, scheduled across nodes, networked with a stable virtual
IP, exposed through an ingress, and self-heal when they crash or a node dies.

## Workspace crates

| Crate | Role |
|---|---|
| `rk-types` | shared resource definitions (serde structs for all resources) |
| `rk-store` | state store behind a trait (sqlite via sqlx first; etcd can swap in later) |
| `rk-api` | control-plane API server (REST, axum) |
| `rk-scheduler` | assigns pods to nodes |
| `rk-controllers` | reconciliation loops (Deployment → ReplicaSet → Pod, self-healing) |
| `rk-runtime` | container lifecycle: pull OCI images, build rootfs, run containers |
| `rk-net` | container networking, service virtual IPs, embedded DNS |
| `rk-agent` | per-node agent (kubelet equivalent), uses rk-runtime + rk-net |
| `rk-ctl` | CLI (kubectl equivalent) |

Async runtime: tokio. HTTP: axum. All inter-component communication goes over
the API server (agents and controllers watch it; nothing talks directly to the
store except `rk-api`).

## Core resources

Pod, ReplicaSet, Deployment, Service (ClusterIP + NodePort), Ingress, Node,
Namespace, ConfigMap, Secret. Each has `spec` (desired) and `status`
(observed), like real Kubernetes. Declarative YAML, applied with the CLI.

## Component requirements

- **API server (`rk-api`)** — REST CRUD for every resource, validation,
  optimistic concurrency via resource versions, and a watch endpoint
  (long-poll or SSE) so agents/controllers get changes without polling loops.
- **State store (`rk-store`)** — trait-based; sqlite implementation with
  resource versioning.
- **Scheduler (`rk-scheduler`)** — watches for unassigned pods, scores nodes by
  free CPU/memory (resource-aware bin packing), honors `nodeSelector` labels,
  writes the binding back.
- **Controller manager (`rk-controllers`)** — generic reconcile-loop framework,
  then: Deployment controller (rollouts, rolling update with maxUnavailable),
  ReplicaSet controller (keep N replicas alive), Node controller (mark nodes
  dead after missed heartbeats and reschedule their pods).
- **Container runtime (`rk-runtime`)** — pull images from Docker Hub / GHCR
  with oci-client, verify digests, unpack layers (flate2 + tar) into a
  content-addressed store, assemble rootfs (overlayfs). Run containers with raw
  Linux primitives via the nix crate: new PID/UTS/mount/IPC/network namespaces,
  pivot_root, cgroups v2 for CPU + memory limits, env vars, mounted
  ConfigMaps/Secrets, capture stdout/stderr to log files. No Docker daemon.
- **Networking (`rk-net`)** — per-node Linux bridge + veth pair per pod,
  cluster-wide IPAM (each node gets a /24 from a configurable pod CIDR),
  cross-node pod traffic via VXLAN overlay (rtnetlink). Services: stable
  virtual IPs load-balancing across healthy pod IPs using nftables DNAT rules.
  Embedded DNS server so pods resolve `service-name.namespace` to the VIP.
- **Node agent (`rk-agent`)** — registers its node, heartbeats, watches the API
  for pods assigned to it, reconciles reality to match, runs
  liveness/readiness probes (http + tcp + exec), enforces restart policies
  with exponential backoff, reports pod status and node resource usage,
  streams container logs on request.
- **Ingress** — controller that watches Ingress resources and routes external
  HTTP traffic on 80/443 by host/path to Services (small Rust reverse proxy
  using hyper; TLS termination with rustls).
- **CLI (`rk-ctl`)** — `apply -f`, `get`, `describe`, `delete`, `logs <pod>`,
  `scale deployment <name> --replicas=N`, `drain <node>`. Clean table output.
- **Security** — TLS between all components (rustls, self-signed CA generated
  at init), bearer-token auth for agents and the CLI.

## Quality bar

- Compiles with zero warnings; `cargo clippy` clean.
- No `.unwrap()` / `.expect()` outside tests; real error types per crate.
- Unit tests for the scheduler and every controller; e2e integration script.
- `tracing` instrumentation throughout; `RUST_LOG` controls verbosity.
- Makefile with `dev-up`, `dev-down`, `test`, `e2e`.
- Every phase ends with exact commands + expected output (Ubuntu 24.04, root).

## Target environment

- Ubuntu 24.04 x86_64, root, cgroups v2.
- Dev mode: entire multi-node cluster on ONE machine (extra nodes simulated
  with separate network namespaces).
- Production mode: 1 control plane + N workers, joined with a token, installed
  via install script + systemd units.

## Build phases (strictly in order, one per response)

- **Phase 0** — Workspace scaffold: all crates, shared types, config loading,
  logging, Makefile. Everything compiles.
- **Phase 1** — Single-node container runner: pull nginx from Docker Hub, run
  in namespaces + cgroups with a working rootfs. Demo: curl nginx from host.
- **Phase 2** — API server + store + CLI: apply/get/delete Pods as YAML; watch
  endpoint works.
- **Phase 3** — Node agent: watches API, reconciles assigned pods, reports
  status.
- **Phase 4** — Multi-node + scheduler: second (simulated) node joins;
  scheduler places pods by resources and labels.
- **Phase 5** — Controllers: Deployments/ReplicaSets, self-healing, rolling
  updates, scale.
- **Phase 6** — Networking: pod-to-pod across nodes, Services with VIPs, DNS.
- **Phase 7** — Ingress + probes + logs + deploy the website; hit every
  success criterion.
- **Phase 8** — Hardening: TLS everywhere, auth tokens, node drain, e2e suite,
  install script, systemd units, README with architecture diagram.

## Working agreement

- Every file is complete with its full path — no truncation, no TODOs.
- Each phase ships with design notes (max 10 bullets).
- Anything that can't work exactly as written is called out explicitly with
  the closest working alternative — nothing silently faked or mocked.
- Phases too large for one response are split A/B/C within the phase.
