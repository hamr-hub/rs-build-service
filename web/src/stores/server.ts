/** 服务连接状态：健康检查、运行时切换服务地址、主题。 */

import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { apiBase, getMetrics, hasApiOverride, ping, setApiBase, type MetricsSnapshot } from '../api/client'

export type ServerState = 'unknown' | 'online' | 'offline'

const THEME_KEY = 'hotpot.theme'

export const useServerStore = defineStore('server', () => {
  const state = ref<ServerState>('unknown')
  const lastCheckAt = ref<number | null>(null)
  const metrics = ref<MetricsSnapshot | null>(null)
  const metricsError = ref<string | null>(null)
  const base = ref(apiBase())
  const overridden = ref(hasApiOverride())
  const theme = ref<'dark' | 'light'>(
    (localStorage.getItem(THEME_KEY) as 'dark' | 'light' | null) ?? 'dark',
  )

  const online = computed(() => state.value === 'online')

  function applyTheme(): void {
    document.documentElement.dataset.theme = theme.value
    localStorage.setItem(THEME_KEY, theme.value)
  }

  function toggleTheme(): void {
    theme.value = theme.value === 'dark' ? 'light' : 'dark'
    applyTheme()
  }

  /** 手动指定服务地址；传 `null` 表示回到 `/api` 代理默认值。 */
  function updateBase(next: string | null): void {
    setApiBase(next)
    base.value = apiBase()
    overridden.value = hasApiOverride()
    state.value = 'unknown'
    void check()
  }

  /** 健康检查。轻量、频繁，适合轮询。 */
  async function check(): Promise<boolean> {
    const ok = await ping()
    state.value = ok ? 'online' : 'offline'
    lastCheckAt.value = Date.now()
    if (!ok) metrics.value = null
    return ok
  }

  /** 拉取 `/metrics`；失败不影响连接状态，只是仪表盘留空。 */
  async function refreshMetrics(): Promise<void> {
    if (state.value === 'offline') return
    try {
      metrics.value = await getMetrics()
      metricsError.value = null
    } catch (error) {
      metrics.value = null
      metricsError.value = error instanceof Error ? error.message : String(error)
    }
  }

  function init(): void {
    applyTheme()
    void check()
  }

  return {
    state,
    lastCheckAt,
    metrics,
    metricsError,
    base,
    overridden,
    theme,
    online,
    init,
    check,
    refreshMetrics,
    updateBase,
    toggleTheme,
  }
})
