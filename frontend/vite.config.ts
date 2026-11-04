// Конфигурация Vite для клиента чата.
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

// Во время разработки запросы к API и WebSocket проксируются на Rust-сервер.
const backend = process.env.CHAT_DEV_BACKEND ?? 'http://127.0.0.1:3000'

// https://vite.dev/config/
export default defineConfig({
  plugins: [vue()],
  server: {
    port: 5173,
    proxy: {
      // WebSocket-соединение чата.
      '/ws': {
        target: backend,
        ws: true,
      },
      // Служебный HTTP API (например, /api/health).
      '/api': {
        target: backend,
      },
    },
  },
})
