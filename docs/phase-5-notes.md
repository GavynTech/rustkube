# Phase 5 — design notes

- **One manager, one loop, pure planners.** `rk-controllers` keeps four
  list+watch caches (Deployment, ReplicaSet, Pod, Node) and runs a single
  reconcile pass on every change: Deployment → ReplicaSet → Node → GC, in
  dependency order so one pass cascades. Each controller's decision is a pure
  function (`replicaset::reconcile`, `deployment::reconcile`,
  `node::classify`, `gc::is_orphan`) returning ops the loop executes — the
  whole point being that the interesting logic is unit-tested without a
  cluster.
- **Ownership is by uid, not selector.** Pods carry an owner reference to
  their ReplicaSet, ReplicaSets to their Deployment. Controllers find their
  children by that uid, so two ReplicaSets with overlapping labels never
  fight over a pod. Garbage collection falls out of this: an object whose
  controller owner no longer exists is an orphan and is deleted — so deleting
  a Deployment cascades to its ReplicaSets and then their Pods, pass by pass.
- **The ReplicaSet controller keeps N alive** by counting non-terminal owned
  pods: too few → create from the template; too many → delete the least
  valuable first (not-ready before ready, newest before oldest); terminal
  pods are cleared and replaced. It also publishes `status.replicas` /
  `readyReplicas`, which the Deployment controller reads.
- **The Deployment controller rolls by pod-template-hash.** A short, stable
  hash of the template (canonical JSON, hash label excluded) names the
  ReplicaSet of each revision. A template change yields a new hash → a new
  ReplicaSet; the rollout then surges the new one up by `maxSurge` over
  desired and drains the old ones down only while enough pods stay ready
  (`desired - maxUnavailable`), one step per pass, until the new RS holds
  everything and the old sit at zero. `Recreate` instead drains all old pods
  before starting the new. The math is driven by RS spec replicas (for surge)
  and observed ready counts (for availability).
- **Self-healing is just the steady state.** Kill a pod and the RS sees the
  count drop and recreates it; the scheduler places the replacement and an
  agent runs it — no special-case code, the same reconcile that built it.
- **Node death → reschedule.** The Node controller flips a node to NotReady
  once its heartbeat is stale, and after a longer grace deletes the pods
  bound to it. Those pods are RS-owned, so the RS recreates them with no
  `nodeName`; the scheduler (which filters NotReady nodes) places them on a
  live node. The agent's heartbeat racing the controller back to Ready is the
  signal the node is actually alive — benign last-writer-wins.
- **Writers stay disjoint, so optimistic concurrency rarely conflicts.** The
  RS controller owns RS status and pod create/delete; the agent owns pod
  status; the scheduler owns `spec.nodeName`; the node controller owns node
  condition (contended only with the agent's heartbeat, intentionally). Every
  write carries the cached resourceVersion; a 409 just means re-read next pass.
- **The reflector is now shared.** The generic list+watch cache promoted to
  `rk_types::client::reflector` (behind the `client` feature) backs the
  scheduler and all four controller caches — one implementation of the
  reflector loop, fan-out via a single `Notify`.
- **`rk-ctl` grows `scale`** (read-modify-write `spec.replicas` with the
  server enforcing concurrency) and DESIRED/CURRENT/READY columns for
  ReplicaSets and READY/UP-TO-DATE/AVAILABLE for Deployments.
- **Deferred as noted**: revision history limits (old RSes linger at zero,
  Kubernetes' default behavior), `kubectl rollout` status/undo, pod
  disruption budgets, and HPA are out of scope; probe-based readiness is
  Phase 7 (until then "Running" is "ready").
