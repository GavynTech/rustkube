# Phase 7A — design notes

- **Probes split pure from impure, like every controller here.** `probe.rs`'s
  state machine — when a probe is due (initialDelay then period) and what a
  result means once `failureThreshold` consecutive failures accrue (flip
  readiness, or restart) — is unit-tested without any I/O. `execute` is the
  thin async side: a raw HTTP GET (status 2xx/3xx is a pass), a TCP connect,
  or `nsenter` into the pod's namespaces to run an `exec` command.
- **Readiness composes with what already existed.** A readiness probe's
  verdict becomes the pod's container `ready` flag — and the Phase 6B service
  data plane already computes endpoints from *ready* pods, so a not-ready pod
  is automatically dropped from the nftables DNAT set. No new wiring on the
  service side; readiness just feeds the same `ready` field the table reads.
- **Liveness restarts reuse the reconcile machinery.** A liveness failure
  doesn't kill-and-recreate inline; it *removes* the container, and the next
  reconcile pass sees it missing and starts it (bumping the restart tracker,
  honoring backoff) — the same path a crash takes. One code path for "container
  needs to come back," whatever the cause.
- **Probe state lives in the agent loop**, a `HashMap<pod-uid, PodProbes>`
  alongside the restart trackers, pruned when a pod leaves. The reconcile pass
  runs only *due* probes (period-paced off the container's age), so a 10s
  probe isn't hammered by the 5s reconcile cadence.
- **Logs are served from the node, fetched by name.** The agent exposes
  `GET /logs/{ns}/{name}` on port 10250, reading the container's `console.log`
  (the runtime has captured stdout/stderr there since Phase 1). `rk-ctl logs`
  resolves the pod to its `nodeName`, looks up that node's address, and reads
  from there — the kubelet-style log path, minus a server framework (a few
  dozen lines of hand-rolled HTTP/1.1 on each side).
- **`READY` joins the pod table** as `ready/total` containers, so readiness
  gating is visible in `rk-ctl get pods` (`1/1` vs `0/1`) the way kubectl
  shows it.
- **Deferred to 7B and beyond**: `startupProbe`, probe `successThreshold` > 1,
  `terminationGracePeriod`, and log `--follow`/`--tail` streaming (the
  endpoint returns the whole log today). The headline Phase 7 item — the
  Ingress reverse proxy and deploying the actual website — is 7B.
