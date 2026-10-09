import path from "node:path";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const ENV_CONFIG_MARKER =
  '<script id="env-config" type="application/json">{}</script>';

/** In dev, fills the env-config tag the server fills in production. */
function devEnvConfig(): Plugin {
  const config = {
    KEYCLOAK_URL: process.env.RBE_KEYCLOAK_URL,
    KEYCLOAK_REALM: process.env.RBE_KEYCLOAK_REALM,
    KEYCLOAK_CLIENT_ID: process.env.RBE_KEYCLOAK_CLIENT_ID,
  };
  return {
    name: "ferris-dev-env-config",
    apply: "serve",
    transformIndexHtml: (html) =>
      html.replace(
        ENV_CONFIG_MARKER,
        `<script id="env-config" type="application/json">${JSON.stringify(config)}</script>`,
      ),
  };
}

export default defineConfig({
  plugins: [react(), tailwindcss(), devEnvConfig()],
  base: "/",
  resolve: {
    alias: {
      cn: path.resolve(import.meta.dirname, "src/lib/cn.ts"),
      "@": path.resolve(import.meta.dirname, "src"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: process.env.RBE_API_URL ?? "http://localhost:8088",
        changeOrigin: true,
        ws: true,
      },
    },
  },
  build: {
    outDir: "dist",
  },
});
