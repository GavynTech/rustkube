import type {
  Deployment,
  Metadata,
  NodeResource,
  Pod,
  ReplicaSet,
  Service,
  Snapshot,
} from "./types";

export function createDemoSnapshot(now = Date.now()): Snapshot {
  const meta = (name: string, namespace?: string, hours = 48): Metadata => ({
    name,
    namespace,
    uid: `demo-${namespace ?? "cluster"}-${name}`,
    resourceVersion: 1,
    creationTimestamp: new Date(now - hours * 3600000).toISOString(),
  });
  const owner = (resource: Deployment | ReplicaSet) => [
    {
      kind: resource.kind,
      name: resource.metadata.name,
      uid: resource.metadata.uid,
      controller: true,
    },
  ];
  const nodes: NodeResource[] = [1, 2, 3].map((n) => ({
    apiVersion: "v1",
    kind: "Node",
    metadata: {
      ...meta(`rk-worker-0${n}`, undefined, 168),
      labels: {
        "kubernetes.io/arch": "amd64",
        "node-role": "worker",
        disk: "ssd",
      },
    },
    spec: { podCidr: `10.42.${n}.0/24`, unschedulable: n === 3 },
    status: {
      condition: n === 3 ? "NotReady" : "Ready",
      address: `192.168.64.${10 + n}`,
      capacity: { cpu: "4", memory: "8Gi" },
      allocatable: { cpu: "4", memory: "8Gi" },
      lastHeartbeat: new Date(now - (n === 3 ? 360000 : 3000)).toISOString(),
    },
  }));
  const configs = [
    {
      name: "web-frontend",
      namespace: "production",
      image: "nginx:1.27",
      replicas: 4,
      cpu: "250m",
      memory: "256Mi",
    },
    {
      name: "api-gateway",
      namespace: "production",
      image: "traefik:v3.3",
      replicas: 3,
      cpu: "500m",
      memory: "512Mi",
    },
    {
      name: "queue-worker",
      namespace: "production",
      image: "redis:7.4",
      replicas: 2,
      cpu: "500m",
      memory: "1Gi",
    },
    {
      name: "image-processor",
      namespace: "staging",
      image: "alpine:3.21",
      replicas: 1,
      cpu: "2",
      memory: "2Gi",
      pending: true,
    },
  ];
  const deployments: Deployment[] = [];
  const replicasets: ReplicaSet[] = [];
  const pods: Pod[] = [];
  const services: Service[] = [];
  configs.forEach((config, index) => {
    const labels = { app: config.name };
    const template = {
      metadata: { labels },
      spec: {
        containers: [
          {
            name: config.name,
            image: config.image,
            resources: { requests: { cpu: config.cpu, memory: config.memory } },
          },
        ],
        ...(config.pending ? { nodeSelector: { accelerator: "gpu" } } : {}),
      },
    };
    const deployment: Deployment = {
      apiVersion: "v1",
      kind: "Deployment",
      metadata: {
        ...meta(config.name, config.namespace, index === 3 ? 1 : 48),
        labels,
      },
      spec: {
        replicas: config.replicas,
        selector: { matchLabels: labels },
        template,
      },
      status: {
        replicas: config.replicas,
        readyReplicas: config.pending ? 0 : config.replicas,
        updatedReplicas: config.replicas,
      },
    };
    const rs: ReplicaSet = {
      apiVersion: "v1",
      kind: "ReplicaSet",
      metadata: {
        ...meta(`${config.name}-7d9f${index}`, config.namespace),
        labels,
        ownerReferences: owner(deployment),
      },
      spec: deployment.spec,
      status: deployment.status,
    };
    deployments.push(deployment);
    replicasets.push(rs);
    for (let i = 0; i < config.replicas; i++) {
      const node = ((i + index) % 2) + 1;
      pods.push({
        apiVersion: "v1",
        kind: "Pod",
        metadata: {
          ...meta(
            `${rs.metadata.name}-${["x4k2p", "m8r6t", "n3w9s", "b2j5v"][i]}`,
            config.namespace,
            config.pending ? 1 : 12 + i,
          ),
          labels,
          ownerReferences: owner(rs),
        },
        spec: {
          ...template.spec,
          ...(config.pending ? {} : { nodeName: `rk-worker-0${node}` }),
        },
        status: config.pending
          ? {
              phase: "Pending",
              message:
                "0/3 nodes available: 2 nodeSelector mismatch, 1 node not ready. Requires accelerator=gpu.",
            }
          : {
              phase: "Running",
              podIP: `10.42.${node}.${20 + index * 10 + i}`,
              containerStatuses: [
                {
                  name: config.name,
                  ready: true,
                  restartCount: index === 2 && i === 0 ? 1 : 0,
                  state: {
                    running: {
                      startedAt: new Date(now - 12 * 3600000).toISOString(),
                    },
                  },
                },
              ],
            },
      });
    }
    if (!config.pending)
      services.push({
        apiVersion: "v1",
        kind: "Service",
        metadata: meta(config.name, config.namespace),
        spec: {
          type: "ClusterIP",
          clusterIP: `10.96.0.${10 + index}`,
          selector: labels,
          ports: [
            {
              port: index === 2 ? 6379 : 80,
              targetPort: index === 2 ? 6379 : 80,
              protocol: "TCP",
            },
          ],
        },
      });
  });
  return {
    deployments,
    replicasets,
    pods,
    nodes,
    services,
    namespaces: ["default", "production", "staging"].map((name) => ({
      apiVersion: "v1",
      kind: "Namespace",
      metadata: meta(name),
      status: { phase: "Active" },
    })),
    version: "0.1.0",
    fetchedAt: now,
  };
}
