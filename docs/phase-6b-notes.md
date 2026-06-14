# Phase 6B — design notes

- **Services are a node-local data plane fed by a cluster-wide watch.** Every
  node's agent watches all Services and Pods, computes each service's ready
  endpoints (`service_targets`), and reprograms its own nftables table and DNS
  records. There's no central proxy: a VIP works from any pod because every
  node programs the same rules, and the DNAT'd traffic rides the Phase 6A
  overlay to whichever node the chosen backend lives on.
- **ClusterIPs are allocated at admission.** On Service create the API server
  scans the live services for used IPs/ports and assigns a free ClusterIP from
  `10.96.0.0/16` (starting at `.10`) and, for NodePort services, a port from
  `30000–32767`. Single writer, so no allocator races — the same place Secret
  `stringData` folding happens.
- **nftables does the load balancing.** The whole `rk-services` table is
  regenerated and re-applied (flush + reload) whenever services or endpoints
  change — simple and always convergent. Each ClusterIP rule DNATs to one
  backend chosen by `numgen random mod N`, in both the `prerouting` hook
  (pod-origin traffic) and `output` (host-origin). NodePort adds a
  `fib daddr type local` rule so only traffic to a node's own address is
  caught. Generating the ruleset is pure and tested; only `apply`/`clear`
  shell out to `nft`.
- **The DNS server is ~120 lines and embedded.** A tiny UDP responder parses
  one question, answers A records for `<svc>.<ns>` and
  `<svc>.<ns>.svc.cluster.local` from a map the agent keeps live, and NXDOMAINs
  the rest. It binds the node's bridge gateway, which the runtime writes into
  each pod's `/etc/resolv.conf` along with the Kubernetes search list and
  `ndots:5`, so `curl http://svc/` resolves cluster-locally.
- **The bridge is created at agent startup**, not lazily on the first pod, so
  the DNS socket has its gateway address to bind to immediately.
- **`clusterIP`, not `clusterIp`.** The Service type now renames the field to
  the Kubernetes spelling. The API allocator and the CLI always used
  `clusterIP`; the typed model's camelCase derive produced `clusterIp`, so the
  agent's reflected `Service` deserialized `cluster_ip` as `None` and computed
  zero endpoints — the table was being cleared, not filled. Exactly the `podIP`
  bug from Phase 3, in a new field; fixed the same way.
- **Demo verification is honest end-to-end**: a busybox client pod, entered
  with `nsenter` into its network + mount namespaces (so it uses *its*
  resolv.conf and busybox's `wget`), resolves the service by name and curls
  the VIP ten times — landing on backends on both nodes, proving DNS + VIP +
  nftables DNAT + cross-node overlay all compose.
- **Deferred**: session affinity, headless services, multi-port DNS SRV
  records, ExternalName, and IPv6 — none needed to demonstrate the spec's
  "stable virtual IPs load-balancing across healthy pod IPs" plus
  `service-name.namespace` resolution.
