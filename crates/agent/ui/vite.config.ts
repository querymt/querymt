import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: 'dist',
    cssMinify: false,
  },
  server: {
    port: 5173,
    proxy: {
      // Proxy WebSocket connections to backend
      '/ui/ws': {
        target: 'ws://127.0.0.1:3030',
        ws: true,
      },
    },
  },
})
