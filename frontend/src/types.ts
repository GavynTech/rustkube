// Wire names follow crates/rk-types. Optional fields also accommodate minimal manifests.
export interface Metadata {
  name: string;
  namespace?: string;
  uid?: string;
  resourceVersion?: number;
  creationTimestamp?: string;
  deletionTimestamp?: string;
  labels?: Record<string, string>;
  annotations?: Record<string, string>;
  ownerReferences?: {
    kind: string;
    name: string;
    uid?: string;
    controller?: boolean;
  }[];
}
export interface Quantities {
  cpu?: string | number;
  memory?: string | number;
}
export interface Container {
  name: string;
  image: string;
  resources?: { requests?: Quantities; limits?: Quantities };
}
export interface PodSpec {
  containers?: Container[];
  nodeName?: string;
  nodeSelector?: Record<string, string>;
  restartPolicy?: string;
}
export interface WorkloadSpec {
  replicas?: number;
  selector?: { matchLabels?: Record<string, string> };
  template?: { metadata?: { labels?: Record<string, string> }; spec?: PodSpec };
}
interface ObjectResource<K, S, T> {
  apiVersion?: string;
  kind: K;
  metadata: Metadata;
  spec?: S;
  status?: T;
}
export type Pod = ObjectResource<
  "Pod",
  PodSpec,
  {
    phase?: "Pending" | "Running" | "Succeeded" | "Failed" | "Unknown";
    podIP?: string;
    message?: string;
    containerStatuses?: {
      name: string;
      ready?: boolean;
      restartCount?: number;
      state?: unknown;
    }[];
  }
>;
export type Deployment = ObjectResource<
  "Deployment",
  WorkloadSpec,
  { replicas?: number; readyReplicas?: number; updatedReplicas?: number }
>;
export type ReplicaSet = ObjectResource<
  "ReplicaSet",
  WorkloadSpec,
  { replicas?: number; readyReplicas?: number }
>;
export type NodeResource = ObjectResource<
  "Node",
  { podCidr?: string; unschedulable?: boolean },
  {
    condition?: "Ready" | "NotReady" | "Unknown";
    capacity?: Quantities;
    allocatable?: Quantities;
    address?: string;
    lastHeartbeat?: string;
  }
>;
export type Service = ObjectResource<
  "Service",
  {
    type?: "ClusterIP" | "NodePort";
    clusterIP?: string;
    selector?: Record<string, string>;
    ports?: {
      name?: string;
      port: number;
      targetPort?: number;
      nodePort?: number;
      protocol?: string;
    }[];
  },
  Record<string, unknown>
>;
export type Namespace = ObjectResource<
  "Namespace",
  Record<string, unknown>,
  { phase?: string }
>;
export type Resource =
  Pod | Deployment | ReplicaSet | NodeResource | Service | Namespace;
export interface Snapshot {
  deployments: Deployment[];
  replicasets: ReplicaSet[];
  pods: Pod[];
  nodes: NodeResource[];
  services: Service[];
  namespaces: Namespace[];
  version: string;
  fetchedAt: number;
}
export type Mode = "demo" | "live";
export type Page =
  "overview" | "workloads" | "nodes" | "networking" | "connection";
