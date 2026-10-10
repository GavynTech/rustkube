import fs from "node:fs";
import https from "node:https";
import { defineConfig, loadEnv, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

// This local console deliberately proxies only the read endpoints it uses.
const allowedPaths = new Set([
  "/healthz",
  "/version",
  ...[
    "deployments",
    "replicasets",
    "pods",
    "nodes",
    "services",
    "namespaces",
  ].map((kind) => `/api/v1/${kind}`),
]);
const readOnlyProxy: Plugin = {
  name: "rustkube-read-only-proxy",
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      const path = (req.url ?? "").split("?")[0];
      if (!path.startsWith("/cluster-api")) return next();
      if (
        req.method !== "GET" ||
        !allowedPaths.has(path.slice("/cluster-api".length))
      ) {
        res.writeHead(403, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            error: "This console permits resource-list reads only.",
          }),
        );
        return;
      }
      next();
    });
  },
};

export default defineConfig(({ mode }) => {
  const env = { ...loadEnv(mode, process.cwd(), "RK_"), ...process.env };
  const target = env.RK_API_URL || "http://127.0.0.1:6443";
  const url = new URL(target);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== "/"
  )
    throw new Error(
      "RK_API_URL must be an HTTP(S) origin without credentials or a path.",
    );
  const token = env.RK_TOKEN_FILE
    ? fs.readFileSync(env.RK_TOKEN_FILE, "utf8").trim()
    : undefined;
  const ca = env.RK_CA_FILE ? fs.readFileSync(env.RK_CA_FILE) : undefined;
  return {
    plugins: [react(), readOnlyProxy],
    server: {
      host: "127.0.0.1",
      port: 5173,
      strictPort: true,
      proxy: {
        "/cluster-api": {
          target,
          changeOrigin: true,
          secure: true,
          ...(url.protocol === "https:"
            ? { agent: new https.Agent({ ca, rejectUnauthorized: true }) }
            : {}),
          rewrite: (path) => path.replace(/^\/cluster-api/, ""),
          configure(proxy) {
            proxy.on("proxyReq", (proxyReq) => {
              proxyReq.removeHeader("cookie");
              proxyReq.removeHeader("authorization");
              if (token) proxyReq.setHeader("Authorization", `Bearer ${token}`);
            });
          },
        },
      },
    },
    // Static preview intentionally has no privileged API proxy.
    preview: { host: "127.0.0.1", port: 4173, proxy: {} },
  };
});
