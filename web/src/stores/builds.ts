/**
 * 构建列表状态：拉取、筛选、搜索、分页、取消。
 *
 * 一次拉全量（服务端 limit 上限 200），筛选/搜索/分页全在客户端做：
 * 切状态、翻页、打字都是零延迟，键盘输入不会被网络往返卡住。构建量涨到
 * 几百条之后会改成服务端分页，但那时也应该保留本地筛选的即时性。
 */

import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { ApiError, cancelBuild, listBuilds } from '../api/client'
import type { BuildRecord, BuildStatus } from '../api/types'
import { isTerminal } from '../api/types'
import { useToastStore } from './toast'

export const PAGE_SIZE = 25

/** 一次拉取的最大条数（服务端硬上限 200）。 */
const FETCH_LIMIT = 200

export type StatusFilter = BuildStatus | 'all'

export const useBuildsStore = defineStore('builds', () => {
  const toast = useToastStore()

  const builds = ref<BuildRecord[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  const status = ref<StatusFilter>('all')
  const query = ref('')
  const offset = ref(0)
  /** 正在取消的构建 id 集合——集合而非单值，因为可以同时取消多行。 */
  const cancelling = ref<Set<string>>(new Set())

  let inflight: AbortController | null = null

  const hasActive = computed(() => builds.value.some((b) => !isTerminal(b.status)))

  const statusTally = computed(() => {
    const out: Record<string, number> = {}
    for (const build of builds.value) out[build.status] = (out[build.status] ?? 0) + 1
    return out
  })

  /** 搜索命中：ID、项目路径/URL、档位标签都参与匹配。 */
  const filtered = computed(() => {
    const byStatus = status.value === 'all'
      ? builds.value
      : builds.value.filter((b) => b.status === status.value)

    const needle = query.value.trim().toLowerCase()
    if (!needle) return byStatus

    return byStatus.filter((b) => {
      if (b.id.toLowerCase().includes(needle)) return true
      const source =
        b.source.kind === 'local'
          ? b.source.path
          : b.source.kind === 'git'
            ? b.source.url
            : b.source.upload_id
      if (source.toLowerCase().includes(needle)) return true
      if (b.profile.mode.includes(needle)) return true
      if ((b.profile.toolchain ?? '').toLowerCase().includes(needle)) return true
      return b.profile.features.some((f) => f.toLowerCase().includes(needle))
    })
  })

  const pageCount = computed(() => Math.max(1, Math.ceil(filtered.value.length / PAGE_SIZE)))
  const page = computed(() => Math.floor(offset.value / PAGE_SIZE) + 1)
  const visible = computed(() => filtered.value.slice(offset.value, offset.value + PAGE_SIZE))

  async function load(): Promise<void> {
    inflight?.abort()
    inflight = new AbortController()
    loading.value = true
    try {
      builds.value = await listBuilds(
        { status: null, limit: FETCH_LIMIT, offset: 0 },
        inflight.signal,
      )
      error.value = null
    } catch (cause) {
      if (cause instanceof DOMException && cause.name === 'AbortError') return
      error.value =
        cause instanceof ApiError
          ? cause.message
          : cause instanceof Error
            ? cause.message
            : String(cause)
      builds.value = []
    } finally {
      loading.value = false
    }
  }

  function setStatus(next: StatusFilter): void {
    status.value = next
    offset.value = 0
  }

  function setQuery(next: string): void {
    query.value = next
    offset.value = 0
  }

  function setPage(next: number): void {
    offset.value = (Math.min(Math.max(1, next), pageCount.value) - 1) * PAGE_SIZE
  }

  function findById(id: string): BuildRecord | undefined {
    return builds.value.find((b) => b.id === id)
  }

  function patch(record: BuildRecord): void {
    const index = builds.value.findIndex((b) => b.id === record.id)
    if (index === -1) return
    const next = builds.value.slice()
    next[index] = record
    builds.value = next
  }

  /**
   * 取消构建：乐观更新 + 失败回滚。
   *
   * 取消要经过一次 HTTP 往返（服务端要先查状态、判终态、再决定是直接标记
   * 还是给执行器发信号）。乐观地把状态置为 `canceled`，让列表立刻反映用户
   * 的操作；请求失败时回滚到原记录并提示——否则列表会撒谎说"已取消"，
   * 而构建其实还在跑。
   */
  async function cancel(id: string): Promise<boolean> {
    if (cancelling.value.has(id)) return false
    cancelling.value = new Set(cancelling.value).add(id)

    const index = builds.value.findIndex((b) => b.id === id)
    const previous = index === -1 ? null : builds.value[index]!
    if (previous) {
      patch({ ...previous, status: 'canceled' })
    }

    try {
      const updated = await cancelBuild(id)
      patch(updated)
      toast.success('已取消', updated.id.slice(0, 8))
      return true
    } catch (cause) {
      // 回滚：把用户看到的"已取消"撤回。
      if (previous) patch(previous)
      toast.error(
        '取消失败',
        cause instanceof Error ? cause.message : String(cause),
      )
      return false
    } finally {
      const next = new Set(cancelling.value)
      next.delete(id)
      cancelling.value = next
    }
  }

  return {
    builds,
    loading,
    error,
    status,
    query,
    offset,
    hasActive,
    statusTally,
    filtered,
    visible,
    page,
    pageCount,
    cancelling,
    load,
    setStatus,
    setQuery,
    setPage,
    findById,
    patch,
    cancel,
  }
})
