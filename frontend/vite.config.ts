import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

const backend = process.env.CHAT_DEV_BACKEND ?? 'http://127.0.0.1:3000'

// https://vite.dev/config/
export default defineConfig({
  plugins: [vue()],
  server: {
    port: 5173,
    proxy: {
      '/ws': {
        target: backend,
        ws: true,
      },
      '/api': {
        target: backend,
      },
    },
  },
})
