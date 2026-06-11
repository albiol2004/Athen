import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Built output is embedded into the athen binary (rust-embed over
// web/dist, served by http_api.rs). `npm run build` then `cargo build`.
// For UI development against a running instance:
//   npm run dev  →  http://localhost:5173, point the login screen's
//   "Server" field at e.g. http://127.0.0.1:8787 (instance CORS is
//   permissive; auth is the token, not the origin).
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
    assetsDir: 'assets',
  },
});
