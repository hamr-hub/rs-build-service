/** 展示层格式化工具：所有"给人看的"数字与时间都从这里走，保持全站一致。 */

/** 毫秒 → 人类可读时长。跨度大时分段（1.2s / 3m 04s / 1h 12m）。 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  if (ms < 1000) return `${Math.round(ms)}ms`

  const seconds = ms / 1000
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 2 : 1)}s`

  const minutes = Math.floor(seconds / 60)
  const restSeconds = Math.round(seconds % 60)
  if (minutes < 60) return `${minutes}m ${String(restSeconds).padStart(2, '0')}s`

  const hours = Math.floor(minutes / 60)
  const restMinutes = minutes % 60
  return `${hours}h ${String(restMinutes).padStart(2, '0')}m`
}

const BYTE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB'] as const

export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes)) return '—'
  if (bytes < 1024) return `${bytes} B`
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(value >= 100 ? 0 : value >= 10 ? 1 : 2)} ${BYTE_UNITS[unit]}`
}

const pad = (n: number) => String(n).padStart(2, '0')

/** Unix 毫秒 → 本地时间 `MM-DD HH:mm:ss`。 */
export function formatTime(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  const date = new Date(ms)
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(
    date.getMinutes(),
  )}:${pad(date.getSeconds())}`
}

/** Unix 毫秒 → `HH:mm:ss`，日志时间戳用。 */
export function formatClock(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return '--:--:--'
  const date = new Date(ms)
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

/** 相对时间：`刚刚 / 3 分钟前 / 2 小时前 / 5 天前`。 */
export function formatRelative(ms: number | null | undefined, now = Date.now()): string {
  if (ms == null || !Number.isFinite(ms)) return '—'
  const delta = Math.max(0, now - ms)
  if (delta < 45_000) return '刚刚'
  const minutes = Math.floor(delta / 60_000)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  if (days < 30) return `${days} 天前`
  return formatTime(ms)
}

/** 百分比，`ratio` 为 0-1。 */
export function formatPercent(ratio: number | null | undefined, digits = 1): string {
  if (ratio == null || !Number.isFinite(ratio)) return '—'
  return `${(ratio * 100).toFixed(digits)}%`
}

/** 短 ID：`bld_3f2a…` —— 表格里够用，悬停可看全量。 */
export function shortId(id: string, head = 10): string {
  return id.length <= head + 2 ? id : `${id.slice(0, head)}…`
}

/** 路径压成 `…/parent/project`，表格里保住尾部可辨识信息。 */
export function shortenPath(path: string, keep = 2): string {
  const parts = path.split('/').filter(Boolean)
  if (parts.length <= keep) return path
  return `…/${parts.slice(-keep).join('/')}`
}

/** 把 `{a:1, b:2}` 形式的 features 字符串拆成数组。 */
export function parseList(raw: string): string[] {
  return raw
    .split(/[,\s]+/)
    .map((item) => item.trim())
    .filter(Boolean)
}

/**
 * 把执行器的 Rust `Debug` 输出整理成人话。
 *
 * 服务端上报的是 `format!("{:?}", executor)`，实际形如
 * `Docker { image: "rust:slim-bookworm", docker_host: None }` 或 `Local`。
 * 直接甩给用户看没有意义，这里拆出真正有用的部分。
 */
export function formatExecutor(raw: string | null | undefined): string {
  if (!raw) return '—'
  const trimmed = raw.trim()
  if (!trimmed.toLowerCase().startsWith('docker')) return trimmed.toLowerCase()

  const image = trimmed.match(/image:\s*"([^"]*)"/)?.[1]
  const host = trimmed.match(/docker_host:\s*Some\("([^"]*)"\)/)?.[1]
  if (!image) return 'docker'
  return host ? `docker · ${image} · ${host}` : `docker · ${image}`
}

/**
 * 工具链 spec 的合法写法，必须与 `hotpot_core::toolchain::parse_toolchain`
 * 保持一致：`stable` / `beta` / `nightly` / `nightly-YYYY-MM-DD` /
 * `X` / `X.Y` / `X.Y.Z`。
 */
const TOOLCHAIN_SPEC_RE = /^(?:stable|beta|nightly|nightly-\d{4}-\d{2}-\d{2}|\d+(?:\.\d+){0,2})$/

export function isValidToolchainSpec(spec: string): boolean {
  return TOOLCHAIN_SPEC_RE.test(spec.trim().toLowerCase())
}

/**
 * 从官方 `rust:` 镜像标签里取出可提交的工具链 spec，取不到返回 null。
 *
 * 镜像标签形如 `1.98.0-slim-bookworm`，其中 `slim-bookworm` 是**变体**而不是
 * 版本段 —— 直接把 `1.98.0-slim-bookworm` 当 spec 提交会被服务端
 * `parse_toolchain` 以 400 拒掉。这里与 `resolve_rust_image` 的推导保持一致：
 * 按首个 `-` 切开，只有前半段能解析成工具链时才认。
 *
 * `rust:slim-bookworm` 这类浮动标签的首段是 `slim`，不是版本，因此返回 null
 * （它本身等价于 stable，而 stable 已经在宿主工具链清单里）。
 */
export function imageToolchainSpec(image: string): string | null {
  const tag = image.split(':').slice(1).join(':')
  if (!tag) return null
  const head = tag.split('-')[0] ?? ''
  return isValidToolchainSpec(head) ? head : null
}
