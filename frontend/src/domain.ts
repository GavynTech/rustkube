import type {
  Deployment,
  NodeResource,
  Pod,
  Resource,
  Service,
  Snapshot,
} from "./types";

export const namespaceOf = (resource: Resource) =>
  resource.metadata.namespace ?? "default";
export const resourceKey = (resource: Resource) =>
  `${resource.kind}/${namespaceOf(resource)}/${resource.metadata.name}`;
export const inNamespace = <T extends Resource>(
  resources: T[],
  namespace: string,
) =>
  resources.filter((r) => namespace === "all" || namespaceOf(r) === namespace);
export const searchResources = <T extends Resource>(
  resources: T[],
  query: string,
) => {
  const needle = query.trim().toLowerCase();
  return resources.filter((r) =>
    [
      r.metadata.name,
      r.metadata.namespace,
      ...Object.entries(r.metadata.labels ?? {}).flat(),
      ...imagesOf(r),
    ]
      .join(" ")
      .toLowerCase()
      .includes(needle),
  );
};
export const imagesOf = (resource: Resource) => {
  const containers =
    resource.kind === "Pod"
      ? resource.spec?.containers
      : resource.kind === "Deployment" || resource.kind === "ReplicaSet"
        ? resource.spec?.template?.spec?.containers
        : [];
  return (containers ?? []).map((c) => c.image);
};
export const readyPod = (pod: Pod) =>
  pod.status?.phase === "Running" &&
  !!pod.status.containerStatuses?.length &&
  pod.status.containerStatuses.every((c) => c.ready);
export const terminalPod = (pod: Pod) =>
  pod.status?.phase === "Succeeded" || pod.status?.phase === "Failed";
export const podRestarts = (pod: Pod) =>
  (pod.status?.containerStatuses ?? []).reduce(
    (sum, c) => sum + (c.restartCount ?? 0),
    0,
  );
export const readyCount = (pod: Pod) =>
  `${(pod.status?.containerStatuses ?? []).filter((c) => c.ready).length}/${pod.spec?.containers?.length ?? 0}`;
export type Tone = "good" | "warning" | "bad" | "neutral" | "blue";
export function statusOf(resource: Resource): { label: string; tone: Tone } {
  if (resource.metadata.deletionTimestamp)
    return { label: "Terminating", tone: "warning" };
  if (resource.kind === "Node") {
    if (resource.status?.condition !== "Ready")
      return {
        label:
          resource.status?.condition === "NotReady" ? "Not ready" : "Unknown",
        tone: "bad",
      };
    return resource.spec?.unschedulable
      ? { label: "Cordoned", tone: "warning" }
      : { label: "Ready", tone: "good" };
  }
  if (resource.kind === "Deployment" || resource.kind === "ReplicaSet") {
    const desired = resource.spec?.replicas ?? 1;
    if (!desired) return { label: "Scaled to zero", tone: "neutral" };
    return (resource.status?.readyReplicas ?? 0) >= desired
      ? { label: "Available", tone: "good" }
      : { label: "Progressing", tone: "warning" };
  }
  if (resource.kind === "Pod") {
    const phase = resource.status?.phase ?? "Pending";
    if (phase === "Running")
      return readyPod(resource)
        ? { label: "Running", tone: "good" }
        : { label: "Not ready", tone: "warning" };
    return {
      label: phase,
      tone:
        phase === "Failed"
          ? "bad"
          : phase === "Pending"
            ? "warning"
            : "neutral",
    };
  }
  return {
    label:
      resource.kind === "Service"
        ? (resource.spec?.type ?? "ClusterIP")
        : (resource.status?.phase ?? "Active"),
    tone: "blue",
  };
}

export function ownedBy(child: Resource, parent: Resource) {
  // RustKube controllers identify ownership by controller UID, never by a reused name.
  return (
    !!parent.metadata.uid &&
    namespaceOf(child) === namespaceOf(parent) &&
    (child.metadata.ownerReferences ?? []).some(
      (owner) =>
        owner.controller &&
        owner.kind === parent.kind &&
        owner.uid === parent.metadata.uid,
    )
  );
}
export function deploymentTree(deployment: Deployment, snapshot: Snapshot) {
  const replicaSets = snapshot.replicasets.filter((rs) =>
    ownedBy(rs, deployment),
  );
  const pods = snapshot.pods.filter((pod) =>
    replicaSets.some((rs) => ownedBy(pod, rs)),
  );
  const nodes = snapshot.nodes.filter((node) =>
    pods.some((pod) => pod.spec?.nodeName === node.metadata.name),
  );
  return { replicaSets, pods, nodes };
}
export function servicePods(service: Service, pods: Pod[]) {
  const entries = Object.entries(service.spec?.selector ?? {});
  return entries.length
    ? pods.filter(
        (pod) =>
          namespaceOf(pod) === namespaceOf(service) &&
          entries.every(([key, value]) => pod.metadata.labels?.[key] === value),
      )
    : [];
}
export const serviceEndpoints = (service: Service, pods: Pod[]) =>
  servicePods(service, pods).filter(
    (pod) => readyPod(pod) && pod.status?.podIP,
  );

export function cpuCores(value?: string | number): number | null {
  if (value == null) return null;
  const text = String(value).trim();
  if (!/^(?:\d+(?:\.\d{1,3})?|\.\d{1,3}|\d+m)$/.test(text)) return null;
  const result = text.endsWith("m")
    ? Number(text.slice(0, -1)) / 1000
    : Number(text);
  return Number.isFinite(result) ? result : null;
}
export function memoryBytes(value?: string | number): number | null {
  if (value == null) return null;
  const match = /^(\d+)(Ki|Mi|Gi|Ti|k|K|M|G|T)?$/.exec(String(value).trim());
  if (!match) return null;
  const units: Record<string, number> = {
    Ki: 1024,
    Mi: 1024 ** 2,
    Gi: 1024 ** 3,
    Ti: 1024 ** 4,
    k: 1000,
    K: 1000,
    M: 1e6,
    G: 1e9,
    T: 1e12,
  };
  const result = Number(match[1]) * (units[match[2]] ?? 1);
  return Number.isFinite(result) ? result : null;
}
export function reservations(nodes: NodeResource[], pods: Pod[]) {
  const nodeNames = new Set(nodes.map((n) => n.metadata.name));
  const scheduled = pods.filter(
    (p) =>
      p.spec?.nodeName && nodeNames.has(p.spec.nodeName) && !terminalPod(p),
  );
  const containers = scheduled.flatMap((p) => p.spec?.containers ?? []);
  const cpuCapacities = nodes.map((n) => cpuCores(n.status?.capacity?.cpu));
  const memCapacities = nodes.map((n) =>
    memoryBytes(n.status?.capacity?.memory),
  );
  return {
    cpu: containers.reduce(
      (sum, c) => sum + (cpuCores(c.resources?.requests?.cpu) ?? 0),
      0,
    ),
    memory: containers.reduce(
      (sum, c) => sum + (memoryBytes(c.resources?.requests?.memory) ?? 0),
      0,
    ),
    cpuCapacity:
      nodes.length && cpuCapacities.every((v) => v !== null)
        ? cpuCapacities.reduce<number>((sum, v) => sum + v!, 0)
        : null,
    memoryCapacity:
      nodes.length && memCapacities.every((v) => v !== null)
        ? memCapacities.reduce<number>((sum, v) => sum + v!, 0)
        : null,
  };
}
export const compactNumber = (n: number) =>
  new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(n);
export function formatMemory(bytes: number) {
  if (bytes >= 1024 ** 3) return `${compactNumber(bytes / 1024 ** 3)} GiB`;
  if (bytes >= 1024 ** 2) return `${compactNumber(bytes / 1024 ** 2)} MiB`;
  return `${compactNumber(bytes)} B`;
}
export function age(timestamp?: string, now = Date.now()) {
  if (!timestamp || !Number.isFinite(Date.parse(timestamp))) return "—";
  const seconds = Math.max(0, Math.floor((now - Date.parse(timestamp)) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}
