# Phase 4 — design notes

- **The scheduler is filter-then-score, and the decision is pure.**
  `schedule::schedule()` takes a pod's `nodeSelector` + summed requests, the
  node views, and the per-node requested tally, and returns a node name or a
  Kubernetes-style "0/N nodes available: …" reason. No I/O, so every rule
  (Ready, cordon, selector, fit, scoring, message aggregation) is a unit test.
- **Scoring is most-allocated** — the average post-placement utilization of
  cpu and memory, pick the fullest node a pod still fits on. That's the
  resource-aware bin packing the spec asks for: a small node fills before a
  big one spills over, consolidating onto fewer nodes. Ties break by node name
  for determinism.
- **The scheduler accounts for resources itself**, summing container
  *requests* of non-terminal bound pods per node (`tally_requested`) and
  subtracting from each node's advertised `allocatable`. It never trusts the
  agent to pre-subtract — that would race. Limits are runtime enforcement,
  not placement; absent requests count as zero, like Kubernetes.
- **Assumed bindings prevent oversubscription.** A pod the scheduler just
  bound takes a beat to reappear (with its `nodeName`) through the watch.
  Until it does, the scheduler keeps it in an `assumed` set — both to avoid
  re-binding it *and*, crucially, to count its request against the node, so
  the next pod in the same burst can't oversubscribe a node already filled.
  Assumptions clear when the watch confirms the binding, or after 30s if a
  bind was lost.
- **Binding is a plain `spec.nodeName` PUT** with optimistic concurrency
  (resourceVersion); a 409 just means the cache was stale and the next pass
  retries. We have no `/binding` subresource, so this is the honest mechanism.
- **Unschedulable pods get a `status.message`** ("0/2 nodes available: 2
  Insufficient cpu") — and only the scheduler writes it, because an
  unscheduled pod (no `nodeName`) has no owning agent, so the two writers
  never collide. The write is idempotent, so it never churns.
- **Generic list+watch reflector** (`cache::reflect`) keeps Node and Pod
  caches live off the shared `rk_types::client`; either changing wakes a
  single `Notify` that drives a scheduling pass (plus a resync ticker).
- **Per-node pod /24.** `rk-runtime` IPAM now reads a `/24` base from the
  data dir (persisted in `ipam.json`), set once by the agent
  (`configure_pod_cidr`) from its `podCidr` config; `rkrt` keeps the
  10.42.0.0/24 default. So node1's pods are 10.42.0.x and node2's are
  10.42.1.x — no aliasing. Cross-node routing between those /24s is Phase 6.
- **Multi-node simulation is real, not faked** (`scripts/sim-node.sh`): the
  extra node is an agent running in its own network namespace — the spec's
  dev-mode "node = netns" — reaching the API server and Docker Hub over a
  veth uplink with NAT, creating its pods' veths inside that namespace. The
  agent gained dev knobs to make placement demonstrable on one box:
  `nodeLabels` (for `nodeSelector`), and `capacityCpu`/`capacityMemory`
  overrides (netns nodes share the host's real cpu/memory, which would make
  every node identical) — the realistic equivalent of kube-reserved.
- **Deferred as noted**: cross-node pod connectivity, Services, and DNS are
  Phase 6 (node2's pods are reachable from within its namespace, not yet from
  node1); preemption, affinity/anti-affinity, and taints/tolerations are out
  of scope for this learning scheduler.
