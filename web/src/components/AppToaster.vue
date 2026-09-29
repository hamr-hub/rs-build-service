<script setup lang="ts">
/** 通知容器。放在 App 根部，跨路由存活。 */
import { useToastStore } from '../stores/toast'
import AppIcon from './AppIcon.vue'

const toast = useToastStore()

const ICONS: Record<string, string> = {
  ok: 'check',
  bad: 'alert',
  warn: 'alert',
  info: 'info',
}
</script>

<template>
  <div class="toaster" role="region" aria-label="通知" aria-live="polite">
    <TransitionGroup name="toast">
      <div
        v-for="item in toast.toasts"
        :key="item.id"
        class="toast"
        :class="`toast--${item.tone}`"
        :role="item.tone === 'bad' ? 'alert' : 'status'"
      >
        <AppIcon :name="ICONS[item.tone] ?? 'info'" :size="16" class="toast__icon" />
        <div class="toast__body">
          <p class="toast__title">{{ item.title }}</p>
          <p v-if="item.description" class="toast__desc">{{ item.description }}</p>
        </div>
        <button
          v-if="item.action"
          class="toast__action"
          type="button"
          @click="item.action.run(); toast.dismiss(item.id)"
        >
          {{ item.action.label }}
        </button>
        <button class="toast__close" type="button" aria-label="关闭通知" @click="toast.dismiss(item.id)">
          <AppIcon name="close" :size="13" />
        </button>
      </div>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.toaster {
  position: fixed;
  right: var(--sp-5);
  bottom: var(--sp-5);
  z-index: 200;
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  width: min(380px, calc(100vw - var(--sp-6)));
  pointer-events: none;
}

.toast {
  display: flex;
  align-items: flex-start;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface);
  box-shadow: var(--shadow-lg);
  pointer-events: auto;
}

.toast__icon {
  margin-top: 2px;
  flex: none;
}

.toast--ok .toast__icon { color: var(--ok); }
.toast--bad .toast__icon { color: var(--bad); }
.toast--warn .toast__icon { color: var(--warn); }
.toast--info .toast__icon { color: var(--active); }

.toast__body {
  flex: 1;
  min-width: 0;
}

.toast__title {
  font-size: var(--text-base);
  font-weight: 600;
}

.toast__desc {
  margin-top: 2px;
  font-size: var(--text-sm);
  color: var(--text-2);
  word-break: break-word;
}

.toast__action {
  flex: none;
  align-self: center;
  padding: 2px var(--sp-2);
  border: 0;
  border-radius: var(--radius-sm);
  background: var(--surface-3);
  color: var(--text);
  font-size: var(--text-sm);
  font-weight: 600;
  cursor: pointer;
}

.toast__action:hover {
  background: var(--accent-soft);
  color: var(--accent);
}

.toast__close {
  flex: none;
  display: grid;
  place-items: center;
  width: 20px;
  height: 20px;
  border: 0;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-3);
  cursor: pointer;
}

.toast__close:hover {
  background: var(--surface-3);
  color: var(--text);
}

/* 进入/退出：轻微上浮 + 淡入，不做横向滑动——纵向更符合"从下方出现"的直觉 */
.toast-enter-active,
.toast-leave-active {
  transition: opacity 200ms var(--ease), transform 200ms var(--ease);
}

.toast-enter-from {
  opacity: 0;
  transform: translateY(8px) scale(0.98);
}

.toast-leave-to {
  opacity: 0;
  transform: translateX(12px);
}

.toast-move {
  transition: transform 200ms var(--ease);
}
</style>
