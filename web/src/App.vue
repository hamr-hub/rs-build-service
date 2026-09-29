<script setup lang="ts">
/**
 * 应用外壳：侧边导航 + 顶栏 + 路由视图。
 *
 * 顶栏标题不写死 —— 由各视图通过 `definePageMeta` 风格的方式传入太绕，
 * 这里直接读路由 meta 的 title，副标题用 `route.meta.subtitle`。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { navRoutes } from './router'
import { useBuildsStore } from './stores/builds'
import { useServerStore } from './stores/server'
import AppIcon from './components/AppIcon.vue'

const route = useRoute()
const router = useRouter()
const server = useServerStore()
const builds = useBuildsStore()

/** 服务地址设置面板。 */
const showSettings = ref(false)
const draftBase = ref(server.base)
const savedNote = ref<string | null>(null)

const title = computed(() => (route.meta.title as string | undefined) ?? 'Hotpot')
const subtitle = computed(() => route.meta.subtitle as string | undefined)

const navItems = computed(() =>
  navRoutes.map((r) => ({
    name: r.name as string,
    title: r.meta?.title as string,
    icon: r.meta?.icon as string,
  })),
)

function isActive(name: unknown): boolean {
  if (name === 'dashboard') return route.name === 'dashboard'
  // "提交构建" 在详情页里也要保持高亮，因为它和 builds 是同一条脉络。
  if (name === 'build-new') return route.name === 'build-new'
  return route.name === name
}

function openSettings(): void {
  draftBase.value = server.base
  savedNote.value = null
  showSettings.value = true
}

function saveBase(): void {
  server.updateBase(draftBase.value.trim() || null)
  savedNote.value = '已切换服务，正在重新探测…'
  window.setTimeout(() => {
    showSettings.value = false
    void builds.load()
  }, 600)
}

let heartbeat: number | undefined
let listPoller: number | undefined

onMounted(() => {
  server.init()
  void builds.load()
  // 15s 探一次健康；列表里还有在跑的构建时加密轮询。
  heartbeat = window.setInterval(() => void server.check(), 15_000)
  listPoller = window.setInterval(() => {
    if (builds.hasActive) void builds.load()
  }, 5_000)
})

onUnmounted(() => {
  if (heartbeat) window.clearInterval(heartbeat)
  if (listPoller) window.clearInterval(listPoller)
})

// 路由切换时如果服务刚恢复，连带把数据补上。
watch(
  () => server.state,
  (next) => {
    if (next === 'online') {
      void builds.load()
      void server.refreshMetrics()
    }
  },
)

function onKey(event: KeyboardEvent): void {
  if (event.key === 'Escape' && showSettings.value) showSettings.value = false
}

onMounted(() => window.addEventListener('keydown', onKey))
onUnmounted(() => window.removeEventListener('keydown', onKey))
</script>

<template>
  <div class="app">
    <aside class="sidebar">
      <div class="brand">
        <div class="brand__mark">
          <svg width="19" height="19" viewBox="0 0 24 24" fill="none" stroke="var(--on-accent)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 3c-3 3.4-5.4 6-5.4 9a5.4 5.4 0 0 0 10.8 0c0-1.6-.7-3.1-1.8-4.4-.3 1-1 1.7-1.8 1.7-1 0-1.8-.8-1.8-2 0-1.5.7-3 2-4.3z" />
            <path d="M8.5 21h7" />
          </svg>
        </div>
        <div>
          <div class="brand__name">Hotpot</div>
          <div class="brand__sub">Console</div>
        </div>
      </div>

      <nav class="nav">
        <p class="nav__label">构建平台</p>
        <RouterLink
          v-for="item in navItems"
          :key="String(item.name)"
          :to="{ name: String(item.name) }"
          class="nav__item"
          :class="{ 'is-active': isActive(item.name) }"
        >
          <AppIcon :name="item.icon" />
          {{ item.title }}
        </RouterLink>
      </nav>

      <div class="sidebar__footer">
        <button class="server-chip" type="button" title="切换服务地址" @click="openSettings">
          <span
            class="dot"
            :class="server.state === 'online' ? 'is-online' : server.state === 'offline' ? 'is-offline' : 'is-unknown'"
          />
          <span class="server-chip__url">{{ server.base }}</span>
          <AppIcon name="chevronDown" :size="13" />
        </button>
      </div>
    </aside>

    <div class="main">
      <header class="topbar">
        <div class="topbar__title">
          <h1>{{ title }}</h1>
          <p v-if="subtitle">{{ subtitle }}</p>
        </div>
        <div class="topbar__actions">
          <span
            class="badge"
            :class="{
              'badge--ok': server.state === 'online',
              'badge--bad': server.state === 'offline',
              'badge--neutral': server.state === 'unknown',
            }"
          >
            <span class="badge__dot" />
            {{
              server.state === 'online'
                ? '服务在线'
                : server.state === 'offline'
                  ? '服务离线'
                  : '探测中'
            }}
          </span>
          <button
            class="btn btn--ghost btn--icon"
            type="button"
            :title="server.theme === 'dark' ? '切换到浅色' : '切换到深色'"
            @click="server.toggleTheme()"
          >
            <AppIcon :name="server.theme === 'dark' ? 'sun' : 'moon'" />
          </button>
          <button
            class="btn btn--ghost btn--icon"
            type="button"
            title="刷新"
            @click="builds.load(); server.refreshMetrics()"
          >
            <AppIcon name="refresh" />
          </button>
          <button class="btn btn--primary" type="button" @click="router.push({ name: 'build-new' })">
            <AppIcon name="plus" />
            提交构建
          </button>
        </div>
      </header>

      <main class="page">
        <RouterView />
      </main>
    </div>

    <!-- 服务地址设置 -->
    <div v-if="showSettings" class="modal" @click.self="showSettings = false">
      <div class="modal__panel" role="dialog" aria-modal="true" aria-label="服务设置">
        <div class="modal__head">
          <h2>服务地址</h2>
          <button class="btn btn--ghost btn--icon" type="button" @click="showSettings = false">
            <AppIcon name="info" :size="14" />
          </button>
        </div>
        <div class="modal__body stack stack--tight">
          <div class="field">
            <label class="field__label" for="api-base">API 基址</label>
            <input
              id="api-base"
              v-model="draftBase"
              class="input input--mono"
              placeholder="/api"
              @keyup.enter="saveBase"
            />
            <p class="field__hint">
              留空则走 Vite 开发代理（<code class="code">/api</code> → 服务端）。
              直接填写地址需要服务端或反向代理开启 CORS。
            </p>
          </div>
          <p v-if="savedNote" class="field__hint">{{ savedNote }}</p>
        </div>
        <div class="modal__foot">
          <button class="btn" type="button" @click="server.updateBase(null); showSettings = false">
            用默认值
          </button>
          <div class="spacer" />
          <button class="btn" type="button" @click="showSettings = false">取消</button>
          <button class="btn btn--primary" type="button" @click="saveBase">保存</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.dot {
  width: 7px;
  height: 7px;
  flex: none;
  border-radius: 50%;
}
.dot.is-online {
  background: var(--ok);
  box-shadow: 0 0 0 3px var(--ok-soft);
}
.dot.is-offline {
  background: var(--bad);
  box-shadow: 0 0 0 3px var(--bad-soft);
}
.dot.is-unknown {
  background: var(--warn);
  box-shadow: 0 0 0 3px var(--warn-soft);
}

.modal {
  position: fixed;
  inset: 0;
  z-index: 100;
  display: grid;
  place-items: center;
  padding: var(--sp-4);
  background: var(--overlay);
  backdrop-filter: blur(4px);
  animation: fade var(--speed) var(--ease);
}

.modal__panel {
  width: min(520px, 100%);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  background: var(--surface);
  box-shadow: var(--shadow-lg);
  animation: rise 220ms var(--ease);
}

.modal__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--sp-4) var(--sp-5);
  border-bottom: 1px solid var(--border);
}

.modal__head h2 {
  font-size: var(--text-md);
}

.modal__body {
  padding: var(--sp-5);
}

.modal__foot {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-4) var(--sp-5);
  border-top: 1px solid var(--border);
  background: var(--surface-2);
  border-radius: 0 0 var(--radius-lg) var(--radius-lg);
}

@keyframes fade {
  from {
    opacity: 0;
  }
}

@keyframes rise {
  from {
    opacity: 0;
    transform: translateY(10px) scale(0.985);
  }
}
</style>
