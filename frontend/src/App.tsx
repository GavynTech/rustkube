import { useEffect, useState } from "react";
import {
  Activity,
  ArrowRight,
  ArrowUpRight,
  Box,
  Cable,
  Check,
  ChevronDown,
  ChevronRight,
  CircleHelp,
  CircleOff,
  Command,
  Cpu,
  Database,
  ExternalLink,
  FlaskConical,
  GitBranch,
  Layers,
  LayoutDashboard,
  Menu,
  Network,
  RefreshCw,
  Search,
  Server,
  Settings2,
  ShieldCheck,
  TriangleAlert,
  X,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import {
  Badge,
  CapacityBar,
  EmptyState,
  Pipeline,
  ResourceDrawer,
  ResourceTable,
} from "./components";
import {
  age,
  deploymentTree,
  inNamespace,
  namespaceOf,
  readyPod,
  reservations,
  resourceKey,
  searchResources,
  serviceEndpoints,
  statusOf,
} from "./domain";
import type { Deployment, Mode, Page, Resource, Snapshot } from "./types";
import { useCluster } from "./useCluster";

const pages: { id: Page; label: string; icon: LucideIcon }[] = [
  { id: "overview", label: "Overview", icon: LayoutDashboard },
  { id: "workloads", label: "Workloads", icon: Layers },
  { id: "nodes", label: "Nodes", icon: Server },
  { id: "networking", label: "Networking", icon: Network },
];
const pageFromHash = (): Page =>
  [...pages.map((p) => p.id), "connection"].includes(location.hash.slice(1))
    ? (location.hash.slice(1) as Page)
    : "overview";

export default function App() {
  const cluster = useCluster();
  const [page, setPage] = useState<Page>(pageFromHash);
  const [namespace, setNamespace] = useState("all");
  const [query, setQuery] = useState("");
  const [inspected, setInspected] = useState<Resource | null>(null);
  const [mobileNav, setMobileNav] = useState(false);
  const { snapshot, mode, loading, error } = cluster;
  useEffect(() => {
    const onHash = () => {
      setPage(pageFromHash());
      setQuery("");
      setMobileNav(false);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  const navigate = (next: Page) => {
    location.hash = next;
    setPage(next);
    setQuery("");
    setMobileNav(false);
  };
  const switchMode = (next: Mode) => {
    setNamespace("all");
    setQuery("");
    setInspected(null);
    cluster.switchMode(next);
  };
  const namespaces = [
    ...new Set([
      ...(snapshot?.namespaces.map((n) => n.metadata.name) ?? []),
      ...[
        ...(snapshot?.pods ?? []),
        ...(snapshot?.deployments ?? []),
        ...(snapshot?.services ?? []),
      ].map(namespaceOf),
    ]),
  ].sort();
  const activeNamespace = namespaces.includes(namespace) ? namespace : "all";
  const label =
    page === "connection"
      ? "Connection"
      : pages.find((p) => p.id === page)!.label;
  const connected = mode === "live" && !!snapshot && !error;
  const scope = snapshot
    ? inNamespace(snapshot.deployments, activeNamespace)
    : [];

  return (
    <div className="app-shell">
      {mobileNav && (
        <button
          className="nav-scrim"
          aria-label="Close navigation"
          onClick={() => setMobileNav(false)}
        />
      )}
      <aside
        className={`sidebar ${mobileNav ? "open" : ""}`}
        aria-label="Main navigation"
      >
        <a
          className="brand"
          href="#overview"
          onClick={() => navigate("overview")}
        >
          <span className="brand-mark">
            <Box size={24} strokeWidth={1.8} />
          </span>
          <span>
            rustkube<span className="brand-period">.</span>
          </span>
          <span className="edition">CONSOLE</span>
        </a>
        <button
          className="cluster-selector"
          onClick={() => navigate("connection")}
        >
          <span className="cluster-symbol">
            <Layers size={19} />
          </span>
          <span>
            <strong>
              {mode === "demo" ? "Demo cluster" : "Local cluster"}
            </strong>
            <small>
              {mode === "demo"
                ? "Explore RustKube"
                : connected
                  ? "API connected"
                  : "API connection"}
            </small>
          </span>
          <ChevronDown size={15} />
        </button>
        <div className="nav-section-title">WORKSPACE</div>
        <nav>
          {pages.map(({ id, label: title, icon: Icon }) => (
            <a
              key={id}
              href={`#${id}`}
              onClick={() => navigate(id)}
              className={`nav-link ${page === id ? "active" : ""}`}
              aria-current={page === id ? "page" : undefined}
            >
              <Icon size={18} />
              <span>{title}</span>
              {snapshot && id !== "overview" && (
                <span className="nav-count">
                  {id === "workloads"
                    ? scope.length
                    : id === "nodes"
                      ? snapshot.nodes.length
                      : inNamespace(snapshot.services, activeNamespace).length}
                </span>
              )}
            </a>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="sidebar-note">
            <span className="sidebar-note-icon">
              <GitBranch size={18} />
            </span>
            <strong>
              Small footprint.
              <br />
              Full control.
            </strong>
            <p>
              Your infrastructure,
              <br />
              orchestrated in Rust.
            </p>
            <a
              href="https://github.com/GavynTech/rustkube#architecture"
              target="_blank"
              rel="noreferrer"
            >
              Explore the architecture <ArrowUpRight size={14} />
            </a>
          </div>
          <a
            className={`nav-link ${page === "connection" ? "active" : ""}`}
            href="#connection"
            onClick={() => navigate("connection")}
            aria-current={page === "connection" ? "page" : undefined}
          >
            <Settings2 size={18} />
            <span>Connection</span>
          </a>
          <a
            className="nav-link"
            href="https://github.com/GavynTech/rustkube#readme"
            target="_blank"
            rel="noreferrer"
          >
            <CircleHelp size={18} />
            <span>Documentation</span>
            <ExternalLink size={13} />
          </a>
          <div className="workspace-profile">
            <span className="profile-icon">
              <Command size={19} />
            </span>
            <span>
              <strong>Local workspace</strong>
              <small>Read-only access</small>
            </span>
            <ShieldCheck size={16} />
          </div>
        </div>
      </aside>
      <div className="main-shell">
        <header className="topbar">
          <button
            className="icon-button mobile-toggle"
            onClick={() => setMobileNav((v) => !v)}
            aria-label="Toggle navigation"
            aria-expanded={mobileNav}
          >
            <Menu size={21} />
          </button>
          <div className="breadcrumbs">
            <span>Workspace</span>
            <ChevronRight size={13} />
            <strong>{label}</strong>
          </div>
          <div className="topbar-right">
            <span className="access-label">
              <ShieldCheck size={14} />
              Read-only
            </span>
            <span className="topbar-divider" />
            <span className={`source-pill ${mode}`}>
              <span className="status-dot" />
              {mode === "demo"
                ? "Demo environment"
                : connected
                  ? "Live cluster"
                  : error
                    ? "Disconnected"
                    : "Connecting"}
            </span>
          </div>
        </header>
        <main>
          <div className="page-heading">
            <div>
              <div className="eyebrow">RUSTKUBE CONTROL CONSOLE</div>
              <h1>{page === "overview" ? "Cluster overview" : label}</h1>
              <p>
                {page === "overview"
                  ? "A clear view of your cluster. From desired state to running workloads."
                  : page === "workloads"
                    ? "Inspect your applications, their replicas, and the pods behind them."
                    : page === "nodes"
                      ? "Understand where your workloads run and how resources are reserved."
                      : page === "networking"
                        ? "Follow your services from a stable cluster IP to ready pod endpoints."
                        : "Choose how this console reads your cluster."}
              </p>
            </div>
            <div className="heading-actions">
              {page !== "connection" && (
                <>
                  <button
                    className={`button ${loading ? "loading" : ""}`}
                    disabled={loading || mode === "demo"}
                    title={
                      mode === "demo"
                        ? "Demo data is a fixed sample snapshot"
                        : "Refresh cluster data"
                    }
                    onClick={cluster.refresh}
                  >
                    <RefreshCw size={15} className={loading ? "spin" : ""} />
                    Refresh
                  </button>
                  <button
                    className="button primary"
                    onClick={() => navigate("connection")}
                  >
                    <Cable size={16} />
                    {mode === "demo" ? "Connect cluster" : "Connection"}
                  </button>
                </>
              )}
            </div>
          </div>
          {mode === "demo" ? (
            <div className="demo-banner">
              <FlaskConical size={17} />
              <div>
                <strong>You’re exploring a demo cluster.</strong>
                <span> Sample resources show how RustKube works.</span>
              </div>
              <button onClick={() => navigate("connection")}>
                Connect your cluster <ArrowRight size={15} />
              </button>
            </div>
          ) : error ? (
            <div className="notice error" role="alert">
              <TriangleAlert size={19} />
              <div>
                <strong>
                  {snapshot
                    ? "Connection lost · data is stale"
                    : "Unable to connect"}
                </strong>
                <p>{error}</p>
              </div>
              <button
                className="button"
                disabled={loading}
                onClick={cluster.refresh}
              >
                Retry
              </button>
            </div>
          ) : (
            <div className="live-banner">
              <Activity size={16} />
              <span>
                {snapshot
                  ? "Connected to RustKube. Resource data refreshes automatically."
                  : "Connecting to your RustKube API…"}
              </span>
              <span>{snapshot ? "10s refresh" : "Please wait"}</span>
            </div>
          )}
          {page !== "connection" && snapshot && (
            <div className="scope-bar">
              <div className="scope-label">
                <Database size={14} />
                <span>
                  {mode === "demo" ? "demo-cluster" : "local-cluster"}
                </span>
                <span className="slash">/</span>
                <label htmlFor="namespace">Namespace</label>
                <div className="select-wrap">
                  <select
                    id="namespace"
                    value={activeNamespace}
                    onChange={(e) => {
                      setNamespace(e.target.value);
                      setInspected(null);
                    }}
                    disabled={page === "nodes"}
                  >
                    <option value="all">All namespaces</option>
                    {namespaces.map((ns) => (
                      <option key={ns} value={ns}>
                        {ns}
                      </option>
                    ))}
                  </select>
                  <ChevronDown size={13} />
                </div>
                {page === "nodes" && (
                  <span className="scope-note">Nodes are cluster scoped</span>
                )}
              </div>
              <span className="snapshot-time">
                {mode === "demo"
                  ? "Sample snapshot"
                  : `Updated ${new Date(snapshot.fetchedAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}`}
                <span className={`status-dot ${error ? "warning" : "good"}`} />
              </span>
            </div>
          )}
          {page === "connection" ? (
            <Connection
              mode={mode}
              connected={connected}
              loading={loading}
              snapshot={snapshot}
              switchMode={switchMode}
              navigate={navigate}
            />
          ) : !snapshot ? (
            <div className="panel">
              <EmptyState
                title={
                  loading
                    ? "Loading your cluster…"
                    : "Your cluster is not connected"
                }
              >
                {loading
                  ? "Reading resources from the RustKube API."
                  : "Open Connection to configure your API, or explore the demo environment."}
              </EmptyState>
              <div className="empty-actions">
                <button
                  className="button"
                  onClick={() => navigate("connection")}
                >
                  Connection settings
                </button>
                {!loading && (
                  <button
                    className="button primary"
                    onClick={() => switchMode("demo")}
                  >
                    Explore demo
                  </button>
                )}
              </div>
            </div>
          ) : page === "overview" ? (
            <Overview
              snapshot={snapshot}
              namespace={activeNamespace}
              onInspect={setInspected}
              navigate={navigate}
            />
          ) : page === "workloads" ? (
            <Workloads
              snapshot={snapshot}
              namespace={activeNamespace}
              query={query}
              setQuery={setQuery}
              onInspect={setInspected}
            />
          ) : page === "nodes" ? (
            <Nodes
              snapshot={snapshot}
              query={query}
              setQuery={setQuery}
              onInspect={setInspected}
            />
          ) : (
            <Networking
              snapshot={snapshot}
              namespace={activeNamespace}
              query={query}
              setQuery={setQuery}
              onInspect={setInspected}
            />
          )}
          <footer className="page-footer">
            <span>
              <Box size={13} />
              RustKube console<span className="footer-dot">·</span>
              {snapshot ? `API v${snapshot.version}` : "API offline"}
            </span>
            <span>Built on Rust. Designed for clarity.</span>
          </footer>
        </main>
      </div>
      {inspected && snapshot && (
        <ResourceDrawer
          resource={inspected}
          snapshot={snapshot}
          onClose={() => setInspected(null)}
          onInspect={setInspected}
        />
      )}
    </div>
  );
}

function Overview({
  snapshot,
  namespace,
  onInspect,
  navigate,
}: {
  snapshot: Snapshot;
  namespace: string;
  onInspect: (r: Resource) => void;
  navigate: (page: Page) => void;
}) {
  const deployments = inNamespace(snapshot.deployments, namespace);
  const pods = inNamespace(snapshot.pods, namespace);
  const services = inNamespace(snapshot.services, namespace);
  const [selected, setSelected] = useState("");
  const deployment =
    deployments.find((d) => resourceKey(d) === selected) ?? deployments[0];
  const readyNodes = snapshot.nodes.filter(
    (n) => n.status?.condition === "Ready",
  ).length;
  const running = pods.filter((p) => p.status?.phase === "Running").length;
  const readyDeployments = deployments.filter(
    (d) => statusOf(d).tone === "good",
  ).length;
  const pending = pods.filter(
    (p) => (p.status?.phase ?? "Pending") === "Pending",
  ).length;
  const failed = pods.filter((p) => p.status?.phase === "Failed").length;
  const usage = reservations(snapshot.nodes, snapshot.pods);
  const issues: Resource[] = [
    ...snapshot.nodes.filter((n) => n.status?.condition !== "Ready"),
    ...pods.filter((p) => !readyPod(p) && p.status?.phase !== "Succeeded"),
  ];
  const stats = [
    {
      label: "Running pods",
      value: running,
      total: pods.length,
      sub: `${pending} pending · ${failed} failed`,
      icon: Box,
      color: "blue",
      target: "workloads" as Page,
    },
    {
      label: "Deployments",
      value: deployments.length,
      sub: `${readyDeployments} available · ${deployments.length - readyDeployments} progressing or inactive`,
      icon: Layers,
      color: "purple",
      target: "workloads" as Page,
    },
    {
      label: "Ready nodes",
      value: readyNodes,
      total: snapshot.nodes.length,
      sub: `${snapshot.nodes.length} registered in this cluster`,
      icon: Server,
      color: "teal",
      target: "nodes" as Page,
    },
    {
      label: "Services",
      value: services.length,
      sub: `${services.reduce((sum, service) => sum + serviceEndpoints(service, pods).length, 0)} ready service endpoints`,
      icon: Network,
      color: "orange",
      target: "networking" as Page,
    },
  ];
  return (
    <>
      <div className="stats-grid">
        {stats.map(
          ({ label, value, total, sub, icon: Icon, color, target }) => (
            <button
              className="stat-card"
              key={label}
              onClick={() => navigate(target)}
            >
              <div className="stat-top">
                <span>{label}</span>
                <span className={`stat-icon ${color}`}>
                  <Icon size={18} />
                </span>
              </div>
              <div className="stat-number">
                {value}
                {total !== undefined && <span>/ {total}</span>}
                <ArrowUpRight size={17} />
              </div>
              <div className="stat-bottom">{sub}</div>
            </button>
          ),
        )}
      </div>
      <div className="overview-middle">
        <section className="panel pipeline-panel">
          <div className="panel-heading">
            <div>
              <h2>
                <GitBranch size={18} />
                Workload pipeline
              </h2>
              <p>Follow a deployment from definition to placement.</p>
            </div>
            {deployment && (
              <div className="select-wrap pipeline-selector">
                <select
                  aria-label="Deployment to visualize"
                  value={resourceKey(deployment)}
                  onChange={(e) => setSelected(e.target.value)}
                >
                  {deployments.map((d) => (
                    <option key={resourceKey(d)} value={resourceKey(d)}>
                      {d.metadata.name}
                    </option>
                  ))}
                </select>
                <ChevronDown size={13} />
              </div>
            )}
          </div>
          {deployment ? (
            <>
              <Pipeline
                deployment={deployment}
                snapshot={snapshot}
                onInspect={onInspect}
              />
              <div className="pipeline-footer">
                <span>
                  <span className="status-dot good" />
                  Ready
                </span>
                <span>
                  <span className="status-dot warning" />
                  Needs attention
                </span>
                <span className="pipeline-hint">
                  Select any resource to inspect <ArrowUpRight size={12} />
                </span>
              </div>
            </>
          ) : (
            <EmptyState title="No deployments in this namespace">
              Deploy an application with rk-ctl to see its resource
              relationships here.
            </EmptyState>
          )}
        </section>
        <section className="panel capacity-panel">
          <div className="panel-heading">
            <div>
              <h2>
                <Cpu size={18} />
                Cluster capacity
              </h2>
              <p>Reservations across all namespaces</p>
            </div>
          </div>
          <div className="capacity-body">
            <CapacityBar
              label="CPU requests"
              value={usage.cpu}
              capacity={usage.cpuCapacity}
            />
            <CapacityBar
              label="Memory requests"
              value={usage.memory}
              capacity={usage.memoryCapacity}
              memory
            />
            <div className="capacity-note">
              <CircleHelp size={14} />
              <span>
                Requested resources, not live utilization. Includes registered
                node capacity.
              </span>
            </div>
          </div>
        </section>
      </div>
      <section className="panel">
        <div className="panel-heading">
          <div>
            <h2>
              Deployments{" "}
              <span className="count-chip">{deployments.length}</span>
            </h2>
            <p>The desired state of your applications.</p>
          </div>
          <button className="text-button" onClick={() => navigate("workloads")}>
            View workloads <ArrowRight size={15} />
          </button>
        </div>
        <ResourceTable
          resources={deployments.slice(0, 6)}
          snapshot={snapshot}
          onInspect={onInspect}
        />
      </section>
      <div className="overview-bottom">
        <section className="panel">
          <div className="panel-heading">
            <div>
              <h2>
                <Server size={17} />
                Node health
              </h2>
            </div>
            <button className="text-button" onClick={() => navigate("nodes")}>
              All nodes <ArrowRight size={14} />
            </button>
          </div>
          <div className="node-health-list">
            {snapshot.nodes.length ? (
              snapshot.nodes.slice(0, 4).map((node) => (
                <button key={resourceKey(node)} onClick={() => onInspect(node)}>
                  <span className="node-mini-icon">
                    <Server size={18} />
                  </span>
                  <span className="node-health-name">
                    <strong>{node.metadata.name}</strong>
                    <small className="mono">
                      {node.status?.address ?? "No address"}
                    </small>
                  </span>
                  <span className="node-pod-count">
                    {
                      snapshot.pods.filter(
                        (p) => p.spec?.nodeName === node.metadata.name,
                      ).length
                    }{" "}
                    pods
                  </span>
                  <Badge tone={statusOf(node).tone}>
                    {statusOf(node).label}
                  </Badge>
                  <ChevronRight size={15} />
                </button>
              ))
            ) : (
              <EmptyState title="No nodes registered" />
            )}
          </div>
        </section>
        <section className="panel attention-panel">
          <div className="panel-heading">
            <div>
              <h2>
                <Activity size={18} />
                Needs attention{" "}
                <span className="count-chip">{issues.length}</span>
              </h2>
            </div>
            <Badge tone={issues.length ? "warning" : "good"}>
              {issues.length ? "Review suggested" : "No issues detected"}
            </Badge>
          </div>
          <div className="attention-list">
            {issues.length ? (
              issues.slice(0, 4).map((resource) => (
                <button
                  key={resourceKey(resource)}
                  onClick={() => onInspect(resource)}
                >
                  <span
                    className={`attention-icon ${resource.kind === "Node" ? "bad" : "warning"}`}
                  >
                    <TriangleAlert size={17} />
                  </span>
                  <span>
                    <strong>
                      {resource.kind === "Node"
                        ? `${resource.metadata.name} is not ready`
                        : `${resource.metadata.name} · ${statusOf(resource).label}`}
                    </strong>
                    <small>
                      {resource.kind === "Node"
                        ? "Check the node agent and its last heartbeat."
                        : resource.kind === "Pod"
                          ? (resource.status?.message ??
                            "Inspect this pod for container readiness and status.")
                          : ""}
                    </small>
                  </span>
                  <ChevronRight size={15} />
                </button>
              ))
            ) : (
              <div className="healthy-message">
                <ShieldCheck size={25} />
                <strong>Resources look healthy</strong>
                <p>No node or pod issues in this view.</p>
              </div>
            )}
          </div>
        </section>
      </div>
    </>
  );
}

function SearchBox({
  query,
  setQuery,
  placeholder,
}: {
  query: string;
  setQuery: (q: string) => void;
  placeholder: string;
}) {
  return (
    <div className="search-box">
      <Search size={16} />
      <input
        type="search"
        aria-label={placeholder}
        placeholder={placeholder}
        value={query}
        onChange={(e) => setQuery(e.target.value)}
      />
      {query && (
        <button
          className="icon-button small"
          aria-label="Clear search"
          onClick={() => setQuery("")}
        >
          <X size={14} />
        </button>
      )}
    </div>
  );
}
type ListProps = {
  snapshot: Snapshot;
  query: string;
  setQuery: (q: string) => void;
  onInspect: (r: Resource) => void;
};
function Workloads({
  snapshot,
  namespace,
  query,
  setQuery,
  onInspect,
}: ListProps & { namespace: string }) {
  const [tab, setTab] = useState<"deployments" | "pods">("deployments");
  const [status, setStatus] = useState("all");
  const resources = inNamespace<Resource>(snapshot[tab], namespace);
  const statuses = [...new Set(resources.map((r) => statusOf(r).label))].sort();
  const activeStatus = statuses.includes(status) ? status : "all";
  const filtered = searchResources(resources, query).filter(
    (r) => activeStatus === "all" || statusOf(r).label === activeStatus,
  );
  const [treeKey, setTreeKey] = useState("");
  const deployments = inNamespace(snapshot.deployments, namespace);
  const deployment =
    deployments.find((d) => resourceKey(d) === treeKey) ?? deployments[0];
  return (
    <>
      <section className="panel">
        <div className="resource-toolbar">
          <div className="tabs">
            <button
              aria-pressed={tab === "deployments"}
              className={tab === "deployments" ? "active" : ""}
              onClick={() => {
                setTab("deployments");
                setStatus("all");
              }}
            >
              <Layers size={15} />
              Deployments
              <span className="count-chip">{deployments.length}</span>
            </button>
            <button
              aria-pressed={tab === "pods"}
              className={tab === "pods" ? "active" : ""}
              onClick={() => {
                setTab("pods");
                setStatus("all");
              }}
            >
              <Box size={15} />
              Pods
              <span className="count-chip">
                {inNamespace(snapshot.pods, namespace).length}
              </span>
            </button>
          </div>
          <div className="table-filters">
            <SearchBox
              query={query}
              setQuery={setQuery}
              placeholder="Search workloads…"
            />
            <div className="select-wrap">
              <select
                aria-label="Filter by workload status"
                value={activeStatus}
                onChange={(e) => setStatus(e.target.value)}
              >
                <option value="all">All statuses</option>
                {statuses.map((s) => (
                  <option key={s}>{s}</option>
                ))}
              </select>
              <ChevronDown size={13} />
            </div>
          </div>
        </div>
        <ResourceTable
          resources={filtered}
          snapshot={snapshot}
          onInspect={onInspect}
        />
        <div className="table-footer">
          Showing {filtered.length} of {resources.length} {tab}
          <span>Inspect a resource to see its status and manifest.</span>
        </div>
      </section>
      {deployment && (
        <section className="panel">
          <div className="panel-heading">
            <div>
              <h2>
                <GitBranch size={18} />
                Deployment explorer
              </h2>
              <p>Ownership relationships reported by the control plane.</p>
            </div>
            <div className="select-wrap">
              <select
                aria-label="Deployment to visualize"
                value={resourceKey(deployment)}
                onChange={(e) => setTreeKey(e.target.value)}
              >
                {deployments.map((d) => (
                  <option key={resourceKey(d)} value={resourceKey(d)}>
                    {d.metadata.name}
                  </option>
                ))}
              </select>
              <ChevronDown size={13} />
            </div>
          </div>
          <Pipeline
            deployment={deployment}
            snapshot={snapshot}
            onInspect={onInspect}
          />
          <div className="pipeline-footer">
            <span>
              {deploymentTree(deployment, snapshot).nodes.length} assigned nodes
            </span>
            <span className="pipeline-hint">
              Click a resource to inspect its manifest
            </span>
          </div>
        </section>
      )}
    </>
  );
}
function Nodes({ snapshot, query, setQuery, onInspect }: ListProps) {
  const nodes = searchResources(snapshot.nodes, query);
  return (
    <>
      <div className="section-toolbar">
        <h2>
          Registered nodes{" "}
          <span className="count-chip">{snapshot.nodes.length}</span>
        </h2>
        <SearchBox
          query={query}
          setQuery={setQuery}
          placeholder="Search nodes…"
        />
      </div>
      <div className="node-card-grid">
        {nodes.map((node) => {
          const usage = reservations([node], snapshot.pods);
          const pods = snapshot.pods.filter(
            (p) => p.spec?.nodeName === node.metadata.name,
          );
          return (
            <section className="panel node-card" key={resourceKey(node)}>
              <div className="node-card-top">
                <span className="large-resource-icon">
                  <Server size={22} />
                </span>
                <Badge tone={statusOf(node).tone}>{statusOf(node).label}</Badge>
              </div>
              <button
                className="node-card-name"
                onClick={() => onInspect(node)}
              >
                {node.metadata.name}
                <ArrowUpRight size={17} />
              </button>
              <p className="mono muted">
                {node.status?.address ?? "No address reported"}
              </p>
              <div className="node-facts">
                <span>
                  <Box size={14} />
                  {pods.length} pods
                </span>
                <span>
                  {node.spec?.unschedulable
                    ? "Scheduling disabled"
                    : "Scheduling enabled"}
                </span>
              </div>
              <CapacityBar
                label="CPU requests"
                value={usage.cpu}
                capacity={usage.cpuCapacity}
              />
              <CapacityBar
                label="Memory requests"
                value={usage.memory}
                capacity={usage.memoryCapacity}
                memory
              />
              <div className="node-card-footer">
                <span>
                  Heartbeat{" "}
                  {node.status?.lastHeartbeat
                    ? `${age(node.status.lastHeartbeat)} ago`
                    : "unknown"}
                </span>
                <button className="text-button" onClick={() => onInspect(node)}>
                  Inspect <ArrowRight size={14} />
                </button>
              </div>
            </section>
          );
        })}
      </div>
      {!nodes.length && (
        <section className="panel">
          <EmptyState title="No nodes found" />
        </section>
      )}
      <div className="subtle-note">
        <CircleHelp size={15} />
        Reservations are calculated from non-terminal pods assigned to each
        node. They are not real-time CPU or memory measurements.
      </div>
    </>
  );
}
function Networking({
  snapshot,
  namespace,
  query,
  setQuery,
  onInspect,
}: ListProps & { namespace: string }) {
  const services = inNamespace(snapshot.services, namespace);
  const filtered = searchResources(services, query);
  return (
    <>
      <div className="network-explainer">
        <span className="network-explainer-icon">
          <Network size={27} />
        </span>
        <div>
          <h2>A stable address. Ready backends.</h2>
          <p>
            Services route traffic to matching, ready pods in the same
            namespace.
          </p>
        </div>
        <div className="network-flow">
          <span>Cluster IP</span>
          <ArrowRight size={17} />
          <span>Service</span>
          <ArrowRight size={17} />
          <span>Ready pods</span>
        </div>
      </div>
      <section className="panel">
        <div className="panel-heading">
          <h2>
            Services <span className="count-chip">{services.length}</span>
          </h2>
          <SearchBox
            query={query}
            setQuery={setQuery}
            placeholder="Search services…"
          />
        </div>
        <ResourceTable
          resources={filtered}
          snapshot={snapshot}
          onInspect={onInspect}
        />
        <div className="table-footer">
          Showing {filtered.length} of {services.length} services
          <span>
            Select a service to inspect ports, selectors, and matching pods.
          </span>
        </div>
      </section>
    </>
  );
}
function Connection({
  mode,
  connected,
  loading,
  snapshot,
  switchMode,
  navigate,
}: {
  mode: Mode;
  connected: boolean;
  loading: boolean;
  snapshot: Snapshot | null;
  switchMode: (m: Mode) => void;
  navigate: (page: Page) => void;
}) {
  return (
    <div className="connection-layout">
      <section className="panel connection-options">
        <div className="panel-heading">
          <div>
            <h2>Data source</h2>
            <p>Choose a sample cluster or connect your own.</p>
          </div>
        </div>
        <div
          className={`connection-option ${mode === "demo" ? "selected" : ""}`}
        >
          <span className="connection-option-icon">
            <FlaskConical size={24} />
          </span>
          <div>
            <h3>
              Demo environment{" "}
              {mode === "demo" && <Badge tone="blue">Selected</Badge>}
            </h3>
            <p>
              A consistent sample cluster with deployments, nodes, services, and
              examples of resources that need attention.
            </p>
            <button
              className="button"
              onClick={() => {
                if (mode !== "demo") switchMode("demo");
                navigate("overview");
              }}
            >
              {mode === "demo" ? "Explore demo" : "Switch to demo"}
              <ArrowRight size={14} />
            </button>
          </div>
        </div>
        <div
          className={`connection-option ${mode === "live" ? "selected" : ""}`}
        >
          <span className="connection-option-icon">
            <Cable size={24} />
          </span>
          <div>
            <h3>
              Live RustKube API{" "}
              {mode === "live" && (
                <Badge tone={connected ? "good" : "warning"}>
                  {connected
                    ? "Connected"
                    : loading
                      ? "Connecting"
                      : "Disconnected"}
                </Badge>
              )}
            </h3>
            <p>
              Read your cluster through the local console server. Configure the
              API address and credentials below, then connect.
            </p>
            <button
              className="button primary"
              disabled={loading}
              onClick={() => {
                if (connected) navigate("overview");
                else switchMode("live");
              }}
            >
              {loading ? (
                <RefreshCw className="spin" size={15} />
              ) : (
                <Cable size={15} />
              )}
              {connected
                ? "View live cluster"
                : loading
                  ? "Connecting…"
                  : "Connect to API"}
            </button>
            {connected && snapshot && (
              <small className="connected-info">
                <Check size={13} />
                rk-api v{snapshot.version} · refreshes every 10 seconds
              </small>
            )}
          </div>
        </div>
      </section>
      <section className="panel connection-about">
        <span className="large-resource-icon">
          <ShieldCheck size={24} />
        </span>
        <h2>Observe with confidence</h2>
        <p>
          This console reads resource state. Deployments, scaling, and node
          operations stay in your existing CLI.
        </p>
        <ul>
          <li>
            <Check size={15} />
            No cluster mutations
          </li>
          <li>
            <Check size={15} />
            Credentials stay on the server
          </li>
          <li>
            <Check size={15} />
            TLS certificates are verified
          </li>
        </ul>
        <p className="small-note">
          The development proxy binds to your local machine. Public hosting
          needs a separately authenticated, same-origin proxy.
        </p>
      </section>
      <section className="panel setup-panel">
        <div className="panel-heading">
          <div>
            <h2>
              <Settings2 size={18} />
              Local connection setup
            </h2>
            <p>
              Configure the console server, then restart it to apply changes.
            </p>
          </div>
          <span className="label-chip">frontend/.env.local</span>
        </div>
        <div className="setup-body">
          <ol className="setup-steps">
            <li>
              <strong>Set the API address</strong>
              <p>
                The default development API listens at http://127.0.0.1:6443.
                Use HTTPS for a TLS-enabled cluster.
              </p>
            </li>
            <li>
              <strong>Add credentials for a secured cluster</strong>
              <p>
                Point to your token and CA files on the machine running the
                console.
              </p>
            </li>
            <li>
              <strong>Restart and connect</strong>
              <p>
                Run <code>npm run dev</code> from <code>frontend/</code>, then
                select “Connect to API”.
              </p>
            </li>
          </ol>
          <div className="config-example">
            <div>
              <span className="status-dot blue" />
              Secured cluster example
            </div>
            <pre>
              {
                "RK_API_URL=https://127.0.0.1:6443\nRK_TOKEN_FILE=/path/to/pki/token\nRK_CA_FILE=/path/to/pki/ca.crt"
              }
            </pre>
            <p>For an open development API, set only RK_API_URL.</p>
          </div>
        </div>
      </section>
    </div>
  );
}
