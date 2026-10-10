import { describe, expect, it } from "vitest";
import { createDemoSnapshot } from "./demo";
import {
  cpuCores,
  deploymentTree,
  inNamespace,
  memoryBytes,
  ownedBy,
  readyPod,
  reservations,
  searchResources,
  serviceEndpoints,
  servicePods,
  statusOf,
} from "./domain";

describe("RustKube resource relationships", () => {
  it("traverses controller UIDs and ignores reused names and other namespaces", () => {
    const snapshot = createDemoSnapshot();
    const deployment = snapshot.deployments[0];
    const tree = deploymentTree(deployment, snapshot);
    expect(tree.replicaSets).toHaveLength(1);
    expect(tree.pods).toHaveLength(4);
    expect(tree.nodes).toHaveLength(2);
    expect(
      ownedBy(
        {
          ...tree.replicaSets[0],
          metadata: { ...tree.replicaSets[0].metadata, namespace: "other" },
        },
        deployment,
      ),
    ).toBe(false);
    expect(
      ownedBy(tree.replicaSets[0], {
        ...deployment,
        metadata: { ...deployment.metadata, uid: "replacement" },
      }),
    ).toBe(false);
    expect(
      ownedBy(tree.replicaSets[0], {
        ...deployment,
        metadata: { name: deployment.metadata.name },
      }),
    ).toBe(false);
  });
  it("isolates service selectors by namespace and readiness", () => {
    const { services, pods } = createDemoSnapshot();
    const service = services[0];
    const matched = servicePods(service, pods);
    expect(matched).toHaveLength(4);
    const crossNamespace = {
      ...matched[0],
      metadata: { ...matched[0].metadata, namespace: "other" },
    };
    const unready = {
      ...matched[0],
      status: {
        ...matched[0].status,
        containerStatuses: [{ name: "web", ready: false }],
      },
    };
    const noIp = {
      ...matched[0],
      status: { ...matched[0].status, podIP: undefined },
    };
    expect(
      serviceEndpoints(service, [crossNamespace, unready, noIp]),
    ).toHaveLength(0);
    expect(
      servicePods(
        { ...service, spec: { ...service.spec, selector: {} } },
        pods,
      ),
    ).toHaveLength(0);
  });
  it("filters names, images, labels and namespace without case sensitivity", () => {
    const { deployments } = createDemoSnapshot();
    expect(inNamespace(deployments, "staging")).toHaveLength(1);
    expect(searchResources(deployments, " NGINX ")).toHaveLength(1);
    expect(searchResources(deployments, "production")).toHaveLength(3);
    expect(searchResources(deployments, "nonexistent")).toHaveLength(0);
  });
});
describe("capacity and status", () => {
  it("parses the backend CPU and memory quantity forms", () => {
    expect(cpuCores("250m")).toBe(0.25);
    expect(cpuCores("1.125")).toBe(1.125);
    expect(cpuCores(2)).toBe(2);
    expect(cpuCores("bad")).toBeNull();
    expect(cpuCores("-1")).toBeNull();
    expect(memoryBytes("2Gi")).toBe(2 * 1024 ** 3);
    expect(memoryBytes("512Mi")).toBe(512 * 1024 ** 2);
    expect(memoryBytes("2G")).toBe(2e9);
    expect(memoryBytes(1024)).toBe(1024);
    expect(memoryBytes("wat")).toBeNull();
    expect(memoryBytes()).toBeNull();
  });
  it("counts only scheduled, non-terminal pods and reports unknown capacity", () => {
    const { nodes, pods } = createDemoSnapshot();
    const result = reservations(nodes, pods);
    expect(result.cpu).toBe(3.5);
    expect(result.cpuCapacity).toBe(12);
    expect(result.memory).toBe(4.5 * 1024 ** 3);
    expect(result.memoryCapacity).toBe(24 * 1024 ** 3);
    const terminal = { ...pods[0], status: { phase: "Succeeded" as const } };
    expect(reservations(nodes, [terminal, pods.at(-1)!]).cpu).toBe(0);
    expect(
      reservations([{ ...nodes[0], status: { condition: "Ready" } }], [])
        .cpuCapacity,
    ).toBeNull();
    expect(reservations([], []).memoryCapacity).toBeNull();
  });
  it("handles missing status, scaled-zero workloads, failures and cordoning", () => {
    const { nodes, deployments, pods } = createDemoSnapshot();
    expect(statusOf({ ...pods[0], status: undefined }).label).toBe("Pending");
    expect(statusOf({ ...pods[0], status: { phase: "Failed" } }).tone).toBe(
      "bad",
    );
    expect(readyPod({ ...pods[0], status: { phase: "Running" } })).toBe(false);
    expect(statusOf({ ...deployments[0], spec: { replicas: 0 } }).label).toBe(
      "Scaled to zero",
    );
    expect(statusOf({ ...nodes[0], spec: { unschedulable: true } }).label).toBe(
      "Cordoned",
    );
  });
});
