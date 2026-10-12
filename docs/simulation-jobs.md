# Finite simulation Jobs and directory volumes

RustKube Jobs run indexed, finite batches on the existing CPU/memory scheduler.
A Job's `completions` is the number of indexes, `parallelism` limits simultaneous
attempts, and `backoffLimit` is the maximum retries **per index**, after its first
attempt. These are RustKube semantics, not full Kubernetes Job compatibility.
The template must set `restartPolicy: Never`: the Job controller owns retries.

Workers receive `RK_JOB_INDEX` (zero-based), `RK_JOB_ATTEMPT` (zero-based), and
`RK_JOB_UID`. Use an index to select a parameter tuple, and the UID plus index as
the result key. Completed indexes are not rerun. Completion and failed-attempt
records persist in the Job's SQLite-backed status. Reapplying an unchanged Job
preserves progress. The spec is immutable: use a new name for a new experiment.

A failed index that exhausts its budget fails the entire Job and cancels remaining
active pods. Attempt pods are kept for inspection until the Job is deleted; Job
deletion garbage-collects its pods. Keep attempt pods while a Job is running.
Removing one before its result has been recorded may cause it to run again.
Retries cover observed failed pods; image pulls or unschedulable pods can remain
pending indefinitely. There is no execution deadline in this version.

## Storage

- `hostPath`: bind an **existing directory** from the worker host. Supports
  `readOnly`. Results survive container, pod and Job deletion. Pin the workload
  to that host using `rustkube.io/hostname`; local files do not follow a Job to
  another node. This is not replicated storage or a backup.
- `emptyDir`: scratch space scoped to the pod, preserved during container
  recreation on the same node and removed when the agent tears down the pod.
  A new Job attempt is a new pod and gets fresh scratch space. Store cross-attempt
  checkpoints in `hostPath`, not `emptyDir`.
- ConfigMap/Secret file volumes and persistent-volume claims are not implemented.
  Environment references remain available. Only directory mounts are supported.

Mount paths must be normalized absolute paths, cannot overlap, and cannot be
under `/proc`, `/sys` or `/dev`. Destination symlinks in images are rejected.
Host directories are trusted administrative inputs; a writable hostPath lets the
workload modify that directory. Nested host mounts are not recursively imported.

Checkpoint recovery is the application's responsibility. A failed or disconnected
node can continue executing briefly while a replacement starts; this system does
not promise exactly-once execution. Use idempotent work and atomic result writes.
A returning agent stops attempts marked lost by the node controller.

## Home-machine smoke test

After installing the updated runtime on the Linux host, prepare directories there:

```sh
sudo mkdir -p /srv/rustkube/simulation/code /srv/rustkube/simulation/results
sudo cp examples/simulation/sweep.py /srv/rustkube/simulation/code/
rk-ctl get nodes
```

Set the `rustkube.io/hostname` value in `examples/simulation/job.yaml` to the actual
node name. Then, from a machine with API access:

```sh
rk-ctl apply -f examples/simulation/job.yaml
rk-ctl get jobs
rk-ctl get pods
rk-ctl get job rabi-sweep -o yaml
```

Expect nine successful indexes and `Complete`, with at most two active attempts.
Results are under `/srv/rustkube/simulation/results/<job-uid>/` on the worker.
Reapply the manifest: it should remain Complete without new attempts. Inspect
logs with `rk-ctl logs <pod-name>`. Delete the Job to clean up its pods:

```sh
rk-ctl delete job rabi-sweep
```

The example uses Python's standard library to compute an **ideal two-level Rabi
formula**, solely to verify orchestration and result handling. It is not a Heron
model, a performance benchmark, or a substitute for the REU solver. Replace it
with the validated physics worker and adjust resource requests before real sweeps.
Use immutable image digests and record solver versions, calibration snapshots,
parameters, seeds and tolerances for research runs.

## Verification

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

The normal suite tests the Job controller through the real HTTP API and SQLite,
including retries, completion, reapply, history cleanup and garbage collection.
It also tests directory preparation and mount-path rejection without root.

The actual bind-mount/read-only test is ignored by default. On a disposable Linux
runtime VM with mount privileges, build the test and run it with the required
privileges (using the installed toolchain's absolute path if sudo resets PATH):

```sh
sudo /path/to/cargo test -p rk-runtime --offline \
  volumes::privileged_tests::bind_mounts_enforce_read_only_and_persist_results \
  -- --ignored --nocapture
```

Its mounts occur in a subprocess's private mount namespace. Run the example Job
on that VM as well to verify the full image-to-worker path. CPU/GPU tuning and
quantum performance measurements remain work for the home machine.
