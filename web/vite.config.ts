import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  build: {
    assetsDir: "assets",
    manifest: true,
    sourcemap: false,
    target: "es2022",
  },
  server: {
    proxy: {
      "/v1": "http://127.0.0.1:7777",
    },
  },
});
