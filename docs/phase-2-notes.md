# Phase 2 — design notes

- **One revision counter rules everything.** Every store write bumps a single
  global counter (a one-row sqlite table) inside the write transaction; the
  new revision becomes the object's `metadata.resourceVersion` — stamped by
  the store, the only field of `data` it ever touches — so stored state, list
  responses and watch events can never disagree about versions.
- **The change log is a table, not a channel.** `put`/`delete` append to an
  `events` table in the same transaction that mutates `objects`. A watch
  replays rows `> since` from the table, then switches to a broadcast
  channel, deduping by revision (subscribe-then-query closes the gap). A
  watcher lapped by the channel is cut off and must re-list — the same
  contract as a Kubernetes 410.
- **`prior_revision` rides every event** (Phase 2 contract refinement to the
  Phase 0 `StoreEvent`): the store knows whether a put created or replaced,
  so the API derives `ADDED`/`MODIFIED`/`DELETED` statelessly instead of
  tracking object existence per watcher.
- **Optimistic concurrency is `Some(expected)`,** and `Some(0)` means "must
  not exist" (revisions start at 1) — POST maps to it, making create
  conflict-safe without a get-then-put race.
- **The API stores what you sent, not what it parsed.** Bodies are
  shape-checked through the typed `AnyObject` model (bad specs fail with
  serde's message), but the raw JSON is what persists — round-tripping
  through typed structs would silently drop unknown fields.
- **Scope rules live in the route layer**: namespaced kinds use
  `/api/v1/namespaces/{ns}/{plural}[/{name}]`, cluster-scoped kinds the short
  paths, and mismatches are 400s with the correct path in the message. One
  generic handler set serves all nine kinds; `{plural}` resolves through
  `ResourceKind::from_str`.
- **Server-owned fields behave like Kubernetes**: create assigns `uid` and
  `creationTimestamp` and strips client resource versions; replace (PUT)
  preserves them when the body omits them, requires the object to exist, and
  honors `resourceVersion` as the conflict token when supplied.
- **Watch is SSE** (`/api/v1/watch[/{plural}]?since=&namespace=`), one
  `WatchEvent` JSON per `data:` line plus keep-alive comments — trivially
  consumable by `rk-ctl` today and the Phase 3+ agents with a line reader,
  no HTTP/2 or chunked-JSON framing needed.
- **`rk-ctl apply` is get-then-create-or-replace**: absent objects POST,
  existing ones PUT at the observed `resourceVersion` (unless the manifest
  pins its own), so concurrent edits surface as 409s instead of silent
  overwrites. `get --watch` lists, then resumes the watch from the list's
  `resourceVersion` — the canonical list+watch pattern the controllers will
  reuse.
- **Deferred on purpose**: status subresources, field validation beyond the
  type model, multi-kind watches, and any auth (Phase 8 adds TLS + tokens) —
  none are needed by the Phase 3 agent, and the seams for them are obvious.
