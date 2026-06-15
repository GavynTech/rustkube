# Phase 8A — design notes

- **One PKI command, offline, no openssl.** `rk-ctl pki init` uses rcgen to mint
  a self-signed CA, an API-server leaf cert signed by it (with `serverAuth` EKU
  and SANs for `127.0.0.1`, `localhost`, and any `--api-name`), and a 256-bit
  bearer token from `/dev/urandom`. Keys and the token are written `0600`. It
  never contacts the cluster, so it runs before anything else is up.
- **The token check is middleware, constant-time, with one hole on purpose.**
  `auth::require_bearer` is an axum layer installed only when `authTokenFile` is
  set. It compares the presented token to the configured one in constant time
  (length-checked, XOR-accumulate) and lets `/healthz` through unauthenticated
  so liveness probes and load balancers don't need a credential.
- **TLS is the server's choice, transparently.** With a `tls:` block, rk-api
  serves HTTPS via `axum-server` + rustls; without it, the existing plaintext
  `axum::serve` path is untouched. rustls 0.23 needs a process-wide crypto
  provider chosen explicitly, so `main` installs `aws_lc_rs` before binding.
- **Auth and TLS are opt-in but all-or-nothing per cluster.** Defaults stay open
  + plaintext so every earlier phase's demo still runs unchanged; turning on the
  two config fields hardens the whole plane. The server logs loudly which mode
  it's in.
- **Every client learned the same two knobs.** `ApiClient` grew `with_auth` and
  a file-reading `connect(base, token_file, ca_file)`: the token becomes a
  sensitive default `Authorization` header on the reqwest client (so even the
  hand-built watch request carries it), and the CA PEM is added as an extra
  root so the self-signed server cert verifies.
- **Threaded through configs, not hardcoded.** The agent, scheduler and
  controllers each gained `tokenFile` / `caFile`; `rk-ctl` gained global
  `--token-file` / `--ca-file` (and `RK_TOKEN_FILE` / `RK_CA_FILE`), and now
  builds one client up front and passes it to every subcommand instead of
  reconstructing it.
- **The boundary is demonstrated, not asserted.** `phase8a-demo.sh` shows
  `/healthz` open, `/api/v1/pods` 401 without the token and 200 with it,
  plaintext HTTP refused, and the self-signed cert rejected by a client that
  lacks the CA — then runs the whole control plane (scheduler, controllers,
  agent, CLI) over HTTPS+token to schedule and run a pod.
- **Deferred to 8B/8C**: the ingress still terminates cleartext :80 — TLS
  termination from the Ingress `tls` Secret is 8B, alongside node drain. The
  install script and systemd units that wire this PKI into a real deployment
  are 8C; for now configs point at a `pki/` directory by hand.
- **Not done, and deliberately**: no token rotation, no per-identity tokens or
  RBAC (one shared token authenticates "an in-cluster component"), and no mTLS
  client-cert auth. The point of this phase is a real encrypted, authenticated
  boundary — not an identity system.
