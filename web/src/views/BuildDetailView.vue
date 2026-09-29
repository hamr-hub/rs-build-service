<script setup lang="ts">
/**
 * 构建详情：元信息 + 实时日志 + 产物。
 *
 * 日志走 SSE（`/v1/builds/{id}/logs/stream`），断线由客户端按指数退避重连，
 * 并把已消费到的最大 `seq` 作为 `since` 传回去补齐缺口。
 * 构建进入终态后停止 SSE 与轮询 —— 终态不会再变。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import LogViewer from '../components/LogViewer.vue'
import StatusBadge from '../components/StatusBadge.vue'
import { ApiError, artifactUrl, cancelBuild, getBuild, listArtifacts, streamLogs, type LogStreamHandle } from '../api/client'
import type { ArtifactMeta, BuildEvent, BuildRecord } from '../api/types'
import { isTerminal } from '../api/types'
import { useBuildsStore } from '../stores/builds'
import { formatBytes, formatDuration, formatTime, shortId } from '../utils/format'

const props = defineProps<{ id: string }>()

const builds = useBuildsStore()

const record = ref<BuildRecord | null>(null)
const events = ref<BuildEvent[]>([])
const artifacts = ref<ArtifactMeta[]>([])

const loading = ref(true)
const error = ref<string | null>(null)
const actionError = ref<string | null>(null)
const connected = ref(false)
const streamNotice = ref<string | null>(null)
const cancelling = ref(false)

let stream: LogStreamHandle | null = null
let recordPoller: number | undefined

const terminal = computed(() => (record.value ? isTerminal(record.value.status) : false))

/** 五段耗时：只有服务端填了的段才画，link_ms 目前恒为空。 */
const timingSegments = computed(() => {
  const t = record.value?.timings
  if (!t) return [] as { key: string; label: string; value: number; color: string }[]
  const raw: { key: string; label: string; value: number | null; color: string }[] = [
    { key: 'queue_ms', label: '排队', value: t.queue_ms, color: 'var(--neutral)' },
    { key: 'fetch_ms', label: '拉取', value: t.fetch_ms, color: 'var(--active)' },
    { key: 'build_ms', label: '编译', value: t.build_ms, color: 'var(--accent)' },
    { key: 'link_ms', label: '链接', value: t.link_ms, color: 'var(--text-3)' },
    { key: 'upload_ms', label: '上传', value: t.upload_ms, color: 'var(--ok)' },
  ]
  return raw.filter((seg): seg is { key: string; label: string; value: number; color: string } =>
    seg.value != null && seg.value > 0,
  )
})

const timingTotal = computed(() =>
  timingSegments.value.reduce((sum, seg) => sum + seg.value, 0),
)

const sourceLabel = computed(() => {
  const s = record.value?.source
  if (!s) return '—'
  if (s.kind === 'local') return s.path
  if (s.kind === 'git') return `${s.url}@${s.ref_name}${s.sha ? ` (${s.sha.slice(0, 8)})` : ''}`
  return s.upload_id
})

function stopStreaming(): void {
  stream?.close()
  stream = null
  connected.value = false
  if (recordPoller) {
    window.clearInterval(recordPoller)
    recordPoller = undefined
  }
}

function startStreaming(id: string): void {
  stopStreaming()
  // 重连时带上已消费到的最大 seq，服务端只补发更新的事件。
  const since = events.value.length > 0 ? events.value[events.value.length - 1]!.seq : 0
  streamNotice.value = null
  stream = streamLogs(id, since, {
    onOpen: () => {
      connected.value = true
      streamNotice.value = null
    },
    onEvent: (event) => {
      events.value = [...events.value, event]
    },
    onEnd: () => {
      stopStreaming()
      void refresh()
    },
    onError: (message, retrying) => {
      connected.value = false
      streamNotice.value = retrying ? `${message}，正在重连…` : message
      if (!retrying) stopStreaming()
    },
  })
}

async function refresh(): Promise<void> {
  try {
    const next = await getBuild(props.id)
    record.value = next
    builds.patch(next)
    error.value = null
  } catch (cause) {
    error.value = cause instanceof ApiError ? cause.message : String(cause)
  }
}

async function loadArtifacts(): Promise<void> {
  try {
    artifacts.value = await listArtifacts(props.id)
  } catch {
    artifacts.value = []
  }
}

/** 首次进入或切换构建：拉记录 → 开日志流 → 拉产物 → 必要时轮询记录。 */
async function load(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    record.value = await getBuild(props.id)
    builds.patch(record.value)
  } catch (cause) {
    error.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    loading.value = false
  }

  startStreaming(props.id)
  await loadArtifacts()

  // 耗时字段由 worker 陆续写入，非终态期间定期回填。
  if (record.value && !isTerminal(record.value.status)) {
    recordPoller = window.setInterval(() => void refresh(), 2000)
  }
}

async function onCancel(): Promise<void> {
  if (cancelling.value) return
  cancelling.value = true
  actionError.value = null
  try {
    const next = await cancelBuild(props.id)
    record.value = next
    builds.patch(next)
  } catch (cause) {
    actionError.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    cancelling.value = false
  }
}

function copyId(): void {
  void navigator.clipboard?.writeText(props.id)
}

// 在详情页之间跳转（同一组件复用）时，重置并重新加载。
watch(
  () => props.id,
  () => {
    stopStreaming()
    events.value = []
    artifacts.value = []
    record.value = null
    void load()
  },
)

onMounted(() => void load())

onUnmounted(stopStreaming)

function downloadAll(): void {
  for (const artifact of artifacts.value) {
    window.open(artifactUrl(artifact.digest, artifact.name), '_blank')
  }
}
</script>

<template>
  <div class="stack">
    <div class="row row--between">
      <RouterLink class="btn btn--ghost btn--sm" :to="{ name: 'builds' }">
        <AppIcon name="arrowLeft" :size="14" />
        返回列表
      </RouterLink>
      <div class="row">
        <button class="btn btn--sm" type="button" @click="copyId">
          <AppIcon name="copy" :size="14" />
          复制 ID
        </button>
        <button
          v-if="record && !terminal"
          class="btn btn--sm btn--danger"
          type="button"
          :disabled="cancelling"
          @click="onCancel"
        >
          {{ cancelling ? '取消中…' : '取消构建' }}
        </button>
      </div>
    </div>

    <div v-if="error" class="alert alert--bad">
      <AppIcon name="alert" />
      {{ error }}
    </div>
    <div v-if="actionError" class="alert alert--bad">
      <AppIcon name="alert" />
      {{ actionError }}
    </div>
    <div v-if="record?.error" class="alert alert--bad">
      <AppIcon name="alert" />
      <div>
        <strong>构建失败</strong>
        <p class="mono">{{ record.error }}</p>
      </div>
    </div>

    <div v-if="loading && !record" class="stack">
      <span class="skeleton" style="height: 78px" />
      <span class="skeleton" style="height: 420px" />
    </div>

    <template v-else-if="record">
      <!-- 概要 -->
      <section class="card">
        <div class="card__body">
          <div class="row row--between row--wrap">
            <div class="stack stack--tight">
              <div class="row">
                <h2 class="mono build-id">{{ record.id }}</h2>
                <StatusBadge :status="record.status" />
              </div>
              <p class="dim truncate" :title="sourceLabel">{{ sourceLabel }}</p>
            </div>
            <div class="row row--wrap summary-tags">
              <span class="code">{{ record.profile.mode }}</span>
              <span v-if="record.profile.toolchain" class="code">{{ record.profile.toolchain }}</span>
              <span v-if="record.profile.target" class="code">{{ record.profile.target }}</span>
              <span v-for="feature in record.profile.features" :key="feature" class="code">
                {{ feature }}
              </span>
              <span v-if="record.profile.no_default_features" class="code">no-default-features</span>
              <span v-for="flag in record.profile.cargo_flags" :key="flag" class="code">{{ flag }}</span>
            </div>
          </div>

          <!-- 耗时条 -->
          <div v-if="timingSegments.length" class="timing">
            <div class="timing__bar">
              <div
                v-for="seg in timingSegments"
                :key="seg.key"
                class="timing__seg"
                :style="{
                  width: `${(seg.value / timingTotal) * 100}%`,
                  background: seg.color,
                }"
                :title="`${seg.label} ${formatDuration(seg.value)}`"
              />
            </div>
            <div class="timing__legend">
              <span v-for="seg in timingSegments" :key="seg.key" class="dist-legend__item">
                <span class="dist-legend__swatch" :style="{ background: seg.color }" />
                {{ seg.label }}
                <span class="dist-legend__value">{{ formatDuration(seg.value) }}</span>
              </span>
              <span class="dist-legend__item">
                总计
                <span class="dist-legend__value">{{ formatDuration(record.timings.total_ms || null) }}</span>
              </span>
            </div>
          </div>
        </div>
      </section>

      <div class="detail-grid">
        <!-- 日志 -->
        <section class="card log-card">
          <div class="card__head">
            <div>
              <h2 class="card__title">实时日志</h2>
              <p class="card__hint mono">GET /v1/builds/{{ shortId(record.id, 10) }}/logs/stream</p>
            </div>
            <span
              class="badge"
              :class="connected ? 'badge--ok' : terminal ? 'badge--muted' : 'badge--warn'"
            >
              <span class="badge__dot" />
              {{ connected ? '流已连接' : terminal ? '流已结束' : '连接中' }}
            </span>
          </div>
          <div class="card__body card__body--flush">
            <LogViewer :events="events" :connected="connected" :notice="streamNotice" />
          </div>
        </section>

        <!-- 侧栏：元信息 + 产物 -->
        <aside class="stack">
          <section class="card">
            <div class="card__head"><h2 class="card__title">元信息</h2></div>
            <div class="card__body">
              <div class="kv">
                <span class="kv__k">提交</span>
                <span class="kv__v">{{ formatTime(record.created_at_ms) }}</span>
                <span class="kv__k">开始</span>
                <span class="kv__v">{{ formatTime(record.started_at_ms) }}</span>
                <span class="kv__k">结束</span>
                <span class="kv__v">{{ formatTime(record.finished_at_ms) }}</span>
                <span class="kv__k">日志事件</span>
                <span class="kv__v mono">{{ events.length }}</span>
              </div>
            </div>
          </section>

          <section class="card">
            <div class="card__head">
              <h2 class="card__title">产物</h2>
              <button
                v-if="artifacts.length > 1"
                class="btn btn--sm"
                type="button"
                @click="downloadAll"
              >
                全部下载
              </button>
            </div>
            <div class="card__body card__body--flush">
              <ul v-if="artifacts.length" class="artifacts">
                <li v-for="artifact in artifacts" :key="artifact.digest" class="artifact">
                  <div class="artifact__icon"><AppIcon name="package" :size="15" /></div>
                  <div class="artifact__body">
                    <span class="truncate" :title="artifact.name">{{ artifact.name }}</span>
                    <span class="dim mono">
                      {{ formatBytes(artifact.size) }} · {{ artifact.digest.slice(0, 12) }}
                    </span>
                  </div>
                  <a
                    class="btn btn--sm btn--ghost btn--icon"
                    :href="artifactUrl(artifact.digest, artifact.name)"
                    :title="`下载 ${artifact.name}`"
                    download
                  >
                    <AppIcon name="download" :size="14" />
                  </a>
                </li>
              </ul>
              <p v-else class="artifact-empty dim">
                {{ terminal ? '该构建没有产物' : '构建成功后产物会出现在这里' }}
              </p>
            </div>
          </section>
        </aside>
      </div>
    </template>
  </div>
</template>

<style scoped>
.build-id {
  font-size: var(--text-lg);
  letter-spacing: -0.02em;
}

.summary-tags {
  max-width: 60%;
  justify-content: flex-end;
}

.timing {
  margin-top: var(--sp-5);
  padding-top: var(--sp-4);
  border-top: 1px solid var(--border);
}

.timing__bar {
  display: flex;
  height: 8px;
  border-radius: var(--radius-full);
  overflow: hidden;
  background: var(--track);
}

.timing__seg {
  transition: width 400ms var(--ease);
}

.timing__legend {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-3) var(--sp-5);
  margin-top: var(--sp-3);
}

.detail-grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 320px;
  gap: var(--sp-5);
  align-items: start;
}

.log-card :deep(.console) {
  height: 560px;
  border: 0;
  border-radius: 0;
}

.artifacts {
  display: flex;
  flex-direction: column;
}

.artifact {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--border);
}

.artifact:last-child {
  border-bottom: 0;
}

.artifact__icon {
  display: grid;
  place-items: center;
  width: 30px;
  height: 30px;
  flex: none;
  border-radius: var(--radius-sm);
  background: var(--surface-2);
  border: 1px solid var(--border);
  color: var(--text-3);
}

.artifact__body {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
  font-size: var(--text-sm);
}

.artifact-empty {
  padding: var(--sp-5);
  text-align: center;
  font-size: var(--text-sm);
}

@media (max-width: 1100px) {
  .detail-grid {
    grid-template-columns: minmax(0, 1fr);
  }
  .summary-tags {
    max-width: 100%;
    justify-content: flex-start;
  }
}
</style>
