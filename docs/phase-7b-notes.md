# Phase 7B — design notes

- **Routing is pure; the proxy is the thin I/O shell.** `ingress.rs` turns the
  cluster's Ingress and Service objects into a flat `Vec<Route>` and matches an
  incoming `(host, path)` against it — both unit-tested without a socket.
  `bin/rk-ingress.rs` is the impure half: accept a connection, read just the
  request head, ask the pure matcher for a backend, splice. Same split as every
  controller and scheduler in the repo.
- **The ingress forwards to the Service VIP, not to pods.** A `Route`'s backend
  is the service's `ClusterIP:port`, resolved at table-build time by service
  name + namespace. Once the proxy connects to that VIP, the node's Phase 6B
  nftables rules do the actual pod load-balancing (numgen-random across ready
  endpoints). So the ingress inherits readiness gating and LB for free, and
  must run on a node that carries the `rk-services` table — which is why the
  demo runs it next to the agent.
- **Paths that can't resolve yet are skipped, not faked.** If an Ingress names a
  service that has no `ClusterIP` (just created, not yet admitted), that path is
  dropped from the table rather than routed to a black hole; it reappears on the
  next reflector notification once the service is allocated. This is the same
  `clusterIP` serde-rename field that bit Phase 6B — the route builder reads
  `svc.spec.cluster_ip`, so it depends on that field deserializing.
- **Most-specific match wins**, decided by `max_by_key((host.is_some(),
  path.len()))`: a named-host rule beats a wildcard, and among equal hosts a
  longer prefix beats a shorter one. Prefix matching respects segment
  boundaries — `/api` matches `/api` and `/api/v1` but not `/apiary` — so a
  short prefix can't accidentally swallow a sibling route.
- **Hand-rolled HTTP/1.1, no framework.** The proxy reads the head up to the
  blank line (capped at 64 KB so a slow client can't pin a task open), pulls the
  `Host` header and request path with the port and query stripped, then replays
  the bytes it consumed to the backend and hands off to
  `tokio::io::copy_bidirectional`. This keeps the dependency surface the same as
  the agent's DNS and log servers, and means request bodies, keep-alive, and
  upgrades pass through untouched because we never reframe them.
- **Error responses carry a correct `Content-Length`.** The canned 404 (no
  matching route) and 502 (backend unreachable) are module constants, guarded by
  a test that asserts each declared length equals its body. This is not
  pedantry: an off-by-two on the 404 made `curl` abort with "transfer closed
  with outstanding data" (exit 18) and took the demo's `set -e` down with it —
  a malformed response is a real bug, and now a structurally testable one.
- **Deferred to Phase 8**: TLS termination. The Ingress type already carries a
  `tls` section (host list + Secret name); terminating it means a rustls
  acceptor on :443 reading the cert/key out of that Secret, and belongs with the
  rest of the "TLS everywhere" hardening work rather than bolted on here. Today
  the proxy serves cleartext :80.
