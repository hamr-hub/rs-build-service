<script setup lang="ts">
/**
 * 构建日志控制台。
 *
 * 事件由父组件通过 SSE 推入。超过 `MAX_LINES` 后从头部丢弃 —— 长构建
 * 的日志能到几万行，全量留在 DOM 里会让滚动卡死。
 */
import { computed, nextTick, ref, watch } from 'vue'
import AppIcon from './AppIcon.vue'
import type { BuildEvent, EventKind } from '../api/types'
import { formatClock } from '../utils/format'

const props = withDefaults(
  defineProps<{
    events: BuildEvent[]
    connected?: boolean
    /** 断线重连提示；非空时展示。 */
    notice?: string | null
  }>(),
  { connected: false, notice: null },
)

/** DOM 中保留的最大行数。 */
const MAX_LINES = 4000

const STREAM_FILTERS: { key: EventKind; label: string }[] = [
  { key: 'stdout', label: 'stdout' },
  { key: 'stderr', label: 'stderr' },
  { key: 'phase', label: 'phase' },
  { key: 'status', label: 'status' },
]

const enabled = ref<Record<EventKind, boolean>>({
  stdout: true,
  stderr: true,
  phase: true,
  status: true,
})

/** 跟随滚动：用户手动往上翻时自动暂停，避免"抢滚动条"。 */
const follow = ref(true)
const body = ref<HTMLElement | null>(null)

const visible = computed(() => {
  const out = props.events.filter((e) => enabled.value[e.kind])
  return out.length > MAX_LINES ? out.slice(out.length - MAX_LINES) : out
})

function toggle(kind: EventKind): void {
  enabled.value = { ...enabled.value, [kind]: !enabled.value[kind] }
}

function onScroll(): void {
  const el = body.value
  if (!el) return
  // 距底部 24px 内视为"贴着底部"，继续跟随。
  follow.value = el.scrollHeight - el.scrollTop - el.clientHeight < 24
}

function jumpToLatest(): void {
  follow.value = true
  void nextTick(() => {
    const el = body.value
    if (el) el.scrollTop = el.scrollHeight
  })
}

watch(
  () => props.events.length,
  async () => {
    if (!follow.value) return
    await nextTick()
    const el = body.value
    if (el) el.scrollTop = el.scrollHeight
  },
)
</script>

<template>
  <div class="console">
    <div class="console__bar">
      <div class="console__toggles">
        <button
          v-for="filter in STREAM_FILTERS"
          :key="filter.key"
          class="console__toggle"
          :class="{ 'is-on': enabled[filter.key] }"
          type="button"
          @click="toggle(filter.key)"
        >
          {{ filter.label }}
        </button>
      </div>
      <div class="spacer" />
      <span class="console__meta">
        <span class="console__count">{{ visible.length }} 行</span>
        <button
          v-if="!follow"
          class="console__toggle is-on"
          type="button"
          title="回到最新"
          @click="jumpToLatest"
        >
          <AppIcon name="arrowRight" :size="12" />
          跟随已暂停
        </button>
      </span>
    </div>

    <div v-if="notice" class="console__notice">
      <AppIcon name="refresh" :size="13" />
      {{ notice }}
    </div>

    <div ref="body" class="console__body" @scroll.passive="onScroll">
      <p v-if="!visible.length" class="console__empty">
        {{ connected ? '已连接，等待日志输出…' : '暂无日志事件' }}
      </p>
      <div
        v-for="event in visible"
        :key="event.seq"
        class="log-line"
        :class="`log-line--${event.kind}`"
      >
        <span class="log-line__seq">{{ event.seq }}</span>
        <span class="log-line__time">{{ formatClock(event.timestamp_ms) }}</span>
        <span class="log-line__text">{{ event.payload }}</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.console__meta {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
}

.console__count {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: #6b615b;
}

.console__notice {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-4);
  background: rgba(240, 169, 44, 0.1);
  border-bottom: 1px solid rgba(240, 169, 44, 0.25);
  color: #f0a92c;
  font-size: var(--text-xs);
}
</style>
