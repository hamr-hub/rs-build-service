/**
 * 构建器（Builder）状态。
 *
 * 之前"提交构建"是一个独立路由 `/builds/new`：从列表点进去，页面被整个换掉，
 * 构建历史和表单互相看不见，提交完还要再跳回来。真实使用里这两件事是**交替**
 * 发生的——改一个参数提交一次，看一眼日志再改下一个。
 *
 * 所以构建器改成全局抽屉：浮在当前页面之上，列表/详情/仪表盘的上下文都还在，
 * `Esc` 关掉就回到原处。同时支持**配置模板**（把一组常用档位存下来一键复用），
 * 这是把"每次重填一遍"这个最大的时间黑洞去掉的关键。
 */

import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'
import { createBuild, getToolchains } from '../api/client'
import type { BuildMode, BuildProfile, SourceSpec, ToolchainInventory } from '../api/types'
import { imageToolchainSpec, isValidToolchainSpec, parseList } from '../utils/format'
import { useToastStore } from './toast'

const TEMPLATES_KEY = 'hotpot.buildTemplates.v1'
const DRAFT_KEY = 'hotpot.buildDraft.v1'

export interface BuildTemplate {
  id: string
  name: string
  profile: BuildProfile
  /** 可选：固定源码路径，留空表示每次手填。 */
  path: string
  builtAt: number
}

export interface BuildDraft {
  path: string
  mode: BuildMode
  features: string
  noDefaultFeatures: boolean
  target: string
  toolchain: string
  cargoFlags: string
}

const EMPTY_DRAFT: BuildDraft = {
  path: '',
  mode: 'debug',
  features: '',
  noDefaultFeatures: false,
  target: '',
  toolchain: '',
  cargoFlags: '',
}

/** localStorage 可能被禁用（隐私模式）或已损坏，读写都要兜住。 */
function readJson<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key)
    return raw ? (JSON.parse(raw) as T) : fallback
  } catch {
    return fallback
  }
}

function writeJson(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value))
  } catch {
    /* 配额满或隐私模式：功能降级为不持久化，不该因此报错 */
  }
}

let nextTemplateId = Date.now()

export const useBuilderStore = defineStore('builder', () => {
  const toast = useToastStore()

  const open = ref(false)
  const draft = ref<BuildDraft>({ ...EMPTY_DRAFT, ...readJson<Partial<BuildDraft>>(DRAFT_KEY, {}) })
  const templates = ref<BuildTemplate[]>(readJson<BuildTemplate[]>(TEMPLATES_KEY, []))

  const submitting = ref(false)
  const error = ref<string | null>(null)
  const toolchains = ref<ToolchainInventory | null>(null)
  const toolchainsError = ref<string | null>(null)
  const toolchainsLoading = ref(false)

  /** 抽屉打开时用于关动的原始数据（从某条构建"以此配置重建"）。 */
  const prefillLabel = ref<string | null>(null)

  // 草稿持久化：用户填了一半切走（甚至关掉浏览器）回来还在。
  // 深度监听，因为 draft 内部字段会被直接改。
  watch(draft, (value) => writeJson(DRAFT_KEY, value), { deep: true })
  watch(templates, (value) => writeJson(TEMPLATES_KEY, value), { deep: true })

  /** 可选工具链：宿主 rustup ∪ 容器镜像里能解析出版本段的那些。 */
  const toolchainOptions = computed(() => {
    const inv = toolchains.value
    if (!inv) return [] as string[]
    const set = new Set<string>()
    for (const item of inv.local) set.add(item.spec)
    for (const image of inv.docker_images) {
      const spec = imageToolchainSpec(image)
      if (spec) set.add(spec)
    }
    return [...set].sort()
  })

  /** 手动填的、但不在清单里的工具链（合法的才允许提交）。 */
  const toolchainInvalid = computed(
    () => draft.value.toolchain.trim().length > 0 && !isValidToolchainSpec(draft.value.toolchain),
  )

  const profile = computed<BuildProfile>(() => ({
    toolchain: draft.value.toolchain.trim() || null,
    mode: draft.value.mode,
    features: parseList(draft.value.features),
    no_default_features: draft.value.noDefaultFeatures,
    target: draft.value.target.trim() || null,
    cargo_flags: parseList(draft.value.cargoFlags),
  }))

  const source = computed<SourceSpec>(() => ({ kind: 'local', path: draft.value.path.trim() }))

  const canSubmit = computed(
    () => draft.value.path.trim().length > 0 && !toolchainInvalid.value && !submitting.value,
  )

  /** 提交前的 JSON 预览：让用户在按下按钮之前就看到服务端将收到什么。 */
  const payloadPreview = computed(() =>
    JSON.stringify({ source: source.value, profile: profile.value }, null, 2),
  )

  function openBuilder(options: { path?: string; profile?: BuildProfile; label?: string } = {}): void {
    if (options.path !== undefined) draft.value.path = options.path
    if (options.profile) {
      const p = options.profile
      draft.value.mode = p.mode ?? 'debug'
      draft.value.features = (p.features ?? []).join(', ')
      draft.value.noDefaultFeatures = Boolean(p.no_default_features)
      draft.value.target = p.target ?? ''
      draft.value.toolchain = p.toolchain ?? ''
      draft.value.cargoFlags = (p.cargo_flags ?? []).join(' ')
    }
    prefillLabel.value = options.label ?? null
    error.value = null
    open.value = true
    void loadToolchains()
  }

  function close(): void {
    open.value = false
    // 不清空草稿：下次打开接着填。
    error.value = null
  }

  function reset(): void {
    draft.value = { ...EMPTY_DRAFT }
    prefillLabel.value = null
    error.value = null
  }

  async function loadToolchains(force = false): Promise<void> {
    if (toolchains.value && !force) return
    toolchainsLoading.value = true
    try {
      toolchains.value = await getToolchains()
      toolchainsError.value = null
    } catch (cause) {
      toolchainsError.value = cause instanceof Error ? cause.message : String(cause)
    } finally {
      toolchainsLoading.value = false
    }
  }

  async function submit(): Promise<string | null> {
    if (!canSubmit.value) return null
    submitting.value = true
    error.value = null
    try {
      const record = await createBuild({ source: source.value, profile: profile.value })
      toast.success('构建已提交', `${record.id.slice(0, 8)} · ${draft.value.path.trim()}`)
      return record.id
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      error.value = message
      // 留在抽屉里显示，让用户改完再提交——关闭抽屉等于把错误藏起来。
      return null
    } finally {
      submitting.value = false
    }
  }

  // ---------- 模板 ----------

  function saveTemplate(name: string): boolean {
    const trimmed = name.trim()
    if (!trimmed) return false
    const template: BuildTemplate = {
      id: `tpl_${nextTemplateId++}`,
      name: trimmed,
      profile: JSON.parse(JSON.stringify(profile.value)) as BuildProfile,
      path: draft.value.path.trim(),
      builtAt: Date.now(),
    }
    templates.value = [template, ...templates.value.filter((t) => t.name !== trimmed)]
    toast.success('模板已保存', trimmed)
    return true
  }

  function applyTemplate(template: BuildTemplate): void {
    draft.value = {
      path: template.path,
      mode: template.profile.mode,
      features: template.profile.features.join(', '),
      noDefaultFeatures: template.profile.no_default_features,
      target: template.profile.target ?? '',
      toolchain: template.profile.toolchain ?? '',
      cargoFlags: template.profile.cargo_flags.join(' '),
    }
    toast.info('已载入模板', template.name)
  }

  function removeTemplate(id: string): void {
    const target = templates.value.find((t) => t.id === id)
    templates.value = templates.value.filter((t) => t.id !== id)
    if (target) toast.info('模板已删除', target.name)
  }

  return {
    open,
    draft,
    templates,
    submitting,
    error,
    toolchains,
    toolchainsError,
    toolchainsLoading,
    prefillLabel,
    toolchainOptions,
    toolchainInvalid,
    profile,
    source,
    canSubmit,
    payloadPreview,
    openBuilder,
    close,
    reset,
    loadToolchains,
    submit,
    saveTemplate,
    applyTemplate,
    removeTemplate,
  }
})
