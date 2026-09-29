/**
 * 窗口化渲染（virtual scrolling）。
 *
 * 为什么必须做：构建日志动辄几万行，DOM 里每行一个带 3 个 span 的节点，
 * 10k 行就已经让滚动掉帧、内存飙到几百 MB。窗口化只渲染视口附近的行，
 * DOM 节点数恒定在 O(可见行数 + overscan)，与总行数无关。
 *
 * 代价：需要一个确定高度的滚动容器 + 绝对定位的 spacer。这对等宽字体的
 * 日志行是合适的（日志本来就该等宽单行），对流式布局不适用。
 */

import { computed, onBeforeUnmount, onMounted, ref, type Ref } from 'vue'

export interface VirtualListOptions {
  /** 每行高度（px）。必须与实际渲染的行高一致，否则滚动条会跳。 */
  itemHeight: number
  /** 视口外额外渲染的行数，避免快速滚动时露白。 */
  overscan?: number
}

export function useVirtualList<T>(
  items: Ref<T[]>,
  container: Ref<HTMLElement | null>,
  options: VirtualListOptions,
) {
  const overscan = options.overscan ?? 12
  const scrollTop = ref(0)
  const viewportHeight = ref(0)

  /** 容器可滚动的总高度（= 行数 × 行高）。 */
  const totalHeight = computed(() => items.value.length * options.itemHeight)

  const startIndex = computed(() => {
    const first = Math.floor(scrollTop.value / options.itemHeight) - overscan
    return Math.max(0, first)
  })

  const endIndex = computed(() => {
    const visible = Math.ceil(viewportHeight.value / options.itemHeight) + overscan * 2
    return Math.min(items.value.length, startIndex.value + visible)
  })

  /** 实际要渲染的切片。 */
  const visibleItems = computed(() => items.value.slice(startIndex.value, endIndex.value))

  /** 切片在容器内的纵向偏移：把 startIndex 之前的行高折叠成一个 spacer。 */
  const offsetTop = computed(() => startIndex.value * options.itemHeight)

  let frame = 0

  function onScroll(): void {
    const el = container.value
    if (!el) return
    // scroll 事件触发频率高于渲染帧率；用 rAF 合并，避免重复布局计算。
    if (frame) return
    frame = window.requestAnimationFrame(() => {
      frame = 0
      if (container.value) scrollTop.value = container.value.scrollTop
    })
  }

  function measure(): void {
    const el = container.value
    if (el) viewportHeight.value = el.clientHeight
  }

  let observer: ResizeObserver | null = null

  onMounted(() => {
    const el = container.value
    if (!el) return
    measure()
    el.addEventListener('scroll', onScroll, { passive: true })
    // 容器尺寸会随窗口/布局变化，窗口化必须跟着重算，否则底部会露白。
    observer = new ResizeObserver(measure)
    observer.observe(el)
  })

  onBeforeUnmount(() => {
    const el = container.value
    if (el) el.removeEventListener('scroll', onScroll)
    if (frame) window.cancelAnimationFrame(frame)
    observer?.disconnect()
  })

  return { visibleItems, startIndex, endIndex, offsetTop, totalHeight, measure }
}
