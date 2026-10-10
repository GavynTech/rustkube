# RustKube console

A light, read-only React + TypeScript dashboard for the existing RustKube API.
Includes cluster overview, searchable workloads, deployment ownership graphs,
nodes, service endpoints, and resource details with JSON manifests.

## Start locally

Requires Node.js 22.12+ and npm.

```sh
cd frontend
npm ci
npm run dev
```

Open **http://127.0.0.1:5173**. The initial view is always a clearly labeled demo
cluster. Its fixed, internally consistent sample resources are never written to
RustKube. Reloading the browser returns to demo mode.

## Connect a real cluster

The existing root-free control plane can be started with `make dev-up` from the
repository root. Running actual containers still requires the existing VM setup
documented in the root README.

1. Copy `.env.example` to `.env.local` in this directory.
2. Set `RK_API_URL` to the API origin. The default is `http://127.0.0.1:6443`.
3. For TLS and bearer-token authentication, also set `RK_TOKEN_FILE` and
   `RK_CA_FILE` to absolute paths on the machine serving the dashboard.
4. Restart `npm run dev`, open **Connection**, and select **Connect to API**.

```dotenv
RK_API_URL=https://127.0.0.1:6443
RK_TOKEN_FILE=/absolute/path/to/pki/token
RK_CA_FILE=/absolute/path/to/pki/ca.crt
```

The local server reads these files and adds the bearer token upstream. Do not
prefix credentials with `VITE_`: those variables are public browser values. The
proxy verifies TLS, including the server hostname, and listens only on loopback.
There are no browser token inputs or stored credentials.

## Data contract and behavior

- The live adapter requests `/healthz`, `/version`, and `/api/v1/{kind}` for
  deployments, replicasets, pods, nodes, services, and namespaces. Rust backend
  endpoints and wire names are unchanged.
- After each request cycle, live data refreshes after 10 seconds (or via Refresh);
  requests time out after 8 seconds. No overlapping polling requests. Failed
  refreshes retain the last snapshot with a prominent stale-data warning. Live
  failures never trigger an automatic fallback to demo data.
- Namespaced lists honor the selected namespace. Nodes and cluster capacity
  always include the entire cluster and say so in the interface.
- Deployment graphs follow controlling owner UIDs and namespace boundaries.
  They do not infer ownership from name prefixes or matching labels.
- Service matching follows namespace + non-empty selectors. Ready endpoints
  require a Running pod, non-empty ready container statuses, and a pod IP.
- Resource reservations sum requests for scheduled, non-terminal pods and are
  compared with reported node capacity. These are **not measured utilization**.
  Unknown capacity is displayed explicitly. API health does not imply every
  cluster component is healthy.
- Resource JSON is shown as received. Secret and ConfigMap resources are never
  fetched. Workload JSON can still contain literal environment values configured
  by the cluster owner; the console is intended for trusted local operators.

## Checks and build

```sh
npm test
npm run build
npm run preview
```

The production bundle is written to `dist/`. Preview serves it on
**http://127.0.0.1:4173** with demo mode available. Static preview has no API proxy.
Tests cover ownership, selectors, namespace filtering, quantities, readiness,
API errors, and user navigation and connection behavior.

## Deployment boundary

This release is a **local single-cluster console**, not a public management
service. Its Vite development proxy only allows GETs to the listed read
endpoints; it rejects mutations and requests for other resource kinds.

To host `dist/` with live data, provide a separately authenticated same-origin
proxy at `/cluster-api`, retaining the same endpoint allowlist, upstream TLS
verification, and server-side credentials. Authenticate access to that proxy
before exposing it beyond localhost. Never expose the Vite development server
or its privileged upstream token as a public service.

Deployment controls, container logs, historical telemetry, user accounts, and
multi-cluster management are intentionally deferred.
