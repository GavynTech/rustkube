# Phase 1 — design notes

- **Process model is youki-style, daemonless.** `rkrt run` prepares everything,
  spawns `/proc/self/exe shim` with `unshare(UTS|IPC|NET|PID)` in `pre_exec`,
  and exits; the shim forks PID 1, reaps it, records the exit status, and is
  the only thing left running per container. No daemon, no Docker.
- **The shim stays in the host PID and mount namespaces on purpose**: the
  parent must attach it to the cgroup and reach its netns via `/proc/<pid>`,
  and the shim must write state files to host paths. Only its *children* land
  in the new PID namespace; only PID 1 unshares the mount namespace, because
  `pivot_root` rewrites the root of every process sharing it.
- **Stdin is the startup gate.** The shim blocks reading one byte; the parent
  writes it only after cgroup attach + veth wiring, so the workload cannot
  outrun its limits or its network. EOF instead of a byte means the parent
  died — the shim aborts rather than start unconfined. Post-pivot, PID 1
  re-points stdin at `/dev/null`.
- **`/proc` is mounted by PID 1, not the shim** — a `/proc` mounted from the
  host PID namespace would show host processes. Same logic puts the minimal
  `/dev` (tmpfs + mknod nodes + pts + shm + fd symlinks) and the
  `pivot_root(".", ".")` + lazy-unmount idiom inside PID 1.
- **Every blob is digest-verified before storage** (sha2 against the manifest
  descriptor); blobs and extracted layers are content-addressed under
  `/var/lib/rustkube`, shared across images, extract-once via temp dir +
  rename. Multi-arch references resolve through `oci-client`'s linux/amd64
  resolver.
- **Whiteout conversion happens at extraction**: AUFS `.wh.*` entries become
  overlayfs 0:0 char devices, `.wh..wh..opq` becomes the
  `trusted.overlay.opaque` xattr — both root-only, matching the VM-as-root
  deployment. `lowerdir` is the manifest layer list **reversed** (overlay
  reads top-first).
- **Networking is deliberately minimal**: one veth pair per container, a /30
  per container carved from `10.42.0.0/24` (host .1, container .2), default
  route via the host end. The host curls the container over the connected
  route. No bridge, no NAT, no DNS, no outbound — that is Phase 6 (`rk-net`);
  the container side is configured by a scratch thread that `setns`es into
  the shim's netns and runs rtnetlink on a current-thread runtime.
- **cgroups v2 only**: `rustkube/<id>` with `cpu.max` (millicores × 100, 1ms
  floor) and `memory.max` (bytes), controllers enabled top-down, shim attached
  before release so the whole tree inherits.
- **Reused from Phase 0 / rk-types**: `ImageRef` parsing feeds `oci-client`
  references; CPU/memory limits parse through `rk_types::quantity`, so `rkrt`
  flags speak the same `500m`/`256Mi` grammar as the future manifests.
- **Deviations from the spec text, called out**: `sha2` and `xattr` joined the
  workspace deps (digest verification and opaque markers need them);
  ConfigMap/Secret mounts and named-`USER` resolution are deferred to the
  phases that introduce those resources (nginx runs as root, numeric users
  unaffected); image `EXPOSE` metadata is informational only.
