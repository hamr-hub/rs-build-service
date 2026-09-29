<script setup lang="ts">
/** 指标卡：左侧色条按 tone 取语义色，一眼扫过去就知道好坏。 */
withDefaults(
  defineProps<{
    label: string
    value: string
    meta?: string
    tone?: 'ok' | 'bad' | 'warn' | 'active' | 'accent' | 'neutral'
  }>(),
  { meta: '', tone: 'neutral' },
)
</script>

<template>
  <div class="stat" :style="{ '--stat-accent': `var(--${tone === 'accent' ? 'accent' : tone})` }">
    <div class="stat__label">{{ label }}</div>
    <div class="stat__value">{{ value }}</div>
    <div v-if="meta" class="stat__meta">{{ meta }}</div>
  </div>
</template>
