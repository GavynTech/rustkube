import { useCallback, useEffect, useRef, useState } from "react";
import { fetchSnapshot } from "./client";
import { createDemoSnapshot } from "./demo";
import type { Mode, Snapshot } from "./types";

export function useCluster() {
  const [mode, setMode] = useState<Mode>("demo");
  const demo = useRef(createDemoSnapshot());
  const [snapshot, setSnapshot] = useState<Snapshot | null>(demo.current);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const switchMode = useCallback((next: Mode) => {
    setMode(next);
    setSnapshot(next === "demo" ? demo.current : null);
    setError(null);
    setLoading(next === "live");
    setRevision((n) => n + 1);
  }, []);
  const refresh = useCallback(() => setRevision((n) => n + 1), []);

  useEffect(() => {
    if (mode === "demo") {
      setLoading(false);
      return;
    }
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    let controller: AbortController;
    const load = async () => {
      controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), 8000);
      setLoading(true);
      try {
        const next = await fetchSnapshot(controller.signal);
        if (!stopped) {
          setSnapshot(next);
          setError(null);
        }
      } catch (err) {
        if (!stopped)
          setError(
            controller.signal.aborted
              ? "The API took too long to respond. Check the connection and try again."
              : err instanceof Error
                ? err.message
                : "Unable to load cluster data.",
          );
      } finally {
        clearTimeout(timeout);
        if (!stopped) {
          setLoading(false);
          timer = setTimeout(load, 10000);
        }
      }
    };
    void load();
    return () => {
      stopped = true;
      clearTimeout(timer);
      controller?.abort();
    };
  }, [mode, revision]);

  return { mode, switchMode, snapshot, error, loading, refresh };
}
