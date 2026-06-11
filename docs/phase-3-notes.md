# Phase 3 — design notes

- **The agent is a reflector + a pure reconciler.** A list+watch task keeps a
  pod cache (keyed by UID, filtered to `spec.nodeName == me`) and pings the
  loop; each pass snapshots desired state, observes actual containers, and
  feeds both to a pure `plan()` that returns Start/Restart/Teardown ops —
  fully unit-testable without root, a registry, or an API server.
- **Pods map to runtime containers by name**: `p<uid12>-<container>`. The UID
  prefix makes delete+recreate trivially correct (new UID → old container is
  an orphan), keeps `rkrt ps` readable on the node, and means hand-started
  `rkrt` containers are invisible to the agent — it only ever tears down
  names it can parse as its own.
- **Restart policies run on a 1s→60s exponential backoff**, armed *before*
  each attempt so failing image pulls can't hammer the registry any more
  than crashing binaries can hot-loop. A run that survives 60s forgives the
  history. Crash-looping pods read `CrashLoopBackOff` with the phase staying
  Running, like Kubernetes; restart counts live in agent memory and reset
  with it (honest limitation, noted).
- **Status is derived, not tracked**: phase/podIP/containerStatuses are a
  pure function of (desired plan, runtime row, tracker, problem). Reports
  only fire when the computed status differs from the cached one, and use
  the cached `resourceVersion` — a 409 just means the watch already has a
  fresher object for next pass. No timestamps that drift per-tick, so no
  self-feeding update loops.
- **Binding is manual this phase** (`spec.nodeName` in the manifest) —
  exactly how Kubernetes behaves without a scheduler; Phase 4 writes the
  binding instead.
- **`valueFrom` env works now**: the loop prefetches referenced
  ConfigMaps/Secrets per namespace and resolution stays pure; missing
  references park the pod as unrunnable with the reason in its status. The
  API server also gained Secret admission (`stringData` folds into base64
  `data`), so the types' documented contract is real.
- **`command`/`args` now follow the four Kubernetes rules** (command
  overrides ENTRYPOINT, args overrides CMD, command-without-args drops the
  image CMD); `rkrt` grew `--entrypoint` to match.
- **The shared API client lives in `rk_types::client`** behind a `client`
  feature (typed surface over the `Resource` trait + raw `Value` surface for
  the CLI + the SSE parser). The agent, the CLI today, and the
  scheduler/controllers next phases all ride it; `rk-runtime` never builds
  reqwest. Round-trip tests run a real rk-api on an ephemeral port.
- **Node lifecycle**: register-or-refresh at startup (capacity from
  `/proc/meminfo` + `available_parallelism`, address auto-detected or
  configured), heartbeat re-asserts Ready every 10s, re-registers if the
  Node was deleted. `allocatable == capacity` until the scheduler's
  bin-packing math lands in Phase 4. Missed-heartbeat → NotReady is the
  Phase 5 node controller's job.
- **Called out as deferred**: multi-container pods (need the shared pod
  sandbox — Phase 6 networking), volumes, probes (pods with probes run, with
  a status note that probes aren't evaluated), and log streaming (Phase 7).
  Pod deletion is immediate (no grace period/finalizers); the agent kills
  the container after the object is gone.
