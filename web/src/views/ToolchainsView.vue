<script setup lang="ts">
/**
 * 工具链清单 —— 回答"这台机器上到底能用什么"。
 *
 * 端点是软失败设计：daemon 不可达、rustup 不存在只会体现在 `warnings`，
 * 不会让整个请求 500。所以这里把 warnings 显式展示出来，而不是当成正常。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import AppIcon from '../components/AppIcon.vue'
import EmptyState from '../components/EmptyState.vue'
import { ApiError, getToolchains } from '../api/client'
import type { ToolchainInventory } from '../api/types'
import { formatBytes, formatExecutor, imageToolchainSpec } from '../utils/format'
import { useServerStore } from '../stores/server'

const server = useServerStore()

const inventory = ref<ToolchainInventory | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)

const localCount = computed(() => inventory.value?.local.length ?? 0)
const dockerCount = computed(() => inventory.value?.docker_images.length ?? 0)

async function load(): Promise<void> {
  loading.value = true
  try {
    inventory.value = await getToolchains()
    error.value = null
  } catch (cause) {
    error.value = cause instanceof ApiError ? cause.message : String(cause)
    inventory.value = null
  } finally {
    loading.value = false
  }
}

onMounted(load)
let poller: number | undefined
onMounted(() => {
  poller = window.setInterval(() => {
    if (server.online) void load()
  }, 20_000)
})
onUnmounted(() => {
  if (poller) window.clearInterval(poller)
})

/** 镜像对应的工具链 spec；浮动标签（如 `rust:slim-bookworm`）没有版本段。 */
function imageSpec(image: string): string {
  return imageToolchainSpec(image) ?? '浮动 stable'
}
</script>

<template>
  <div class="stack">
    <div class="row row--between">
      <p class="dim" style="font-size: var(--text-sm)">
        <code class="code">GET /v1/toolchains</code> —— 宿主 rustup 与 docker 镜像的合并清单
      </p>
      <button class="btn btn--sm" type="button" :disabled="loading" @click="load">
        <AppIcon name="refresh" />
        {{ loading ? '刷新中…' : '刷新' }}
      </button>
    </div>

    <div v-if="error" class="alert alert--bad">
      <AppIcon name="alert" />
      {{ error }}
    </div>

    <template v-if="inventory">
      <!-- 非致命告警：工具缺失 / daemon 不可达 -->
      <div v-for="(warning, index) in inventory.warnings" :key="index" class="alert alert--warn">
        <AppIcon name="alert" />
        {{ warning }}
      </div>

      <div class="stat-grid">
        <div class="stat" style="--stat-accent: var(--accent)">
          <div class="stat__label">默认 rustc</div>
          <div class="stat__value" style="font-size: var(--text-md)">
            {{ inventory.default_rustc ?? '不可用' }}
          </div>
          <div class="stat__meta">宿主默认工具链</div>
        </div>
        <div class="stat" style="--stat-accent: var(--ok)">
          <div class="stat__label">宿主工具链</div>
          <div class="stat__value">{{ localCount }}</div>
          <div class="stat__meta">rustup 已安装</div>
        </div>
        <div class="stat" style="--stat-accent: var(--active)">
          <div class="stat__label">容器镜像</div>
          <div class="stat__value">{{ dockerCount }}</div>
          <div class="stat__meta">
            {{ inventory.docker_arch ? `daemon 架构 ${inventory.docker_arch}` : 'daemon 架构未知' }}
          </div>
        </div>
      </div>

      <div class="grid-2">
        <section class="card">
          <div class="card__head">
            <h2 class="card__title">宿主 rustup 工具链</h2>
            <span class="dim" style="font-size: var(--text-xs)">{{ localCount }} 条</span>
          </div>
          <div class="card__body card__body--flush">
            <div v-if="localCount" class="table-wrap">
              <table class="table">
                <thead>
                  <tr>
                    <th>规格</th>
                    <th>rustc 版本</th>
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="item in inventory.local" :key="item.spec">
                    <td class="mono">{{ item.spec }}</td>
                    <td class="dim mono truncate" :title="item.rustc">{{ item.rustc }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
            <EmptyState
              v-else
              icon="cube"
              title="没有检测到 rustup 工具链"
              description="如果服务端跑在容器里，通常由工具链镜像提供编译环境。"
            />
          </div>
        </section>

        <section class="card">
          <div class="card__head">
            <h2 class="card__title">Docker 工具链镜像</h2>
            <span class="dim" style="font-size: var(--text-xs)">{{ dockerCount }} 条</span>
          </div>
          <div class="card__body card__body--flush">
            <ul v-if="dockerCount" class="images">
              <li v-for="image in inventory.docker_images" :key="image" class="image">
                <div class="image__icon"><AppIcon name="package" :size="15" /></div>
                <div class="image__body">
                  <span class="mono truncate">{{ image }}</span>
                  <span class="dim">工具链 <code class="code">{{ imageSpec(image) }}</code></span>
                </div>
              </li>
            </ul>
            <EmptyState
              v-else
              icon="server"
              title="没有缓存的官方镜像"
              description="docker executor 首次构建时会自动拉取对应版本的 rust 镜像。"
            />
          </div>
        </section>
      </div>

      <section v-if="server.metrics" class="card">
        <div class="card__head"><h2 class="card__title">缓存占用</h2></div>
        <div class="card__body">
          <div class="kv">
            <span class="kv__k">执行器</span>
            <span class="kv__v mono">{{ formatExecutor(server.metrics.executor) }}</span>
            <span class="kv__k">CAS 总占用</span>
            <span class="kv__v mono">{{ formatBytes(server.metrics.storeBytes) }}</span>
            <template v-for="(bytes, protocol) in server.metrics.cacheIndexBytes" :key="protocol">
              <span class="kv__k">{{ protocol }} 索引</span>
              <span class="kv__v mono">{{ formatBytes(bytes) }}</span>
            </template>
          </div>
        </div>
      </section>
    </template>

    <div v-else-if="loading" class="stack">
      <span class="skeleton" style="height: 92px" />
      <span class="skeleton" style="height: 240px" />
    </div>
  </div>
</template>

<style scoped>
.grid-2 {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: var(--sp-5);
  align-items: start;
}

.images {
  display: flex;
  flex-direction: column;
}

.image {
  display: flex;
  align-items: center;
  gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--border);
}

.image:last-child {
  border-bottom: 0;
}

.image__icon {
  display: grid;
  place-items: center;
  width: 30px;
  height: 30px;
  flex: none;
  border-radius: var(--radius-sm);
  background: var(--surface-2);
  border: 1px solid var(--border);
  color: var(--text-3);
}

.image__body {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
  font-size: var(--text-sm);
}

@media (max-width: 900px) {
  .grid-2 {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
