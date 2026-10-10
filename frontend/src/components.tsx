import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import {
  ArrowDown,
  ArrowUp,
  Box,
  Check,
  ChevronRight,
  Copy,
  FileJson,
  Layers,
  Network,
  Search,
  Server,
  X,
} from "lucide-react";
import {
  age,
  compactNumber,
  deploymentTree,
  formatMemory,
  imagesOf,
  namespaceOf,
  podRestarts,
  readyCount,
  readyPod,
  reservations,
  resourceKey,
  serviceEndpoints,
  servicePods,
  statusOf,
  type Tone,
} from "./domain";
import type { Deployment, Resource, Snapshot } from "./types";

export function Badge({
  children,
  tone = "neutral",
}: {
  children: ReactNode;
  tone?: Tone;
}) {
  return (
    <span className={`badge ${tone}`}>
      <span className="status-dot" />
      {children}
    </span>
  );
}
export function ResourceIcon({
  kind,
  size = 17,
}: {
  kind: Resource["kind"];
  size?: number;
}) {
  const Icon =
    kind === "Node"
      ? Server
      : kind === "Service"
        ? Network
        : kind === "Pod"
          ? Box
          : Layers;
  return <Icon size={size} aria-hidden="true" />;
}
export function EmptyState({
  title = "No resources found",
  children,
}: {
  title?: string;
  children?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <span className="empty-icon">
        <Search size={24} />
      </span>
      <h3>{title}</h3>
      <p>{children ?? "Try a different search or namespace."}</p>
    </div>
  );
}
export function CapacityBar({
  label,
  value,
  capacity,
  memory = false,
}: {
  label: string;
  value: number;
  capacity: number | null;
  memory?: boolean;
}) {
  const percent = capacity && capacity > 0 ? (value / capacity) * 100 : null;
  const format = memory
    ? formatMemory
    : (v: number) => `${compactNumber(v)} cores`;
  return (
    <div className="capacity-item">
      <div className="capacity-label">
        <span>{label}</span>
        <strong>
          {percent === null ? "Unknown capacity" : `${Math.round(percent)}%`}
        </strong>
      </div>
      <div
        className={`capacity-track ${percent !== null && percent > 85 ? "high" : ""}`}
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={
          percent === null ? undefined : Math.min(100, Math.round(percent))
        }
        aria-valuetext={`${format(value)} requested, ${capacity === null ? "capacity unknown" : `${format(capacity)} total`}`}
      >
        <span style={{ width: `${Math.min(100, percent ?? 0)}%` }} />
      </div>
      <div className="capacity-caption">
        <span>{format(value)} requested</span>
        <span>{capacity === null ? "—" : format(capacity)} total</span>
      </div>
    </div>
  );
}

export function ResourceTable({
  resources,
  snapshot,
  onInspect,
  compact = false,
}: {
  resources: Resource[];
  snapshot: Snapshot;
  onInspect: (resource: Resource) => void;
  compact?: boolean;
}) {
  const [descending, setDescending] = useState(false);
  if (!resources.length) return <EmptyState />;
  const sorted = [...resources].sort(
    (a, b) =>
      (descending ? -1 : 1) * a.metadata.name.localeCompare(b.metadata.name),
  );
  const kind = resources[0].kind;
  return (
    <div className="table-scroll">
      <table>
        <thead>
          <tr>
            <th>
              <button
                className="sort-button"
                onClick={() => setDescending((v) => !v)}
              >
                Name{" "}
                {descending ? <ArrowDown size={13} /> : <ArrowUp size={13} />}
              </button>
            </th>
            <th>Status</th>
            <th>{kind === "Node" ? "Address" : "Namespace"}</th>
            <th>
              {kind === "Pod"
                ? "Ready"
                : kind === "Service"
                  ? "Cluster IP"
                  : kind === "Node"
                    ? "Pods"
                    : "Replicas"}
            </th>
            {!compact && (
              <th>
                {kind === "Pod"
                  ? "Node"
                  : kind === "Service"
                    ? "Ports"
                    : kind === "Node"
                      ? "CPU capacity"
                      : "Image"}
              </th>
            )}
            <th>{kind === "Service" ? "Endpoints" : "Age"}</th>
            <th className="row-arrow">
              <span className="sr-only">Details</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((resource) => {
            const status = statusOf(resource);
            return (
              <tr key={resourceKey(resource)}>
                <td>
                  <button
                    className="resource-name"
                    onClick={() => onInspect(resource)}
                  >
                    <span className="resource-icon">
                      <ResourceIcon kind={resource.kind} />
                    </span>
                    <span>{resource.metadata.name}</span>
                  </button>
                </td>
                <td>
                  <Badge tone={status.tone}>{status.label}</Badge>
                </td>
                <td>
                  {resource.kind === "Node" ? (
                    <span className="mono">
                      {resource.status?.address ?? "—"}
                    </span>
                  ) : (
                    <span className="namespace-label">
                      {namespaceOf(resource)}
                    </span>
                  )}
                </td>
                <td className="mono">
                  {resource.kind === "Pod"
                    ? readyCount(resource)
                    : resource.kind === "Deployment" ||
                        resource.kind === "ReplicaSet"
                      ? `${resource.status?.readyReplicas ?? 0} / ${resource.spec?.replicas ?? 1}`
                      : resource.kind === "Service"
                        ? (resource.spec?.clusterIP ?? "Not allocated")
                        : resource.kind === "Node"
                          ? snapshot.pods.filter(
                              (p) =>
                                p.spec?.nodeName === resource.metadata.name,
                            ).length
                          : "—"}
                </td>
                {!compact && (
                  <td className="cell-secondary mono">
                    {resource.kind === "Pod"
                      ? (resource.spec?.nodeName ?? "Unscheduled")
                      : resource.kind === "Service"
                        ? (resource.spec?.ports ?? [])
                            .map((p) => `${p.port}/${p.protocol ?? "TCP"}`)
                            .join(", ") || "—"
                        : resource.kind === "Node"
                          ? (resource.status?.capacity?.cpu ?? "—")
                          : imagesOf(resource).join(", ") || "—"}
                  </td>
                )}
                <td className="cell-secondary">
                  {resource.kind === "Service" ? (
                    <span>
                      {serviceEndpoints(resource, snapshot.pods).length} ready
                    </span>
                  ) : (
                    age(resource.metadata.creationTimestamp)
                  )}
                </td>
                <td>
                  <button
                    className="icon-button small"
                    aria-label={`Inspect ${resource.kind} ${resource.metadata.name}`}
                    onClick={() => onInspect(resource)}
                  >
                    <ChevronRight size={16} />
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export function Pipeline({
  deployment,
  snapshot,
  onInspect,
}: {
  deployment: Deployment;
  snapshot: Snapshot;
  onInspect: (resource: Resource) => void;
}) {
  const tree = deploymentTree(deployment, snapshot);
  const card = (resource: Resource) => (
    <button
      key={resourceKey(resource)}
      className={`pipeline-card ${statusOf(resource).tone}`}
      onClick={() => onInspect(resource)}
    >
      <span className="pipeline-card-title">
        <ResourceIcon kind={resource.kind} />
        <strong>{resource.metadata.name}</strong>
      </span>
      <span className="pipeline-card-sub">
        {resource.kind === "Pod" ? (
          <>
            <span className={`status-dot ${statusOf(resource).tone}`} />
            {statusOf(resource).label}
            <span className="pipeline-node">
              <Server size={11} />
              {resource.spec?.nodeName ?? "Unscheduled"}
            </span>
          </>
        ) : (
          <>
            <span className={`status-dot ${statusOf(resource).tone}`} />
            {resource.kind === "Deployment" || resource.kind === "ReplicaSet"
              ? `${resource.status?.readyReplicas ?? 0} / ${resource.spec?.replicas ?? 1} ready`
              : statusOf(resource).label}
          </>
        )}
      </span>
    </button>
  );
  return (
    <div className="pipeline">
      <div className="pipeline-column">
        <div className="pipeline-column-label">
          01 <span>Deployment</span>
          <span className="count-chip">1</span>
        </div>
        <div className="pipeline-stage">{card(deployment)}</div>
      </div>
      <ChevronRight className="pipeline-arrow" size={19} />
      <div className="pipeline-column">
        <div className="pipeline-column-label">
          02 <span>ReplicaSets</span>
          <span className="count-chip">{tree.replicaSets.length}</span>
        </div>
        <div className="pipeline-stage">
          {tree.replicaSets.length ? (
            tree.replicaSets.map(card)
          ) : (
            <div className="pipeline-empty">Awaiting controller</div>
          )}
        </div>
      </div>
      <ChevronRight className="pipeline-arrow" size={19} />
      <div className="pipeline-column pods-column">
        <div className="pipeline-column-label">
          03 <span>Pods & placement</span>
          <span className="count-chip">{tree.pods.length}</span>
        </div>
        <div className="pipeline-stage">
          {tree.pods.length ? (
            tree.pods.map(card)
          ) : (
            <div className="pipeline-empty">No owned pods yet</div>
          )}
        </div>
      </div>
    </div>
  );
}

export function ResourceDrawer({
  resource,
  snapshot,
  onClose,
  onInspect,
}: {
  resource: Resource;
  snapshot: Snapshot;
  onClose: () => void;
  onInspect: (resource: Resource) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const heading = useId();
  const [tab, setTab] = useState<"details" | "json">("details");
  const [copyState, setCopyState] = useState("Copy JSON");
  const previousFocus = useRef<HTMLElement | null>(null);
  useEffect(() => {
    previousFocus.current = document.activeElement as HTMLElement;
    dialog.current?.showModal();
    return () => {
      previousFocus.current?.focus();
    };
  }, []);
  useEffect(() => {
    setTab("details");
    setCopyState("Copy JSON");
  }, [resourceKey(resource)]);
  const current = [
    ...snapshot.deployments,
    ...snapshot.replicasets,
    ...snapshot.pods,
    ...snapshot.nodes,
    ...snapshot.services,
    ...snapshot.namespaces,
  ].find((r) => resourceKey(r) === resourceKey(resource));
  const shown = current ?? resource;
  const status = statusOf(shown);
  const related: Resource[] =
    shown.kind === "Deployment"
      ? deploymentTree(shown, snapshot).pods
      : shown.kind === "ReplicaSet"
        ? snapshot.pods.filter(
            (p) =>
              p.metadata.ownerReferences?.some(
                (o) =>
                  o.controller &&
                  o.uid === shown.metadata.uid &&
                  o.kind === "ReplicaSet",
              ) && namespaceOf(p) === namespaceOf(shown),
          )
        : shown.kind === "Node"
          ? snapshot.pods.filter(
              (p) => p.spec?.nodeName === shown.metadata.name,
            )
          : shown.kind === "Service"
            ? servicePods(shown, snapshot.pods)
            : [];
  const fields: [string, ReactNode][] = [
    ["Kind", shown.kind],
    [
      "Namespace",
      shown.kind === "Node" || shown.kind === "Namespace"
        ? "Cluster scoped"
        : namespaceOf(shown),
    ],
    [
      "Created",
      shown.metadata.creationTimestamp
        ? new Date(shown.metadata.creationTimestamp).toLocaleString()
        : "—",
    ],
    ["Resource version", shown.metadata.resourceVersion ?? "—"],
  ];
  if (shown.kind === "Pod")
    fields.push(
      ["Node", shown.spec?.nodeName ?? "Unscheduled"],
      ["Pod IP", shown.status?.podIP ?? "Not assigned"],
      ["Ready containers", readyCount(shown)],
      ["Restarts", podRestarts(shown)],
    );
  if (shown.kind === "Deployment" || shown.kind === "ReplicaSet")
    fields.push(
      ["Desired replicas", shown.spec?.replicas ?? 1],
      ["Ready replicas", shown.status?.readyReplicas ?? 0],
    );
  if (shown.kind === "Node")
    fields.push(
      ["Address", shown.status?.address ?? "—"],
      ["Pod CIDR", shown.spec?.podCidr ?? "—"],
      ["Scheduling", shown.spec?.unschedulable ? "Disabled" : "Enabled"],
      [
        "Last heartbeat",
        shown.status?.lastHeartbeat
          ? new Date(shown.status.lastHeartbeat).toLocaleString()
          : "—",
      ],
    );
  if (shown.kind === "Service")
    fields.push(
      ["Cluster IP", shown.spec?.clusterIP ?? "Not allocated"],
      [
        "Ports",
        (shown.spec?.ports ?? [])
          .map(
            (p) =>
              `${p.port} → ${p.targetPort ?? p.port}/${p.protocol ?? "TCP"}`,
          )
          .join(", ") || "—",
      ],
      ["Ready endpoints", serviceEndpoints(shown, snapshot.pods).length],
    );
  const usage =
    shown.kind === "Node" ? reservations([shown], snapshot.pods) : null;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(JSON.stringify(shown, null, 2));
      setCopyState("Copied");
    } catch {
      setCopyState("Copy unavailable");
    }
  };
  return (
    <dialog
      className="resource-drawer"
      ref={dialog}
      aria-labelledby={heading}
      onCancel={onClose}
      onClick={(e) => {
        if (e.target === e.currentTarget) {
          const rect = e.currentTarget.getBoundingClientRect();
          if (
            e.clientX < rect.left ||
            e.clientX > rect.right ||
            e.clientY < rect.top ||
            e.clientY > rect.bottom
          )
            onClose();
        }
      }}
    >
      <div className="drawer-top">
        <span className="eyebrow">RESOURCE DETAILS</span>
        <button
          className="icon-button"
          aria-label="Close resource details"
          onClick={onClose}
        >
          <X size={20} />
        </button>
      </div>
      <div className="drawer-heading">
        <span className="large-resource-icon">
          <ResourceIcon kind={shown.kind} size={25} />
        </span>
        <div>
          <p>{shown.kind}</p>
          <h2 id={heading}>{shown.metadata.name}</h2>
        </div>
      </div>
      <div className="drawer-status">
        <Badge tone={status.tone}>{status.label}</Badge>
        <span className="muted">Read-only resource</span>
      </div>
      {!current && (
        <div className="notice warning">
          This resource is no longer in the latest snapshot. Showing its last
          known state.
        </div>
      )}
      <div className="tabs" aria-label="Resource detail view">
        <button
          aria-pressed={tab === "details"}
          className={tab === "details" ? "active" : ""}
          onClick={() => setTab("details")}
        >
          Overview
        </button>
        <button
          aria-pressed={tab === "json"}
          className={tab === "json" ? "active" : ""}
          onClick={() => setTab("json")}
        >
          <FileJson size={15} />
          Manifest JSON
        </button>
      </div>
      {tab === "json" ? (
        <div className="json-section">
          <button className="button" onClick={copy}>
            {copyState === "Copied" ? <Check size={15} /> : <Copy size={15} />}
            {copyState}
          </button>
          <pre tabIndex={0}>{JSON.stringify(shown, null, 2)}</pre>
          <span className="sr-only" role="status">
            {copyState}
          </span>
        </div>
      ) : (
        <div className="drawer-content">
          {shown.kind === "Pod" && shown.status?.message && (
            <div className="notice warning">{shown.status.message}</div>
          )}
          <dl className="detail-list">
            {fields.map(([label, value]) => (
              <div key={label}>
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
          {!!imagesOf(shown).length && (
            <section>
              <h3>Container images</h3>
              {imagesOf(shown).map((image, i) => (
                <div className="image-tag mono" key={`${i}-${image}`}>
                  <Box size={15} />
                  {image}
                </div>
              ))}
            </section>
          )}
          <section>
            <h3>Labels</h3>
            <div className="label-list">
              {Object.entries(shown.metadata.labels ?? {}).length ? (
                Object.entries(shown.metadata.labels!).map(([key, value]) => (
                  <span className="label-chip" key={key}>
                    {key}
                    <span>=</span>
                    {value}
                  </span>
                ))
              ) : (
                <p className="muted">No labels</p>
              )}
            </div>
          </section>
          {shown.kind === "Service" && (
            <section>
              <h3>Pod selector</h3>
              <div className="label-list">
                {Object.entries(shown.spec?.selector ?? {}).map(
                  ([key, value]) => (
                    <span className="label-chip" key={key}>
                      {key}={value}
                    </span>
                  ),
                )}
                {!Object.keys(shown.spec?.selector ?? {}).length && (
                  <p className="muted">No selector; no pod endpoints.</p>
                )}
              </div>
            </section>
          )}
          {usage && (
            <section>
              <h3>Resource reservations</h3>
              <CapacityBar
                label="CPU"
                value={usage.cpu}
                capacity={usage.cpuCapacity}
              />
              <CapacityBar
                label="Memory"
                value={usage.memory}
                capacity={usage.memoryCapacity}
                memory
              />
            </section>
          )}
          {shown.kind !== "Pod" && shown.kind !== "Namespace" && (
            <section>
              <h3>
                {shown.kind === "Service" ? "Matching pods" : "Pods"}{" "}
                <span className="count-chip">{related.length}</span>
              </h3>
              <div className="related-list">
                {related.length ? (
                  related.map((pod) => (
                    <button
                      key={resourceKey(pod)}
                      onClick={() => onInspect(pod)}
                    >
                      <Box size={15} />
                      <span>{pod.metadata.name}</span>
                      <Badge tone={statusOf(pod).tone}>
                        {statusOf(pod).label}
                      </Badge>
                      <ChevronRight size={15} />
                    </button>
                  ))
                ) : (
                  <p className="muted">
                    No related pods in the current snapshot.
                  </p>
                )}
              </div>
            </section>
          )}
        </div>
      )}
    </dialog>
  );
}
