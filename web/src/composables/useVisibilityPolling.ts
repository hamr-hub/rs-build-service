/**
 * 页面可见时才轮询。
 *
 * 后台标签页里的定时器是纯浪费：用户看不见，却一直在打服务端。更糟的是
 * 几百个后台标签页能把构建服务压垮。`visibilitychange` 切回来时立刻补一次，
 * 这样"回来就看到最新状态"，而不是等下一个轮询周期。
 */

import { onBeforeUnmount, onMounted, ref } from 'vue'

export function useVisibilityPolling(fn: () => void | Promise<void>, intervalMs: number) {
  const visible = ref(document.visibilityState === 'visible')
  let timer: number | undefined
  let running = false

  function tick(): void {
    if (running) return // 上一轮还没回来就跳过，避免请求堆积
    running = true
    Promise.resolve(fn()).finally(() => {
      running = false
    })
  }

  function start(): void {
    if (timer) return
    timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') tick()
    }, intervalMs)
  }

  function stop(): void {
    if (!timer) return
    window.clearInterval(timer)
    timer = undefined
  }

  function onVisibilityChange(): void {
    const nowVisible = document.visibilityState === 'visible'
    visible.value = nowVisible
    // 切回前台立刻补一次，用户不用等一个周期。
    if (nowVisible) tick()
  }

  onMounted(() => {
    document.addEventListener('visibilitychange', onVisibilityChange)
    start()
    tick()
  })

  onBeforeUnmount(() => {
    document.removeEventListener('visibilitychange', onVisibilityChange)
    stop()
  })

  return { visible, tick, start, stop }
}
