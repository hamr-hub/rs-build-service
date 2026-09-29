<script setup lang="ts">
/**
 * 提交构建。
 *
 * 表单字段与 `hotpot_core::model::BuildProfile` 一一对应；工具链下拉来自
 * `GET /v1/toolchains`，所以用户不必猜这台机器上到底装了什么。
 *
 * 客户端只做"轻量预校验"——真正的形状校验（工具链写法、target 三元组、
 * 列表规模）在服务端边界 fail fast，400 会原样回显。
 */
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import { ApiError, createBuild, getToolchains } from '../api/client'
import type { BuildMode, BuildProfile, ToolchainInventory } from '../api/types'
import { imageToolchainSpec, isValidToolchainSpec, parseList } from '../utils/format'

const router = useRouter()

const path = ref('')
const mode = ref<BuildMode>('debug')
const features = ref('')
const noDefaultFeatures = ref(false)
const target = ref('')
const toolchain = ref('')
const cargoFlags = ref('')

const submitting = ref(false)
const error = ref<string | null>(null)

const inventory = ref<ToolchainInventory | null>(null)
const inventoryError = ref<string | null>(null)
const loadingToolchains = ref(false)

/** 服务端能用的工具链规格：宿主 rustup 列表 ∪ docker 官方镜像的版本段。 */
const toolchainOptions = computed(() => {
  const inv = inventory.value
  if (!inv) return [] as string[]
  const set = new Set<string>()
  for (const item of inv.local) set.add(item.spec)
  for (const image of inv.docker_images) {
    // `rust:1.98.0-slim-bookworm` → `1.98.0`（`-slim-bookworm` 是镜像变体）
    const spec = imageToolchainSpec(image)
    if (spec) set.add(spec)
  }
  return [...set].sort()
})

/** 请求体预览：提交前能直接看到将要发给服务端的 JSON。 */
const payloadPreview = computed(() => {
  const profile: BuildProfile = {
    toolchain: toolchain.value.trim() || null,
    mode: mode.value,
    features: parseList(features.value),
    no_default_features: noDefaultFeatures.value,
    target: target.value.trim() || null,
    cargo_flags: parseList(cargoFlags.value),
  }
  return JSON.stringify({ source: { kind: 'local', path: path.value.trim() }, profile }, null, 2)
})

const canSubmit = computed(() => path.value.trim().length > 0 && !submitting.value)

onMounted(async () => {
  loadingToolchains.value = true
  try {
    inventory.value = await getToolchains()
    inventoryError.value = null
  } catch (cause) {
    inventoryError.value = cause instanceof Error ? cause.message : String(cause)
  } finally {
    loadingToolchains.value = false
  }
})

async function submit(): Promise<void> {
  if (!canSubmit.value) return
  submitting.value = true
  error.value = null
  try {
    const record = await createBuild({
      source: { kind: 'local', path: path.value.trim() },
      profile: {
        toolchain: toolchain.value.trim() || null,
        mode: mode.value,
        features: parseList(features.value),
        no_default_features: noDefaultFeatures.value,
        target: target.value.trim() || null,
        cargo_flags: parseList(cargoFlags.value),
      },
    })
    await router.push({ name: 'build-detail', params: { id: record.id } })
  } catch (cause) {
    error.value =
      cause instanceof ApiError
        ? cause.message
        : cause instanceof Error
          ? cause.message
          : String(cause)
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <div class="submit-layout">
    <div class="stack">
      <div v-if="error" class="alert alert--bad">
        <AppIcon name="alert" />
        <div>
          <strong>提交被拒绝</strong>
          <p>{{ error }}</p>
          <p class="dim">服务端在边界上 fail fast：坏请求不会占用队列或 worker。</p>
        </div>
      </div>

      <section class="card">
        <div class="card__head">
          <div>
            <h2 class="card__title">源码</h2>
            <p class="card__hint">当前仅支持服务端主机上可访问的本地 Cargo 项目</p>
          </div>
        </div>
        <div class="card__body stack">
          <div class="field">
            <label class="field__label" for="path">项目根目录（绝对路径）</label>
            <input
              id="path"
              v-model="path"
              class="input input--mono"
              placeholder="/Users/you/code/my-app"
              autocomplete="off"
              spellcheck="false"
              @keyup.enter="submit"
            />
            <p class="field__hint">
              该目录必须存在且包含 <code class="code">Cargo.toml</code>。服务端以
              <code class="code">HOTPOT_PROJECT_ROOT</code> 为允许范围。
            </p>
          </div>
        </div>
      </section>

      <section class="card">
        <div class="card__head">
          <h2 class="card__title">构建档位</h2>
        </div>
        <div class="card__body stack">
          <div class="field">
            <span class="field__label">编译模式</span>
            <div class="segmented">
              <button
                type="button"
                :class="{ 'is-active': mode === 'debug' }"
                @click="mode = 'debug'"
              >
                debug
              </button>
              <button
                type="button"
                :class="{ 'is-active': mode === 'release' }"
                @click="mode = 'release'"
              >
                release
              </button>
            </div>
          </div>

          <div class="form-grid">
            <div class="field">
              <label class="field__label" for="toolchain">工具链</label>
              <select id="toolchain" v-model="toolchain" class="select">
                <option value="">{{ loadingToolchains ? '正在发现…' : '使用 worker 默认' }}</option>
                <option v-for="option in toolchainOptions" :key="option" :value="option">
                  {{ option }}
                </option>
                <option v-if="toolchain && !toolchainOptions.includes(toolchain)" :value="toolchain">
                  {{ toolchain }}（未在清单中{{ isValidToolchainSpec(toolchain) ? '' : '，格式可能不被服务端接受' }}）
                </option>
              </select>
              <p v-if="inventoryError" class="field__error">
                工具链发现失败：{{ inventoryError }}
              </p>
              <p v-else class="field__hint">
                docker 后端下会自动把镜像版本段换成目标版本，宿主与容器保持一致。
              </p>
            </div>

            <div class="field">
              <label class="field__label" for="target">目标三元组</label>
              <input
                id="target"
                v-model="target"
                class="input input--mono"
                placeholder="宿主默认（留空即可）"
                autocomplete="off"
                spellcheck="false"
              />
              <p class="field__hint">如 <code class="code">aarch64-unknown-linux-gnu</code></p>
            </div>
          </div>

          <div class="form-grid">
            <div class="field">
              <label class="field__label" for="features">Features</label>
              <input
                id="features"
                v-model="features"
                class="input input--mono"
                placeholder="jwt, metrics"
                autocomplete="off"
                spellcheck="false"
              />
              <p class="field__hint">逗号或空格分隔</p>
            </div>

            <div class="field">
              <label class="field__label" for="flags">Cargo 参数</label>
              <input
                id="flags"
                v-model="cargoFlags"
                class="input input--mono"
                placeholder="--offline --locked"
                autocomplete="off"
                spellcheck="false"
              />
              <p class="field__hint">直接透传给 cargo，如 <code class="code">--offline</code></p>
            </div>
          </div>

          <label class="switch">
            <input v-model="noDefaultFeatures" type="checkbox" />
            <span class="switch__track" />
            <span>
              关闭默认 features
              <span class="field__hint" style="display: block">对应 <code class="code">--no-default-features</code></span>
            </span>
          </label>
        </div>
      </section>
    </div>

    <!-- 侧边：预览与提交 -->
    <aside class="submit-side">
      <section class="card">
        <div class="card__head">
          <h2 class="card__title">请求预览</h2>
          <span class="dim" style="font-size: var(--text-xs)">POST /v1/builds</span>
        </div>
        <div class="card__body card__body--flush">
          <pre class="preview mono">{{ payloadPreview }}</pre>
        </div>
        <div class="card__body submit-actions">
          <button class="btn btn--primary btn--block" type="button" :disabled="!canSubmit" @click="submit">
            <AppIcon :name="submitting ? 'clock' : 'play'" />
            {{ submitting ? '提交中…' : '开始构建' }}
          </button>
          <p class="field__hint" style="text-align: center">
            提交后自动跳转到详情页并附加实时日志流
          </p>
        </div>
      </section>
    </aside>
  </div>
</template>

<style scoped>
.submit-layout {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 360px;
  gap: var(--sp-5);
  align-items: start;
}

.submit-side {
  position: sticky;
  top: calc(var(--topbar-h) + var(--sp-6));
}

.preview {
  margin: 0;
  padding: var(--sp-4) var(--sp-5);
  max-height: 340px;
  overflow: auto;
  font-size: var(--text-xs);
  line-height: 1.6;
  color: var(--text-2);
  background: var(--log-bg);
  border-radius: 0 0 var(--radius-lg) var(--radius-lg);
}

.submit-actions {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  border-top: 1px solid var(--border);
}

@media (max-width: 1000px) {
  .submit-layout {
    grid-template-columns: minmax(0, 1fr);
  }
  .submit-side {
    position: static;
  }
}
</style>
