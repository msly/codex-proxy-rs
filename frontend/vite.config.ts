import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/stats": "http://127.0.0.1:18080",
      "/admin": "http://127.0.0.1:18080",
      "/health": "http://127.0.0.1:18080"
    }
  }
});
