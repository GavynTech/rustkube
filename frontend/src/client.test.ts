import { describe, expect, it, vi } from "vitest";
import { fetchSnapshot, parseResourceList, resourceKinds } from "./client";
import { createDemoSnapshot } from "./demo";

export function fixtureFetch(empty = false) {
  const snapshot = createDemoSnapshot();
  return vi.fn(async (url: string | URL | Request, _options?: RequestInit) => {
    const path = String(url);
    if (path.endsWith("/healthz")) return new Response("ok");
    if (path.endsWith("/version"))
      return Response.json({ component: "rk-api", version: "0.1.0" });
    const key = path.split("/").pop() as keyof typeof resourceKinds;
    return Response.json({
      items: empty ? [] : snapshot[key],
      resourceVersion: 12,
    });
  });
}
describe("live API adapter", () => {
  it("reads existing endpoints without exposing credentials or requesting secrets", async () => {
    const fetcher = fixtureFetch();
    const result = await fetchSnapshot(new AbortController().signal, fetcher);
    expect(result.nodes).toHaveLength(3);
    expect(result.pods).toHaveLength(10);
    expect(result.version).toBe("0.1.0");
    expect(fetcher).toHaveBeenCalledTimes(8);
    for (const [url, options] of fetcher.mock.calls) {
      expect(url).toMatch(/^\/cluster-api\//);
      expect(options?.method).toBeUndefined();
      expect(options?.headers).not.toHaveProperty("Authorization");
      expect(url).not.toMatch(/secrets|configmaps/);
    }
  });
  it("accepts a healthy empty cluster", async () => {
    const result = await fetchSnapshot(
      new AbortController().signal,
      fixtureFetch(true),
    );
    expect(result.pods).toEqual([]);
    expect(result.nodes).toEqual([]);
  });
  it("distinguishes authentication failure, proxy failure, and bad responses", async () => {
    await expect(
      fetchSnapshot(
        new AbortController().signal,
        vi.fn(async () => new Response("", { status: 401 })),
      ),
    ).rejects.toThrow("Access denied");
    await expect(
      fetchSnapshot(
        new AbortController().signal,
        vi.fn(async () => {
          throw new TypeError("offline");
        }),
      ),
    ).rejects.toThrow("Cannot reach");
    await expect(
      fetchSnapshot(
        new AbortController().signal,
        vi.fn(async () => new Response("not json")),
      ),
    ).rejects.toThrow("invalid response");
    await expect(
      fetchSnapshot(
        new AbortController().signal,
        vi.fn(async () => new Response("", { status: 502 })),
      ),
    ).rejects.toThrow("HTTP 502");
  });
  it("rejects malformed lists and resources", () => {
    expect(() => parseResourceList({ items: null }, "Pod")).toThrow(
      "Invalid Pod list",
    );
    expect(() =>
      parseResourceList(
        { items: [{ kind: "Node", metadata: { name: "x" } }] },
        "Pod",
      ),
    ).toThrow("Invalid Pod resource");
    expect(() =>
      parseResourceList(
        { items: [{ kind: "Pod", metadata: { name: "x" }, spec: "wrong" }] },
        "Pod",
      ),
    ).toThrow("Invalid Pod spec");
  });
});
