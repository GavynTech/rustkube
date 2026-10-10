// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  renderHook,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import "@testing-library/jest-dom/vitest";
import App from "./App";
import { createDemoSnapshot } from "./demo";
import { useCluster } from "./useCluster";
import { resourceKinds } from "./client";

function mockCluster(empty = false) {
  const snapshot = createDemoSnapshot();
  return vi.fn(async (url: string | URL | Request) => {
    const path = String(url);
    if (path.endsWith("/healthz")) return new Response("ok");
    if (path.endsWith("/version"))
      return Response.json({ component: "rk-api", version: "0.1.0" });
    const key = path.split("/").pop() as keyof typeof resourceKinds;
    return Response.json({
      items: empty ? [] : snapshot[key],
      resourceVersion: 1,
    });
  });
}
beforeEach(() => {
  history.replaceState(null, "", "/");
  // jsdom has no native modal implementation. Real focus trapping is browser QA.
  HTMLDialogElement.prototype.showModal = function () {
    this.setAttribute("open", "");
  };
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("console interaction", () => {
  it("opens with an explicit demo label, scoped counts, and a real ownership pipeline", async () => {
    const user = userEvent.setup();
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    render(<App />);
    expect(
      screen.getByRole("heading", { name: "Cluster overview" }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("You’re exploring a demo cluster."),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Running pods/ }),
    ).toHaveTextContent("9/ 10");
    expect(fetcher).not.toHaveBeenCalled();
    await user.selectOptions(screen.getByLabelText("Namespace"), "staging");
    expect(
      screen.getByRole("button", { name: /Running pods/ }),
    ).toHaveTextContent("0/ 1");
    expect(
      screen.getByRole("combobox", { name: "Deployment to visualize" }),
    ).toHaveValue("Deployment/staging/image-processor");
    expect(screen.getByText("Unscheduled")).toBeInTheDocument();
  });
  it("navigates, searches by image, and filters pod status", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("link", { name: /Workloads/ }));
    await user.type(
      screen.getByRole("searchbox", { name: "Search workloads…" }),
      "nginx",
    );
    const table = screen.getByRole("table");
    expect(within(table).getAllByRole("row")).toHaveLength(2);
    expect(
      within(table).getByRole("button", { name: "web-frontend" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Clear search" }));
    await user.click(screen.getByRole("button", { name: /^Pods/ }));
    await user.selectOptions(
      screen.getByRole("combobox", { name: "Filter by workload status" }),
      "Pending",
    );
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(
      2,
    );
    expect(
      within(screen.getByRole("table")).getByText("Unscheduled"),
    ).toBeInTheDocument();
  });
  it("opens resource details and JSON, then closes the drawer", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(
      screen.getByRole("button", { name: "Inspect Deployment web-frontend" }),
    );
    const dialog = screen.getByRole("dialog", { name: "web-frontend" });
    expect(within(dialog).getByText("Desired replicas")).toBeInTheDocument();
    expect(within(dialog).getByText("nginx:1.27")).toBeInTheDocument();
    await user.click(
      within(dialog).getByRole("button", { name: "Manifest JSON" }),
    );
    expect(
      within(dialog).getByText(/"kind": "Deployment"/),
    ).toBeInTheDocument();
    await user.click(
      within(dialog).getByRole("button", { name: "Close resource details" }),
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  it("supports node and service inspection plus an empty search state", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("link", { name: /^Nodes/ }));
    expect(screen.getByText("Scheduling disabled")).toBeInTheDocument();
    expect(screen.getByLabelText("Namespace")).toBeDisabled();
    await user.type(
      screen.getByRole("searchbox", { name: "Search nodes…" }),
      "not-a-node",
    );
    expect(screen.getByText("No nodes found")).toBeInTheDocument();
    await user.click(screen.getByRole("link", { name: /^Networking/ }));
    await user.click(
      screen.getByRole("button", { name: "Inspect Service web-frontend" }),
    );
    const dialog = screen.getByRole("dialog", { name: "web-frontend" });
    expect(within(dialog).getByText("10.96.0.10")).toBeInTheDocument();
    expect(
      within(dialog).getByRole("heading", { name: /Matching pods/ }),
    ).toHaveTextContent("4");
  });
  it("shows live authentication failure without falling back to demo", async () => {
    const user = userEvent.setup();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("", { status: 401 })),
    );
    render(<App />);
    await user.click(screen.getByRole("button", { name: "Connect cluster" }));
    await user.click(screen.getByRole("button", { name: "Connect to API" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Access denied");
    expect(
      screen.queryByText("You’re exploring a demo cluster."),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("link", { name: "Overview" }));
    expect(
      screen.getByText("Your cluster is not connected"),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Explore demo" }));
    expect(
      screen.getByText("You’re exploring a demo cluster."),
    ).toBeInTheDocument();
  });
  it("renders an empty live cluster and retains stale data after refresh errors", async () => {
    const user = userEvent.setup();
    const fetcher = mockCluster(true);
    vi.stubGlobal("fetch", fetcher);
    render(<App />);
    await user.click(screen.getByRole("button", { name: "Connect cluster" }));
    await user.click(screen.getByRole("button", { name: "Connect to API" }));
    await user.click(
      await screen.findByRole("button", { name: "View live cluster" }),
    );
    expect(
      screen.getByText("No deployments in this namespace"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Running pods/ }),
    ).toHaveTextContent("0/ 0");
    fetcher.mockImplementation(async () => new Response("", { status: 502 }));
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("data is stale");
    expect(
      screen.getByRole("button", { name: /Running pods/ }),
    ).toHaveTextContent("0/ 0");
    expect(
      screen.queryByText("You’re exploring a demo cluster."),
    ).not.toBeInTheDocument();
  });
});

describe("live refresh lifecycle", () => {
  it("polls, keeps stale snapshots, and stops polling on return to demo", async () => {
    vi.useFakeTimers();
    const fetcher = mockCluster();
    vi.stubGlobal("fetch", fetcher);
    const { result } = renderHook(() => useCluster());
    await act(async () => {
      result.current.switchMode("live");
    });
    expect(result.current.snapshot?.pods).toHaveLength(10);
    expect(fetcher).toHaveBeenCalledTimes(8);
    fetcher.mockImplementation(async () => new Response("", { status: 503 }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10000);
    });
    expect(result.current.error).toContain("HTTP 503");
    expect(result.current.snapshot?.pods).toHaveLength(10);
    await act(async () => {
      result.current.switchMode("demo");
    });
    expect(result.current.error).toBeNull();
    const calls = fetcher.mock.calls.length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30000);
    });
    expect(fetcher).toHaveBeenCalledTimes(calls);
  });
  it("ignores late responses after leaving live mode", async () => {
    let resolveResponse!: (value: Response) => void;
    const response = new Promise<Response>((resolve) => {
      resolveResponse = resolve;
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(() => response),
    );
    const { result } = renderHook(() => useCluster());
    act(() => result.current.switchMode("live"));
    act(() => result.current.switchMode("demo"));
    await act(async () => {
      resolveResponse(new Response("", { status: 401 }));
    });
    expect(result.current.mode).toBe("demo");
    expect(result.current.error).toBeNull();
    expect(result.current.snapshot?.pods).toHaveLength(10);
  });
});
