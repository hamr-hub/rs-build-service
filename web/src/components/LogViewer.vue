<script setup lang="ts">
/**
 * 构建日志控制台（窗口化渲染）。
 *
 * 性能是这个组件存在的理由。一个 ripgrep 级别的构建能产出十几万行日志，
 * 全部塞进 DOM 会让页面卡死、内存吃掉几百 MB。这里只渲染视口附近的行，
 * DOM 节点数与总行数无关。
 *
 * 代价是**行高必须固定**，所以日志改成终端式的单行不换行（横向滚动），
 * 而不是之前的 `pre-wrap`。这对日志来说本来就更合适：一行编译警告就该是
 * 一行，自动折行反而让 seq 与内容的对应关系变得难以扫读。
 *
 * 内存也要有上界：超过 `MAX_EVENTS` 后丢弃最旧的事件，而不是无限累积。
 */
import { computed, nextTick, ref, watch } from 'vue'
import AppIcon from './AppIcon.vue'
import { useVirtualList } from '../composables/useVirtualList'
import type { BuildEvent, EventKind } from '../api/types'
import { formatClock } from '../utils/format'

const props = withDefaults(
  defineProps<{
    events: BuildEvent[]
    connected?: boolean
    notice?: string | null
  }>(),
  { connected: false, notice: null },
)

/** DOM 中保留的最大行数（超出丢弃最旧的）。 */
const MAX_EVENTS = 20_000
/** 行高，必须与 CSS 中的 --log-row-h 一致。 */
const ROW_HEIGHT = 21

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

const body = ref<HTMLElement | null>(null)
const follow = ref(true)
const atBottom = ref(true)

/** 过滤 + 截断后的可见事件。 */
const lines = computed(() => {
  const filtered = props.events.filter((e) => enabled.value[e.kind])
  return filtered.length > MAX_EVENTS ? filtered.slice(filtered.length - MAX_EVENTS) : filtered
})

const { visibleItems, offsetTop, totalHeight } = useVirtualList(lines, body, {
  itemHeight: ROW_HEIGHT,
  overscan: 20,
})

/** 截断了多少行——要让用户知道"上面还有内容"，否则会以为构建没输出过。 */
const droppedCount = computed(() =>
  Math.max(0, props.events.length - lines.value.length),
)

function toggle(kind: EventKind): void {
  enabled.value = { ...enabled.value, [kind]: !enabled.value[kind] }
  follow.value = true
  void nextTick(jumpToLatest)
}

function onScroll(): void {
  const el = body.value
  if (!el) return
  const distance = el.scrollHeight - el.scrollTop - el.clientHeight
  atBottom.value = distance < ROW_HEIGHT * 2
  // 用户主动上翻就不再抢滚动条；回到底部自动恢复跟随。
  follow.value = atBottom.value
}

function jumpToLatest(): void {
  const el = body.value
  if (!el) return
  el.scrollTop = el.scrollHeight
  follow.value = true
}

// 新事件到达时：只有在跟随状态才滚动。用户正在翻历史就不打断。
watch(
  () => lines.value.length,
  async () => {
    if (!follow.value) return
    await nextTick()
    const el = body.value
    if (el) el.scrollTop = el.scrollHeight
  },
)

// 切换构建时回到跟随，并立刻贴到底部。
watch(
  () => props.events[0]?.build_id,
  async () => {
    follow.value = true
    await nextTick(jumpToLatest)
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
          :aria-pressed="enabled[filter.key]"
          @click="toggle(filter.key)"
        >
          {{ filter.label }}
        </button>
      </div>
      <div class="spacer" />
      <span class="console__count">
        {{ lines.length.toLocaleString() }}<template v-if="droppedCount"> / {{ events.length.toLocaleString() }}</template> 行
      </span>
      <button
        v-if="!atBottom"
        class="console__toggle is-on"
        type="button"
        title="回到最新"
        @click="jumpToLatest"
      >
        <AppIcon name="arrowRight" :size="12" />
        跟随已暂停
      </button>
    </div>

    <div v-if="notice" class="console__notice">
      <AppIcon name="refresh" :size="13" />
      {{ notice }}
    </div>

    <div ref="body" class="console__body" role="log" aria-live="polite" @scroll.passive="onScroll">
      <p v-if="!lines.length" class="console__empty">
        {{ connected ? '已连接，等待日志输出…' : '暂无日志事件' }}
      </p>

      <template v-else>
        <div v-if="droppedCount" class="log-dropped">
          <AppIcon name="info" :size="12" />
          为控制内存只保留最近 {{ MAX_EVENTS.toLocaleString() }} 行（已丢弃 {{ droppedCount.toLocaleString() }} 行）
        </div>
        <!--
          窗口化的标准结构：canvas 撑出等于全部行高的滚动范围，
          viewport 再用 translateY 移到当前窗口。滚动条长度因此始终反映
          真实总行数，而不是"渲染出来的那几十行"。
        -->
        <div class="log-canvas" :style="{ height: `${totalHeight}px` }">
          <div class="log-viewport" :style="{ transform: `translateY(${offsetTop}px)` }">
            <div
              v-for="event in visibleItems"
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
    </div>
  </div>
</template>

<style scoped>
.console__count {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  color: #6b615b;
  white-space: nowrap;
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
  flex: none;
}

.log-canvas {
  position: relative;
  width: 100%;
}

.log-viewport {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  /* 只改 transform 不触发布局，配合 contain 让滚动更稳 */
  will-change: transform;
}

.log-dropped {
  position: sticky;
  top: 0;
  z-index: 1;
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-4);
  background: #191512;
  border-bottom: 1px dashed #3a312b;
  color: #8b8079;
  font-size: var(--text-xs);
}
</style>
