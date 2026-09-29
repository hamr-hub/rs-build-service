<script setup lang="ts">
/** 内联图标集。刻意不引第三方图标库：只需十来个图形，内联更轻。 */
import { computed } from 'vue'

const props = withDefaults(defineProps<{ name: string; size?: number }>(), { size: 16 })

// 24×24 网格下的 path 数据，统一 stroke 风格（无填充），视觉更整齐。
const PATHS: Record<string, string> = {
  grid: 'M4 4h6v6H4zM14 4h6v6h-6zM4 14h6v6H4zM14 14h6v6h-6z',
  stack: 'M4 7h16M4 12h16M4 17h10',
  plus: 'M12 5v14M5 12h14',
  cube: 'M12 3l8 4.5v9L12 21l-8-4.5v-9L12 3zM12 12l8-4.5M12 12v9M12 12L4 7.5',
  refresh: 'M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6',
  play: 'M7 4.5l12 7.5-12 7.5v-15z',
  stop: 'M6 6h12v12H6z',
  download: 'M12 4v10m0 0l-4-4m4 4l4-4M5 18h14',
  search: 'M11 19a8 8 0 1 1 0-16 8 8 0 0 1 0 16zM21 21l-4.3-4.3',
  check: 'M5 13l4 4L19 7',
  alert: 'M12 8v5M12 17h.01M10.3 4.3L2.6 18a2 2 0 0 0 1.7 3h15.4a2 2 0 0 0 1.7-3L13.7 4.3a2 2 0 0 0-3.4 0z',
  info: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 11v5M12 8h.01',
  arrowRight: 'M5 12h14M13 6l6 6-6 6',
  arrowLeft: 'M19 12H5M11 18l-6-6 6-6',
  chevronDown: 'M6 9l6 6 6-6',
  copy: 'M9 9h10v10H9zM5 15V5h10',
  sun: 'M12 17a5 5 0 1 0 0-10 5 5 0 0 0 0 10zM12 2v2M12 20v2M4.2 4.2l1.4 1.4M18.4 18.4l1.4 1.4M2 12h2M20 12h2M4.2 19.8l1.4-1.4M18.4 5.6l1.4-1.4',
  moon: 'M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5z',
  folder: 'M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z',
  terminal: 'M5 5h14v14H5zM8.5 10l2.5 2.5-2.5 2.5M13 15h4',
  clock: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 7v5l3 2',
  trend: 'M3 17l6-6 4 4 8-8M15 7h6v6',
  package: 'M12 3l8 4.5v9L12 21l-8-4.5v-9L12 3zM8 7.8l4 2.2 4-2.2M12 10v11',
  server: 'M4 4h16v6H4zM4 14h16v6H4zM7.5 7h.01M7.5 17h.01',
  external: 'M14 4h6v6M20 4l-8 8M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
}

const path = computed(() => PATHS[props.name] ?? PATHS.info)
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.7"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    class="app-icon"
  >
    <path :d="path" />
  </svg>
</template>

<style scoped>
.app-icon {
  display: block;
  flex: none;
}
</style>
