<script setup lang="ts">
/** 状态徽标：状态 → 语义色 → 文案。非终态带呼吸点，提示"还在动"。 */
import { computed } from 'vue'
import type { BuildStatus } from '../api/types'
import { STATUS_META, isTerminal } from '../api/types'

const props = defineProps<{ status: BuildStatus }>()

const meta = computed(() => STATUS_META[props.status])
const live = computed(() => !isTerminal(props.status))
</script>

<template>
  <span
    class="badge"
    :class="[`badge--${meta.tone}`, { 'badge--pulse': live }]"
    :title="meta.hint"
  >
    <span class="badge__dot" />
    {{ meta.label }}
  </span>
</template>
