import type { Resource, Snapshot } from "./types";

export const resourceKinds = {
  deployments: "Deployment",
  replicasets: "ReplicaSet",
  pods: "Pod",
  nodes: "Node",
  services: "Service",
  namespaces: "Namespace",
} as const;
export class ApiError extends Error {
  constructor(
    message: string,
    public status?: number,
  ) {
    super(message);
    this.name = "ApiError";
  }
}
export function parseResourceList(
  value: unknown,
  kind: Resource["kind"],
): Resource[] {
  if (
    !value ||
    typeof value !== "object" ||
    !("items" in value) ||
    !Array.isArray(value.items)
  )
    throw new ApiError(`Invalid ${kind} list returned by the API.`);
  for (const item of value.items) {
    if (
      !item ||
      typeof item !== "object" ||
      item.kind !== kind ||
      !item.metadata ||
      typeof item.metadata.name !== "string" ||
      !item.metadata.name
    )
      throw new ApiError(`Invalid ${kind} resource returned by the API.`);
    for (const key of ["spec", "status"]) {
      if (
        item[key] != null &&
        (typeof item[key] !== "object" || Array.isArray(item[key]))
      )
        throw new ApiError(`Invalid ${kind} ${key} returned by the API.`);
    }
  }
  return value.items as Resource[];
}

export async function fetchSnapshot(
  signal: AbortSignal,
  fetcher: typeof fetch = fetch,
): Promise<Snapshot> {
  // The proxy owns upstream credentials. Never put tokens in browser storage or URLs.
  const request = async (path: string, json = true) => {
    let response: Response;
    try {
      response = await fetcher(`/cluster-api${path}`, {
        signal,
        headers: { Accept: json ? "application/json" : "text/plain" },
        cache: "no-store",
      });
    } catch (error) {
      if (signal.aborted) throw error;
      throw new ApiError(
        "Cannot reach the API proxy. Check the connection configuration and that rk-api is running.",
      );
    }
    if (!response.ok) {
      if (response.status === 401 || response.status === 403)
        throw new ApiError(
          "Access denied. Check RK_TOKEN_FILE on the dashboard server.",
          response.status,
        );
      throw new ApiError(
        `The API request ${path} failed (HTTP ${response.status}). Check the API address and trusted CA.`,
        response.status,
      );
    }
    try {
      return json ? await response.json() : await response.text();
    } catch {
      throw new ApiError(`The API returned an invalid response for ${path}.`);
    }
  };
  const keys = Object.keys(resourceKinds) as (keyof typeof resourceKinds)[];
  const [health, version, ...lists] = await Promise.all([
    request("/healthz", false),
    request("/version"),
    ...keys.map((key) => request(`/api/v1/${key}`)),
  ]);
  if (
    health.trim() !== "ok" ||
    version?.component !== "rk-api" ||
    typeof version?.version !== "string"
  )
    throw new ApiError(
      "This endpoint did not identify itself as a healthy RustKube API.",
    );
  const resources = Object.fromEntries(
    keys.map((key, i) => [
      key,
      parseResourceList(lists[i], resourceKinds[key]),
    ]),
  ) as Pick<Snapshot, keyof typeof resourceKinds>;
  return { ...resources, version: version.version, fetchedAt: Date.now() };
}
