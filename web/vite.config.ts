import { fileURLToPath, URL } from 'node:url'
import vue from '@vitejs/plugin-vue'
import { defineConfig, loadEnv } from 'vite'

/**
 * 开发代理：把 `/api/*` 转发到真实的 hotpot-server。
 *
 * 上游服务端没有 CORS 中间件，浏览器直接跨源调会被拦。用同源代理绕开，
 * 前端代码里也就不用到处判断"开发用代理、生产用直连"。
 *
 * 目标地址用环境变量覆盖：`HOTPOT_SERVER=http://10.0.0.5:7878 npm run dev`
 */
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '')
  const target = env.HOTPOT_SERVER || 'http://127.0.0.1:7878'

  return {
    plugins: [vue()],
    resolve: {
      alias: {
        '@': fileURLToPath(new URL('./src', import.meta.url)),
      },
    },
    server: {
      port: 5173,
      proxy: {
        '/api': {
          target,
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/api/, ''),
          // SSE 必须关掉缓冲，否则日志会攒着一次性吐出来。
          configure: (proxy) => {
            proxy.on('proxyRes', (proxyRes) => {
              if (proxyRes.headers['content-type']?.includes('text/event-stream')) {
                proxyRes.headers['cache-control'] = 'no-cache, no-transform'
                proxyRes.headers['x-accel-buffering'] = 'no'
              }
            })
          },
        },
      },
    },
    build: {
      target: 'es2022',
      sourcemap: true,
    },
  }
})
