<script setup lang="ts">
import AppIcon from './AppIcon.vue'

withDefaults(
  defineProps<{ icon?: string; title: string; description?: string }>(),
  { icon: 'info', description: '' },
)
</script>

<template>
  <div class="empty">
    <div class="empty__icon"><AppIcon :name="icon" :size="20" /></div>
    <p class="empty__title">{{ title }}</p>
    <p v-if="description">{{ description }}</p>
    <slot />
  </div>
</template>
