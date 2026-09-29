<script setup lang="ts">
/**
 * 渲染错误兜底。
 *
 * Vue 默认在渲染出错时把整棵子树清空并把错误打到 console——对用户来说就是
 * "白屏 + 控制台里一堆红字"，完全不知道发生了什么、也没法继续用。
 * 这里把错误留在界面里，并给出**可操作**的出路（重试渲染 / 回到总览 /
 * 看详情），因为"出错了但看得见、点一下就能继续"才是生产级的要求。
 */
import { onErrorCaptured, ref } from 'vue'
import { useRouter } from 'vue-router'
import AppIcon from './AppIcon.vue'

const props = withDefaults(defineProps<{ label?: string }>(), { label: '页面' })

const router = useRouter()
const error = ref<Error | null>(null)
const attempts = ref(0)

onErrorCaptured((err) => {
  // 记到 console 方便排查，但不"再抛一次"——那会变成未处理 rejection。
  console.error(`[${props.label}] render error`, err)
  error.value = err instanceof Error ? err : new Error(String(err))
  return false
})

function retry(): void {
  error.value = null
  attempts.value += 1
}

function goHome(): void {
  error.value = null
  void router.push({ name: 'dashboard' })
}
</script>

<template>
  <!-- key 变化会强制重建子树，是"重试渲染"唯一可靠的做法 -->
  <slot v-if="!error" :key="attempts" />
  <div v-else class="boundary">
    <div class="boundary__icon"><AppIcon name="alert" :size="22" /></div>
    <h2 class="boundary__title">{{ label }}出错了</h2>
    <p class="boundary__msg">{{ error.message }}</p>
    <div class="boundary__actions">
      <button class="btn btn--primary" type="button" @click="retry">重试</button>
      <button class="btn" type="button" @click="goHome">回到总览</button>
      <button class="btn btn--ghost" type="button" @click="error = null">忽略并继续</button>
    </div>
  </div>
</template>

<style scoped>
.boundary {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: var(--sp-3);
  padding: var(--sp-7) var(--sp-5);
  text-align: center;
}

.boundary__icon {
  display: grid;
  place-items: center;
  width: 52px;
  height: 52px;
  border-radius: var(--radius-lg);
  background: var(--bad-soft);
  color: var(--bad);
}

.boundary__title {
  font-size: var(--text-lg);
}

.boundary__msg {
  max-width: 60ch;
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  color: var(--text-2);
  word-break: break-word;
}

.boundary__actions {
  display: flex;
  gap: var(--sp-2);
  margin-top: var(--sp-2);
}
</style>
