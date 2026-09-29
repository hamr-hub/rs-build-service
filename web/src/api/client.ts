/**
 * Hotpot HTTP API 客户端。
 *
 * 关于基址：上游 `hotpot-server` **没有** CORS 中间件，所以浏览器不能直接
 * 跨源调 `http://127.0.0.1:7878`。开发时用 Vite 代理（见 `vite.config.ts`）
 * 把 `/api/*` 转发到真实服务端，同源调用，问题消失。
 *
 * 优先级：`localStorage` 里用户手动指定的地址 > 构建期注入的
 * `VITE_HOTPOT_API` > `/api`（走开发代理）。
 */

import type {
  ArtifactMeta,
  BuildEvent,
  BuildRecord,
  BuildStatus,
  CreateBuildRequest,
  ToolchainInventory,
} from './types'

const OVERRIDE_KEY = 'hotpot.apiBase'

function resolveBase(): string {
  const injected = import.meta.env.VITE_HOTPOT_API as string | undefined
  const stored = localStorage.getItem(OVERRIDE_KEY)
  return (stored || injected || '/api').replace(/\/+$/, '')
}

let base = resolveBase()

export function apiBase(): string {
  return base
}

/** UI 顶栏的"服务地址"设置：留空即回到代理默认值。 */
export function setApiBase(value: string | null): void {
  const trimmed = (value ?? '').trim()
  if (trimmed) {
    localStorage.setItem(OVERRIDE_KEY, trimmed)
    base = trimmed.replace(/\/+$/, '')
  } else {
    localStorage.removeItem(OVERRIDE_KEY)
    base = resolveBase()
  }
}

export function hasApiOverride(): boolean {
  return localStorage.getItem(OVERRIDE_KEY) !== null
}

/** 面向用户的错误：保留服务端原文，附带请求路径便于排障。 */
export class ApiError extends Error {
  readonly status: number
  readonly path: string

  constructor(status: number, path: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.path = path
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response
  try {
    response = await fetch(`${base}${path}`, {
      ...init,
      headers: {
        Accept: 'application/json',
        ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
        ...init?.headers,
      },
    })
  } catch {
    throw new ApiError(0, path, `无法连接服务 ${base} —— 请确认 hotpot-server 已启动`)
  }

  if (!response.ok) {
    throw new ApiError(response.status, path, await readError(response, path))
  }
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

/** 服务端错误体是 `{ "error": "..." }`，退化时退回状态码文案。 */
async function readError(response: Response, path: string): Promise<string> {
  try {
    const body = (await response.json()) as { error?: string }
    if (body?.error) return body.error
  } catch {
    /* 非 JSON 错误体，走下面的兜底 */
  }
  return `${response.status} ${response.statusText || '请求失败'}（${path}）`
}

// ---------- 构建 ----------

export interface ListBuildsParams {
  status?: BuildStatus | null
  limit?: number
  offset?: number
}

export function listBuilds(params: ListBuildsParams = {}, signal?: AbortSignal): Promise<BuildRecord[]> {
  const query = new URLSearchParams()
  if (params.status) query.set('status', params.status)
  if (params.limit != null) query.set('limit', String(params.limit))
  if (params.offset != null) query.set('offset', String(params.offset))
  const suffix = query.toString()
  return request<BuildRecord[]>(`/v1/builds${suffix ? `?${suffix}` : ''}`, { signal })
}

export function getBuild(id: string, signal?: AbortSignal): Promise<BuildRecord> {
  return request<BuildRecord>(`/v1/builds/${encodeURIComponent(id)}`, { signal })
}

export function createBuild(payload: CreateBuildRequest): Promise<BuildRecord> {
  return request<BuildRecord>('/v1/builds', {
    method: 'POST',
    body: JSON.stringify(payload),
  })
}

export function cancelBuild(id: string): Promise<BuildRecord> {
  return request<BuildRecord>(`/v1/builds/${encodeURIComponent(id)}/cancel`, { method: 'POST' })
}

export function listArtifacts(id: string, signal?: AbortSignal): Promise<ArtifactMeta[]> {
  return request<ArtifactMeta[]>(`/v1/builds/${encodeURIComponent(id)}/artifacts`, { signal })
}

/** 产物下载链接。`filename` 会写进 `Content-Disposition`。 */
export function artifactUrl(digest: string, filename?: string): string {
  const query = filename ? `?filename=${encodeURIComponent(filename)}` : ''
  return `${base}/v1/artifacts/${encodeURIComponent(digest)}${query}`
}

// ---------- 工具链 ----------

export function getToolchains(signal?: AbortSignal): Promise<ToolchainInventory> {
  return request<ToolchainInventory>('/v1/toolchains', { signal })
}

// ---------- 健康检查 ----------

export async function ping(signal?: AbortSignal): Promise<boolean> {
  try {
    const response = await fetch(`${base}/healthz`, { signal })
    return response.ok && (await response.text()).trim() === 'ok'
  } catch {
    return false
  }
}

// ---------- SSE 日志流 ----------

/** SSE 里服务端用的事件名，与 `EventKind` 对齐，另加两个控制事件。 */
const STREAM_EVENTS = ['stdout', 'stderr', 'phase', 'status', 'end', 'error'] as const

export interface LogStreamHandlers {
  /** SSE 通道建立成功。 */
  onOpen: () => void
  onEvent: (event: BuildEvent) => void
  /** 服务端主动结束（`end` 事件）或流自然关闭。 */
  onEnd: () => void
  /** 连接层错误（非服务端 `error` 事件），`retrying` 表示会自动重连。 */
  onError: (message: string, retrying: boolean) => void
}

export interface LogStreamHandle {
  close: () => void
  /** 已消费到的最大 seq，用于断线续传。 */
  lastSeq: () => number
}

/**
 * 附加到构建的 SSE 日志流。
 *
 * `since` 让重连可以续传：断线期间服务端仍在写事件，浏览器补不上这段，
 * 所以重连时把已见的最大 seq 传回去，服务端只补发更新的。
 */
export function streamLogs(
  buildId: string,
  since: number,
  handlers: LogStreamHandlers,
): LogStreamHandle {
  let source: EventSource | null = null
  let closed = false
  let seq = since
  let retryTimer: number | undefined
  let attempt = 0

  const url = `${base}/v1/builds/${encodeURIComponent(buildId)}/logs/stream?since=${seq}`

  const connect = () => {
    if (closed) return
    source = new EventSource(url)

    source.addEventListener('open', () => {
      attempt = 0
      handlers.onOpen()
    })

    for (const name of STREAM_EVENTS) {
      source.addEventListener(name, (raw) => {
        // 服务端发的具名 `error` 事件是 MessageEvent；浏览器自身的连接错误是
        // ErrorEvent，没有 `data`。靠这个把两者分开。
        if (name === 'error' && !('data' in raw)) {
          scheduleReconnect('日志流连接中断')
          return
        }
        const data = (raw as MessageEvent<string>).data
        if (name === 'end') {
          handlers.onEnd()
          return
        }
        if (name === 'error') {
          scheduleReconnect(data || '日志流出错')
          return
        }
        let event: BuildEvent
        try {
          event = JSON.parse(data) as BuildEvent
        } catch {
          return
        }
        if (typeof event.seq === 'number') seq = Math.max(seq, event.seq)
        handlers.onEvent(event)
      })
    }

    // EventSource 自带重连，但它重连时不带 since，会从头重放。
    // 关掉自带重连，改由我们控制（携带正确的 since）。
    source.onerror = () => {
      if (closed) return
      scheduleReconnect('日志流连接中断')
    }
  }

  const scheduleReconnect = (message: string) => {
    source?.close()
    source = null
    if (closed) return
    attempt += 1
    handlers.onError(message, true)
    const delay = Math.min(1000 * 2 ** (attempt - 1), 8000)
    retryTimer = window.setTimeout(connect, delay)
  }

  connect()

  return {
    close: () => {
      closed = true
      if (retryTimer) window.clearTimeout(retryTimer)
      source?.close()
      source = null
    },
    lastSeq: () => seq,
  }
}

// ---------- Prometheus 指标 ----------

export interface MetricsSnapshot {
  buildsByStatus: Record<string, number>
  queueDepth: number | null
  queueOldestWaitMs: number | null
  buildDurationAvgMs: number | null
  cacheHitRatio: Record<string, number>
  cacheLookups: Record<string, number>
  cacheHits: Record<string, number>
  cacheMisses: Record<string, number>
  cachePuts: Record<string, number>
  cacheIndexEntries: Record<string, number>
  cacheIndexBytes: Record<string, number>
  storeBytes: number | null
  workers: number | null
  executor: string | null
  version: string | null
  toolchain: string | null
}

const EMPTY_SNAPSHOT: MetricsSnapshot = {
  buildsByStatus: {},
  queueDepth: null,
  queueOldestWaitMs: null,
  buildDurationAvgMs: null,
  cacheHitRatio: {},
  cacheLookups: {},
  cacheHits: {},
  cacheMisses: {},
  cachePuts: {},
  cacheIndexEntries: {},
  cacheIndexBytes: {},
  storeBytes: null,
  workers: null,
  executor: null,
  version: null,
  toolchain: null,
}

interface Sample {
  value: number
  labels: Record<string, string>
}

/**
 * 迷你 Prometheus 文本解析器 —— 只够吃下 `hotpot-server` 自己渲染的那套
 * 输出（`name{labels} value` / `# HELP` / `# TYPE`），不追求通用。
 */
function parsePrometheus(text: string): Map<string, Sample[]> {
  const out = new Map<string, Sample[]>()
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed || trimmed.startsWith('#')) continue
    const braceAt = trimmed.indexOf('{')
    let name: string
    let labels: Record<string, string> = {}
    let rest: string
    if (braceAt === -1) {
      const spaceAt = trimmed.indexOf(' ')
      if (spaceAt === -1) continue
      name = trimmed.slice(0, spaceAt)
      rest = trimmed.slice(spaceAt + 1)
    } else {
      name = trimmed.slice(0, braceAt)
      // 标签值里可能含 `{`/`}`（executor 是 Rust 的 Debug 输出，形如
      // `Docker { image: \"rust:slim\", docker_host: None }`），所以不能
      // 直接 indexOf('}') —— 必须跳过引号内的花括号。
      const braceEnd = findLabelBlockEnd(trimmed, braceAt)
      if (braceEnd === -1) continue
      labels = parseLabels(trimmed.slice(braceAt + 1, braceEnd))
      rest = trimmed.slice(braceEnd + 1)
    }
    const value = Number.parseFloat(rest.trim())
    if (!Number.isFinite(value)) continue
    const samples = out.get(name) ?? []
    samples.push({ value, labels })
    out.set(name, samples)
  }
  return out
}

/**
 * 找到标签块的闭合 `}`。
 *
 * 从 `open`（首个 `{`）开始扫描，跳过双引号包裹的标签值（支持 `\` 转义），
 * 返回与之配对的 `}` 下标；找不到返回 -1。
 */
function findLabelBlockEnd(line: string, open: number): number {
  let inQuotes = false
  for (let i = open + 1; i < line.length; i += 1) {
    const ch = line[i]
    if (ch === '\\') {
      i += 1 // 跳过被转义的字符
      continue
    }
    if (ch === '"') {
      inQuotes = !inQuotes
      continue
    }
    if (ch === '}' && !inQuotes) return i
  }
  return -1
}

function parseLabels(source: string): Record<string, string> {
  const labels: Record<string, string> = {}
  // 标签值里可能有转义引号，所以不能简单按逗号切。
  const pattern = /([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"/g
  let match: RegExpExecArray | null
  while ((match = pattern.exec(source)) !== null) {
    labels[match[1]] = match[2].replace(/\\"/g, '"').replace(/\\n/g, '\n').replace(/\\\\/g, '\\')
  }
  return labels
}

function byLabel(samples: Sample[] | undefined, label: string): Record<string, number> {
  const out: Record<string, number> = {}
  for (const sample of samples ?? []) {
    const key = sample.labels[label]
    if (key != null) out[key] = sample.value
  }
  return out
}

function first(samples: Sample[] | undefined): number | null {
  return samples?.[0]?.value ?? null
}

/** 按标签取值；服务端同一指标会按 `phase` 拆成多条，必须显式挑。 */
function byLabelValue(samples: Sample[] | undefined, label: string, key: string): number | null {
  return samples?.find((s) => s.labels[label] === key)?.value ?? null
}

export async function getMetrics(signal?: AbortSignal): Promise<MetricsSnapshot> {
  const response = await fetch(`${base}/metrics`, { signal, headers: { Accept: 'text/plain' } })
  if (!response.ok) throw new ApiError(response.status, '/metrics', '指标端点不可用')
  const parsed = parsePrometheus(await response.text())
  const info = parsed.get('hotpot_info')?.[0]

  return {
    ...EMPTY_SNAPSHOT,
    buildsByStatus: byLabel(parsed.get('hotpot_builds_by_status'), 'status'),
    queueDepth: first(parsed.get('hotpot_queue_depth')),
    queueOldestWaitMs: first(parsed.get('hotpot_queue_oldest_wait_ms')),
    buildDurationAvgMs: byLabelValue(
      parsed.get('hotpot_build_duration_ms_avg'),
      'phase',
      'total',
    ),
    cacheHitRatio: byLabel(parsed.get('hotpot_cache_hit_ratio'), 'protocol'),
    cacheLookups: byLabel(parsed.get('hotpot_cache_lookups_total'), 'protocol'),
    cacheHits: byLabel(parsed.get('hotpot_cache_hits_total'), 'protocol'),
    cacheMisses: byLabel(parsed.get('hotpot_cache_misses_total'), 'protocol'),
    cachePuts: byLabel(parsed.get('hotpot_cache_puts_total'), 'protocol'),
    cacheIndexEntries: byLabel(parsed.get('hotpot_cache_index_entries'), 'protocol'),
    cacheIndexBytes: byLabel(parsed.get('hotpot_cache_index_bytes'), 'protocol'),
    storeBytes: first(parsed.get('hotpot_store_bytes')),
    workers: first(parsed.get('hotpot_workers')),
    executor: info?.labels.executor ?? null,
    version: info?.labels.version ?? null,
    toolchain: info?.labels.toolchain ?? null,
  }
}
