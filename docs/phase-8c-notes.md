# Phase 8C — design notes

- **One install command, from source to running services.** `scripts/install.sh`
  builds the release binaries, drops them in `$PREFIX/bin`, generates the PKI
  with `rk-ctl pki init` (idempotent — it keeps an existing token unless
  `--force-pki`, so re-running doesn't lock out a live cluster), writes the five
  configs wired for TLS + token, installs the systemd units with the real paths
  substituted, and reloads systemd. `systemctl enable --now rustkube.target`
  then brings the whole secured cluster up.
- **systemd units that match how each component actually runs.** The API,
  scheduler and controllers are network-only, so they get `ProtectSystem=strict`,
  `ProtectHome`, `NoNewPrivileges` and (for the two that write nothing)
  `PrivateTmp`. The agent gets none of that on purpose — it needs real root,
  `Delegate=yes` for its cgroups, and an unsandboxed host to drive namespaces,
  mounts and nftables. Ordering is `After=rk-api.service`; `PartOf=` ties them
  to the target so one `stop` takes the cluster down cleanly.
- **The capstone test secures everything at once.** `scripts/e2e-cluster.sh`
  stands up a TLS + token cluster and runs fourteen assertions through it: the
  auth boundary (401 vs 200), a 3-replica Deployment behind a ClusterIP Service
  and a TLS Ingress, the service VIP load-balancing, HTTPS termination at the
  ingress, scale 3→5, a rolling update to a new ReplicaSet, self-heal after a
  delete, log retrieval, and a drain/uncordon cycle — every major feature of
  every prior phase, exercised together over the hardened transport.
- **Name-based rollout checks beat JSON-in-a-loop.** The first cut of the e2e
  polled pod images with `python3 -c` nested inside `sh -c` inside the wait
  loop; the quoting mangled and it never matched even though the rollout had
  finished. The fix watches the pod-template-hash (the 2nd name segment) change
  instead — a rolling update always lands a new ReplicaSet, so a new hash is the
  signal, checked with plain `awk`.
- **`e2e.sh` stays the fast lane.** The original Phase-2 smoke test (apply /
  get / watch / delete over HTTP, no root) is kept as the quick check;
  `e2e-cluster.sh` is the slow, root, whole-cluster one. Two tiers, two speeds.
- **The README gained an architecture diagram** showing the control plane
  (rk-api over sqlite), the watchers around it (scheduler, controllers, ingress,
  per-node agents), and the data plane (service VIPs via nftables, the VXLAN pod
  overlay, embedded DNS) — the map the prose phases build up to.
- **What's deliberately still out of scope** (and would be the next real work):
  multi-node `install.sh` (it provisions one node; more nodes means repeating it
  with a shared CA + the same token), token rotation and per-identity RBAC, and
  high-availability for the single API server / single sqlite store. This phase
  hardens and packages the existing single-control-plane design; it doesn't make
  it distributed.
- **Phase 8 closes the build.** Phases 0–7 made a Kubernetes-shaped orchestrator
  that runs real containers; Phase 8 put it behind TLS and a token, taught it to
  drain a node, and made it installable as systemd services with one end-to-end
  test over the whole thing.
