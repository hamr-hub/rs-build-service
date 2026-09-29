/**
 * 全局瞬时通知。
 *
 * 为什么需要：之前所有反馈都塞在页面里的 `.alert` 块。问题是页面滚动时它就在
 * 视口外——用户点了"取消构建"，列表在三屏之外，那个红色横幅他根本看不到。
 * 瞬时反馈（取消成功、提交失败、已复制）必须自己浮到视野里。
 *
 * 持续性的、影响决策的信息（比如"服务离线，请检查地址"）仍然留在页面内，
 * 两者职责不同：toast 回答"刚才那下怎么样"，页内 alert 回答"现在能不能干活"。
 */

import { defineStore } from 'pinia'
import { ref } from 'vue'

export type ToastTone = 'ok' | 'bad' | 'warn' | 'info'

export interface Toast {
  id: number
  tone: ToastTone
  title: string
  description?: string
  /** 毫秒；0 表示需要手动关闭。 */
  timeout: number
  action?: { label: string; run: () => void }
}

let nextId = 1

export const useToastStore = defineStore('toast', () => {
  const toasts = ref<Toast[]>([])
  const timers = new Map<number, number>()

  function dismiss(id: number): void {
    const timer = timers.get(id)
    if (timer) {
      window.clearTimeout(timer)
      timers.delete(id)
    }
    toasts.value = toasts.value.filter((t) => t.id !== id)
  }

  function push(toast: Omit<Toast, 'id' | 'timeout'> & { timeout?: number }): number {
    const id = nextId++
    // 同 id 覆盖不可能发生（id 单调），但同标题的重复 toast 会让人烦：
    // 轮询重试时尤其明显。这里按 tone+title 去重。
    const existing = toasts.value.find((t) => t.title === toast.title && t.tone === toast.tone)
    if (existing) {
      dismiss(existing.id)
    }
    const timeout = toast.timeout ?? (toast.tone === 'bad' ? 8000 : 4000)
    toasts.value = [...toasts.value, { ...toast, id, timeout }]
    if (timeout > 0) {
      timers.set(
        id,
        window.setTimeout(() => dismiss(id), timeout),
      )
    }
    return id
  }

  const success = (title: string, description?: string) =>
    push({ tone: 'ok', title, description })
  const error = (title: string, description?: string) => push({ tone: 'bad', title, description })
  const warn = (title: string, description?: string) => push({ tone: 'warn', title, description })
  const info = (title: string, description?: string) => push({ tone: 'info', title, description })

  function clear(): void {
    for (const timer of timers.values()) window.clearTimeout(timer)
    timers.clear()
    toasts.value = []
  }

  return { toasts, push, success, error, warn, info, dismiss, clear }
})
