<script setup lang="ts">
/** 总览：一眼看清"现在有多少构建在跑、缓存有没有在干活、磁盘占了多少"。 */
import { computed, onMounted } from 'vue'
import { RouterLink } from 'vue-router'
import EmptyState from '../components/EmptyState.vue'
import AppIcon from '../components/AppIcon.vue'
import StatCard from '../components/StatCard.vue'
import StatusBadge from '../components/StatusBadge.vue'
import { useBuildsStore } from '../stores/builds'
import { useServerStore } from '../stores/server'
import { useVisibilityPolling } from '../composables/useVisibilityPolling'
import type { BuildStatus } from '../api/types'
import {
  formatBytes,
  formatDuration,
  formatExecutor,
  formatPercent,
  formatRelative,
  shortId,
  shortenPath,
} from '../utils/format'

const builds = useBuildsStore()
const server = useServerStore()

const metrics = computed(() => server.metrics)

/**
 * 状态分布的展示顺序与配色：先看在途的（排队/执行中），再看终态。
 * 服务端 `/metrics` 会下发全部出现的状态，这里按表驱动，缺哪个就少画哪个。
 */
const DISTRIBUTION: { key: BuildStatus; label: string; color: string }[] = [
  { key: 'queued', label: '排队', color: 'var(--neutral)' },
  { key: 'dispatched', label: '执行中', color: 'var(--active)' },
  { key: 'running', label: '运行中', color: 'var(--active)' },
  { key: 'succeeded', label: '成功', color: 'var(--ok)' },
  { key: 'failed', label: '失败', color: 'var(--bad)' },
  { key: 'timeout', label: '超时', color: 'var(--warn)' },
  { key: 'canceled', label: '取消', color: 'var(--text-3)' },
]

const counts = computed(() => {
  const source = metrics.value?.buildsByStatus ?? {}
  const segments = DISTRIBUTION.map((d) => ({ ...d, value: source[d.key] ?? 0 }))
  // 兜底：服务端若新增了状态，这里不至于把它算丢。
  const accounted = segments.reduce((sum, d) => sum + d.value, 0)
  const extra = Object.entries(source)
    .filter(([key]) => !DISTRIBUTION.some((d) => d.key === key))
    .reduce((sum, [, value]) => sum + value, 0)
  return { segments, known: accounted + extra }
})

const totalBuilds = computed(() => {
  const known = counts.value.known
  return known > 0 ? known : builds.filtered.length
})

const successRate = computed(() => {
  const succeeded = counts.value.segments.find((s) => s.key === 'succeeded')?.value ?? 0
  const failed = counts.value.segments.find((s) => s.key === 'failed')?.value ?? 0
  const finished = succeeded + failed
  return finished === 0 ? null : succeeded / finished
})

const activeNow = computed(
  () =>
    (counts.value.segments.find((s) => s.key === 'dispatched')?.value ?? 0) +
    (counts.value.segments.find((s) => s.key === 'queued')?.value ?? 0),
)

/** 两个协议（sccache / turbo）里取命中率更高的那个作为主指标。 */
const bestCache = computed(() => {
  const ratio = metrics.value?.cacheHitRatio ?? {}
  const entries = Object.entries(ratio)
  if (entries.length === 0) return null
  return entries.reduce((best, entry) => (entry[1] > best[1] ? entry : best))
})

const cacheProtocols = computed(() => {
  const m = metrics.value
  if (!m) return []
  const names = new Set([
    ...Object.keys(m.cacheHitRatio),
    ...Object.keys(m.cacheLookups),
  ])
  return [...names].map((protocol) => ({
    protocol,
    hitRatio: m.cacheHitRatio[protocol] ?? 0,
    lookups: m.cacheLookups[protocol] ?? 0,
    hits: m.cacheHits[protocol] ?? 0,
    misses: m.cacheMisses[protocol] ?? 0,
    puts: m.cachePuts[protocol] ?? 0,
    entries: m.cacheIndexEntries[protocol] ?? 0,
    indexBytes: m.cacheIndexBytes[protocol] ?? 0,
  }))
})

const recent = computed(() => builds.filtered.slice(0, 7))

onMounted(() => {
  void server.refreshMetrics()
})

// 指标 10s 一次；页面不可见时自动停摆（见 composable 的说明）。
useVisibilityPolling(() => server.refreshMetrics(), 10_000)

function sourceLabel(path: string): string {
  return shortenPath(path)
}
</script>

<template>
  <div class="stack">
    <!-- 核心指标 -->
    <section class="stat-grid">
      <StatCard
        label="构建总数"
        :value="String(totalBuilds)"
        :meta="activeNow > 0 ? `${activeNow} 个进行中` : '队列空闲'"
        :tone="activeNow > 0 ? 'active' : 'neutral'"
      />
      <StatCard
        label="成功率"
        :value="successRate === null ? '—' : formatPercent(successRate, 0)"
        meta="成功 / (成功 + 失败)"
        :tone="successRate === null ? 'neutral' : successRate >= 0.8 ? 'ok' : successRate >= 0.5 ? 'warn' : 'bad'"
      />
      <StatCard
        label="队列深度"
        :value="metrics?.queueDepth == null ? '—' : String(Math.round(metrics.queueDepth))"
        :meta="
          metrics?.queueOldestWaitMs == null
            ? '等待队列长度'
            : `最久等待 ${formatDuration(metrics.queueOldestWaitMs)}`
        "
        :tone="(metrics?.queueDepth ?? 0) > 0 ? 'warn' : 'ok'"
      />
      <StatCard
        label="缓存命中率"
        :value="bestCache ? formatPercent(bestCache[1], 1) : '—'"
        :meta="bestCache ? `${bestCache[0]} 协议` : '未挂载缓存协议'"
        :tone="bestCache && bestCache[1] >= 0.5 ? 'ok' : bestCache ? 'warn' : 'neutral'"
      />
    </section>

    <!-- 状态分布 -->
    <section class="card">
      <div class="card__head">
        <div>
          <h2 class="card__title">状态分布</h2>
          <p class="card__hint">来自 <code class="code">/metrics</code> 的实时计数</p>
        </div>
        <RouterLink class="btn btn--sm" :to="{ name: 'builds' }">
          查看全部
          <AppIcon name="arrowRight" :size="14" />
        </RouterLink>
      </div>
      <div class="card__body">
        <template v-if="counts.known > 0">
          <div class="dist-bar">
            <div
              v-for="seg in counts.segments.filter((s) => s.value > 0)"
              :key="seg.key"
              class="dist-bar__seg"
              :style="{ width: `${(seg.value / counts.known) * 100}%`, background: seg.color }"
              :title="`${seg.label} ${seg.value}`"
            />
          </div>
          <div class="dist-legend">
            <div v-for="seg in counts.segments" :key="seg.key" class="dist-legend__item">
              <span class="dist-legend__swatch" :style="{ background: seg.color }" />
              {{ seg.label }}
              <span class="dist-legend__value">{{ seg.value }}</span>
            </div>
          </div>
        </template>
        <EmptyState
          v-else
          icon="stack"
          title="还没有构建记录"
          description="服务端刚启动或队列是空的。提交一次构建，这里就会活起来。"
        >
          <RouterLink class="btn btn--primary" :to="{ name: 'build-new' }">
            <AppIcon name="plus" />
            提交第一个构建
          </RouterLink>
        </EmptyState>
      </div>
    </section>

    <div class="grid-2">
      <!-- 最近构建 -->
      <section class="card">
        <div class="card__head">
          <h2 class="card__title">最近构建</h2>
          <RouterLink class="btn btn--sm" :to="{ name: 'builds' }">全部</RouterLink>
        </div>
        <div class="card__body card__body--flush">
          <ul v-if="recent.length" class="recent">
            <li v-for="build in recent" :key="build.id">
              <RouterLink class="recent__row" :to="{ name: 'build-detail', params: { id: build.id } }">
                <div class="recent__main">
                  <span class="mono truncate">{{ shortId(build.id, 14) }}</span>
                  <span class="dim truncate">
                    {{ build.source.kind === 'local' ? sourceLabel(build.source.path) : build.source.kind }}
                  </span>
                </div>
                <div class="recent__meta">
                  <span class="dim">{{ formatRelative(build.created_at_ms) }}</span>
                  <span class="dim mono">{{ formatDuration(build.timings.total_ms || null) }}</span>
                  <StatusBadge :status="build.status" />
                </div>
              </RouterLink>
            </li>
          </ul>
          <EmptyState v-else icon="clock" title="暂无构建" description="提交一次构建后会显示在这里。" />
        </div>
      </section>

      <div class="stack">
        <!-- 缓存协议 -->
        <section class="card">
          <div class="card__head">
            <h2 class="card__title">远程缓存</h2>
            <RouterLink class="btn btn--sm" :to="{ name: 'toolchains' }">环境</RouterLink>
          </div>
          <div class="card__body stack stack--tight">
            <div v-for="row in cacheProtocols" :key="row.protocol" class="cache-row">
              <div class="row row--between">
                <strong>{{ row.protocol }}</strong>
                <span class="mono">{{ formatPercent(row.hitRatio, 1) }}</span>
              </div>
              <div class="meter">
                <div
                  class="meter__fill"
                  :style="{
                    width: `${Math.min(100, row.hitRatio * 100)}%`,
                    background: row.hitRatio >= 0.5 ? 'var(--ok)' : 'var(--warn)',
                  }"
                />
              </div>
              <p class="dim">
                命中 {{ row.hits }} · 未命中 {{ row.misses }} · 写入 {{ row.puts }} · 索引
                {{ formatBytes(row.indexBytes) }}（{{ row.entries }} 条）
              </p>
            </div>
            <p v-if="!cacheProtocols.length" class="dim">
              服务端未挂载缓存协议端点（<code class="code">/sccache</code>、<code class="code">/v8/artifacts</code>）。
            </p>
          </div>
        </section>

        <!-- 运行时 -->
        <section class="card">
          <div class="card__head">
            <h2 class="card__title">运行时</h2>
          </div>
          <div class="card__body">
            <div class="kv">
              <span class="kv__k">执行器</span>
              <span class="kv__v mono">{{ formatExecutor(metrics?.executor) }}</span>
              <span class="kv__k">Worker</span>
              <span class="kv__v mono">{{ metrics?.workers ?? '—' }}</span>
              <span class="kv__k">默认工具链</span>
              <span class="kv__v mono">{{ metrics?.toolchain ?? '—' }}</span>
              <span class="kv__k">版本</span>
              <span class="kv__v mono">{{ metrics?.version ?? '—' }}</span>
              <span class="kv__k">CAS 占用</span>
              <span class="kv__v mono">{{ formatBytes(metrics?.storeBytes) }}</span>
              <span class="kv__k">平均构建</span>
              <span class="kv__v mono">{{ formatDuration(metrics?.buildDurationAvgMs) }}</span>
            </div>
          </div>
        </section>
      </div>
    </div>
  </div>
</template>

<style scoped>
.grid-2 {
  display: grid;
  grid-template-columns: minmax(0, 1.35fr) minmax(0, 1fr);
  gap: var(--sp-5);
  align-items: start;
}

@media (max-width: 1100px) {
  .grid-2 {
    grid-template-columns: minmax(0, 1fr);
  }
}

.recent__row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-4);
  padding: var(--sp-3) var(--sp-5);
  border-bottom: 1px solid var(--border);
  transition: background var(--speed) var(--ease);
}

.recent__row:last-child {
  border-bottom: 0;
}

.recent__row:hover {
  background: var(--surface-2);
}

.recent__main {
  display: flex;
  flex-direction: column;
  min-width: 0;
  gap: 2px;
  font-size: var(--text-sm);
}

.recent__meta {
  display: flex;
  align-items: center;
  gap: var(--sp-4);
  flex: none;
  font-size: var(--text-sm);
}

.cache-row {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding-bottom: var(--sp-3);
  border-bottom: 1px dashed var(--border);
}

.cache-row:last-of-type {
  border-bottom: 0;
  padding-bottom: 0;
}

.cache-row strong {
  font-size: var(--text-base);
}
</style>
