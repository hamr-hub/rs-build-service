/**
 * 全局快捷键。
 *
 * 生产级界面的硬指标：不碰鼠标也能完成主要操作。这里只绑定**单键**或
 * **简单组合**，且在输入框/可编辑区域里自动让位——打字时按 `n` 不该
 * 弹出新建构建面板。
 *
 * 支持两种写法：
 * - `k` / `/` / `escape`：单键；
 * - `mod+k`：`Cmd`（macOS）或 `Ctrl`；
 * - `g d`：序列键——先按 `g`，1.2s 内再按 `d`（仿 vim 的 `gd` 跳总览）。
 */

import { onBeforeUnmount, onMounted, type Ref } from 'vue'

export interface Hotkey {
  /** 形如 `k`、`/`、`mod+k`、`g d`。 */
  combo: string
  handler: (event: KeyboardEvent) => void
  /** 允许在输入框内触发，默认 false。 */
  allowInInput?: boolean
  /** 展示用说明（快捷键面板里显示）。 */
  description?: string
}

const EDITABLE_TAGS = new Set(['INPUT', 'TEXTAREA', 'SELECT'])
/** 序列键的超时：超过就放弃当前的 prefix。 */
const SEQUENCE_TIMEOUT_MS = 1200

function isEditing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false
  if (target.isContentEditable) return true
  return EDITABLE_TAGS.has(target.tagName)
}

/** 把事件归一化成 combo 字符串，便于与注册表比对。 */
function comboOf(event: KeyboardEvent): string {
  const key = event.key
  const name = key === ' ' ? 'space' : key.toLowerCase()
  return event.metaKey || event.ctrlKey ? `mod+${name}` : name
}

export function useHotkeys(hotkeys: Hotkey[], enabled: Ref<boolean> = { value: true } as Ref<boolean>) {
  const prefixes = new Set(
    hotkeys.map((h) => h.combo.split(' ')[0]).filter((c) => c.includes(' ')),
  )

  let pending: string | null = null
  let pendingAt = 0

  function onKeyDown(event: KeyboardEvent): void {
    if (!enabled.value) return
    // Shift/Alt 组合留给浏览器和系统（如 Shift+F10）。
    if (event.shiftKey || event.altKey) return

    const editing = isEditing(event.target)

    // 1) 先看是否是某个序列的后半段。
    if (pending && Date.now() - pendingAt < SEQUENCE_TIMEOUT_MS) {
      const sequence = `${pending} ${event.key.toLowerCase()}`
      const hit = hotkeys.find((h) => h.combo === sequence)
      pending = null
      if (hit && (!editing || hit.allowInInput)) {
        event.preventDefault()
        hit.handler(event)
      }
      return
    }
    pending = null

    // 2) 单键 / mod 组合。
    const combo = comboOf(event)
    const direct = hotkeys.find((h) => h.combo === combo)
    if (direct && (!editing || direct.allowInInput)) {
      event.preventDefault()
      direct.handler(event)
      return
    }

    // 3) 序列键的前缀。
    if (!editing && prefixes.has(combo)) {
      pending = combo
      pendingAt = Date.now()
    }
  }

  onMounted(() => window.addEventListener('keydown', onKeyDown))
  onBeforeUnmount(() => window.removeEventListener('keydown', onKeyDown))

  return { hotkeys }
}
