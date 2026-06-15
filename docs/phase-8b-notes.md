# Phase 8B — design notes

- **The ingress terminates TLS, but stays the same proxy.** `proxy` became
  generic over the client stream (`AsyncRead + AsyncWrite`), so the :80 path
  (plain `TcpStream`) and the :443 path (a `tokio_rustls` `TlsStream`) run the
  exact same body: read the head, match a route, splice to the backend. TLS is
  one extra `accept()` in front; everything downstream is unchanged cleartext.
- **Certificates come from the cluster, by SNI.** `ingress_tls::build_certs`
  turns each Ingress `tls` entry into `host -> CertifiedKey` by loading the
  referenced Secret's `tls.crt` / `tls.key` (base64 PEM, the same encoding the
  admission path produces), parsing them with rustls-pemfile. A `SniResolver`
  the watcher swaps out on every change answers `ClientHello` by server name,
  falling back to the single cert when a client sends no SNI.
- **One rustls config, a hot-swappable resolver.** The `ServerConfig` is built
  once at startup with the resolver; the watcher only replaces the inner cert
  map (behind an `RwLock`, recovered rather than panicked on poison). So new or
  rotated TLS Secrets take effect without rebinding the socket or restarting.
- **Same crypto-provider rule as the API.** rustls 0.23 has no default provider,
  so `rk-ingress` installs `aws_lc_rs` before building the server config — the
  one footgun that turns into a runtime panic if forgotten.
- **Drain is cordon + evict, leaning on what exists.** `rk-ctl drain` sets the
  node's `spec.unschedulable` (the scheduler's filter has honored that since
  Phase 4) and then deletes every pod on the node. The controllers recreate
  managed pods, and because the node is now cordoned they land elsewhere; on a
  one-node cluster they sit `Pending` until `uncordon`, which is itself the
  proof that cordon gates scheduling. `cordon`/`uncordon` are the halves.
- **`SchedulingDisabled` shows up where kubectl puts it.** `rk-ctl get nodes`
  appends it to the node's status when `spec.unschedulable` is set, so a
  cordoned node is visible at a glance.
- **Demonstrated end to end.** `phase8b-demo.sh` curls `https://shop.example.com`
  through the ingress (TLS 1.3, SNI-matched cert, verified against the CA;
  refused without it), shows cleartext :80 still serving, then drains the node
  (two pods evicted, recreated `Pending`) and uncordons (back to `Running`).
- **Known limitation, called out**: the ingress watches Secrets cluster-wide to
  find its TLS material — real Kubernetes scopes that with RBAC, which this
  single shared-token model doesn't have. Wildcard-host SNI matching and
  HTTP→HTTPS redirect are also not implemented; exact host match only.
- **Deferred to 8C**: wiring all of this (API + ingress TLS, tokens, a drained
  node during upgrades) into an install script, systemd units, and an
  end-to-end test that exercises the whole secured cluster at once.
