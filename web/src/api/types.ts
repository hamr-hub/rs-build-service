/**
 * 与 Rust 端 `serde` 类型一一对应的接口定义。
 *
 * 字段名刻意保持 snake_case —— 它们就是 `/v1/*` 响应里的 JSON 键，
 * 这样 UI 层不需要任何"驼峰化"中间层，减少一处可能出错的转换。
 */

/** 构建状态机的七个状态（`hotpot_core::model::BuildStatus`）。 */
export type BuildStatus =
  | 'queued'
  | 'dispatched'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'canceled'
  | 'timeout'

/** 构建档位的编译模式。 */
export type BuildMode = 'debug' | 'release'

/** 日志事件的分类（决定前端如何着色与过滤）。 */
export type EventKind = 'stdout' | 'stderr' | 'phase' | 'status'

/** 源码来源。`upload` 目前服务端会明确拒绝，UI 里只作为只读展示。 */
export type SourceSpec =
  | { kind: 'local'; path: string }
  | { kind: 'git'; url: string; ref_name: string; sha: string | null }
  | { kind: 'upload'; upload_id: string; root: string | null }

/** 提交构建时可配的档位（`BuildProfile`）。 */
export interface BuildProfile {
  toolchain: string | null
  mode: BuildMode
  features: string[]
  no_default_features: boolean
  target: string | null
  cargo_flags: string[]
}

/** 五段耗时，单位毫秒；`link_ms` 目前服务端恒为 null。 */
export interface BuildTimings {
  queue_ms: number
  fetch_ms: number
  build_ms: number
  link_ms: number | null
  upload_ms: number
  total_ms: number
}

/** 一条构建记录。 */
export interface BuildRecord {
  id: string
  project_id?: string
  source: SourceSpec
  profile: BuildProfile
  status: BuildStatus
  timings: BuildTimings
  created_at_ms: number
  started_at_ms: number | null
  finished_at_ms: number | null
  error: string | null
}

/** 单条日志事件，`seq` 在同一构建内单调递增（断线续传靠它）。 */
export interface BuildEvent {
  build_id: string
  seq: number
  timestamp_ms: number
  kind: EventKind
  payload: string
}

/** 产物元信息；`digest` 是 blake3 内容哈希，可直接拼下载链接。 */
export interface ArtifactMeta {
  name: string
  digest: string
  size: number
  attrs: Record<string, string>
}

/** 宿主上已安装的一条 rustup 工具链。 */
export interface InstalledToolchain {
  spec: string
  rustc: string
}

/** `GET /v1/toolchains` 的响应。 */
export interface ToolchainInventory {
  default_rustc: string | null
  local: InstalledToolchain[]
  docker_images: string[]
  docker_arch: string | null
  warnings: string[]
}

/** 提交构建的请求体（`POST /v1/builds`）。 */
export interface CreateBuildRequest {
  source: SourceSpec
  profile: BuildProfile
}

/** 终态集合 —— 终态构建不会再变化，可以停止轮询。 */
export const TERMINAL_STATUSES: readonly BuildStatus[] = [
  'succeeded',
  'failed',
  'canceled',
  'timeout',
]

export function isTerminal(status: BuildStatus): boolean {
  return TERMINAL_STATUSES.includes(status)
}

/** 状态的展示元信息：中文标签 + 语义色键。 */
export const STATUS_META: Record<BuildStatus, { label: string; tone: Tone; hint: string }> = {
  queued: { label: '排队中', tone: 'neutral', hint: '已入队，等待 worker 空闲' },
  dispatched: { label: '执行中', tone: 'active', hint: '已被 worker 认领，正在执行' },
  running: { label: '执行中', tone: 'active', hint: '正在执行' },
  succeeded: { label: '成功', tone: 'ok', hint: '构建成功，产物已入 CAS' },
  failed: { label: '失败', tone: 'bad', hint: '构建失败' },
  canceled: { label: '已取消', tone: 'muted', hint: '被手动取消' },
  timeout: { label: '超时', tone: 'warn', hint: '超过单构建超时上限' },
}

export type Tone = 'ok' | 'bad' | 'warn' | 'active' | 'neutral' | 'muted'
