/** 构建列表状态：筛选、分页、轮询刷新。 */

import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { ApiError, listBuilds } from '../api/client'
import type { BuildRecord, BuildStatus } from '../api/types'
import { isTerminal } from '../api/types'

export const PAGE_SIZE = 20

export const useBuildsStore = defineStore('builds', () => {
  const builds = ref<BuildRecord[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  const status = ref<BuildStatus | 'all'>('all')
  const offset = ref(0)

  let inflight: AbortController | null = null

  /** 有构建还在跑时列表要自己会动，否则状态会永远停在"排队中"。 */
  const hasActive = computed(() => builds.value.some((b) => !isTerminal(b.status)))

  /** 状态筛选在客户端做：一次拉全量，切换零延迟。 */
  const filtered = computed(() =>
    status.value === 'all' ? builds.value : builds.value.filter((b) => b.status === status.value),
  )

  const pageCount = computed(() => Math.max(1, Math.ceil(filtered.value.length / PAGE_SIZE)))
  const page = computed(() => Math.floor(offset.value / PAGE_SIZE) + 1)
  const visible = computed(() => filtered.value.slice(offset.value, offset.value + PAGE_SIZE))

  async function load(): Promise<void> {
    inflight?.abort()
    inflight = new AbortController()
    loading.value = true
    try {
      builds.value = await listBuilds(
        { status: null, limit: 200, offset: 0 },
        inflight.signal,
      )
      error.value = null
      offset.value = 0
    } catch (cause) {
      if (cause instanceof DOMException && cause.name === 'AbortError') return
      error.value =
        cause instanceof ApiError ? cause.message : cause instanceof Error ? cause.message : String(cause)
      builds.value = []
    } finally {
      loading.value = false
    }
  }

  function setStatus(next: BuildStatus | 'all'): void {
    status.value = next
    offset.value = 0
  }

  function setPage(next: number): void {
    offset.value = (Math.min(Math.max(1, next), pageCount.value) - 1) * PAGE_SIZE
  }

  function findById(id: string): BuildRecord | undefined {
    return builds.value.find((b) => b.id === id)
  }

  /** 就地替换一条记录（取消后 / 详情页轮询回填）。 */
  function patch(record: BuildRecord): void {
    const index = builds.value.findIndex((b) => b.id === record.id)
    if (index === -1) return
    const next = builds.value.slice()
    next[index] = record
    builds.value = next
  }

  return {
    builds,
    loading,
    error,
    status,
    offset,
    hasActive,
    filtered,
    visible,
    page,
    pageCount,
    load,
    setStatus,
    setPage,
    findById,
    patch,
  }
})
