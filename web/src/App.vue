<script setup lang="ts">
/**
 * 应用外壳：侧边导航 + 顶栏 + 路由视图 + 全局构建器抽屉 + 通知。
 *
 * 构建器从"独立页面"改成"全局抽屉"是这个版本最关键的产品改动：
 * 提交构建和浏览构建历史是交替发生的，浮层让用户不用在两个页面间来回跳。
 * 任何页面按 `n` 或点右上角按钮都能就地唤起，`Esc` 关掉回到原处。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { navRoutes } from './router'
import { useBuildsStore } from './stores/builds'
import { useBuilderStore } from './stores/builder'
import { useServerStore } from './stores/server'
import { useToastStore } from './stores/toast'
import { useHotkeys } from './composables/useHotkeys'
import { useVisibilityPolling } from './composables/useVisibilityPolling'
import AppIcon from './components/AppIcon.vue'
import AppToaster from './components/AppToaster.vue'
import BuildDrawer from './components/BuildDrawer.vue'
import ErrorBoundary from './components/ErrorBoundary.vue'

const route = useRoute()
const router = useRouter()
const server = useServerStore()
const builds = useBuildsStore()
const builder = useBuilderStore()
const toast = useToastStore()

/** 服务地址设置面板。 */
const showSettings = ref(false)
const draftBase = ref(server.base)
const savedNote = ref<string | null>(null)
const showShortcuts = ref(false)

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

function refreshAll(): void {
  void builds.load()
  void server.check()
  void server.refreshMetrics()
}

// ---------- 轮询：仅在页面可见时进行 ----------

useVisibilityPolling(() => {
  void server.check()
  // 没有在途构建时用慢档；有在途构建时加快，让状态及时收敛。
  if (builds.hasActive) void builds.load()
}, 6000)

onMounted(() => {
  server.init()
  void builds.load()
  void server.refreshMetrics()
})

// 服务恢复时把数据补上；断开时给一次明确提示（只提示一次，不刷屏）。
let wasOffline = false
watch(
  () => server.state,
  (next, prev) => {
    if (next === 'online') {
      void builds.load()
      void server.refreshMetrics()
      if (prev === 'offline') toast.success('服务已恢复', server.base)
    } else if (next === 'offline' && !wasOffline) {
      wasOffline = true
      toast.error('服务连接中断', `${server.base} 无法访问，正在自动重试`)
    }
  },
)

// ---------- 快捷键 ----------

const drawerOpen = computed(() => builder.open)

useHotkeys([
  { combo: 'n', handler: () => builder.openBuilder(), description: '新建构建' },
  { combo: 'mod+k', handler: () => builder.openBuilder(), description: '新建构建' },
  { combo: '/', handler: () => router.push({ name: 'builds' }), description: '构建列表' },
  { combo: 'g d', handler: () => router.push({ name: 'dashboard' }), description: '总览' },
  { combo: 'g b', handler: () => router.push({ name: 'builds' }), description: '构建' },
  { combo: 'g t', handler: () => router.push({ name: 'toolchains' }), description: '工具链' },
  { combo: 'r', handler: refreshAll, description: '刷新' },
  {
    combo: '?',
    handler: () => (showShortcuts.value = true),
    description: '快捷键',
    allowInInput: true,
  },
])

function onKey(event: KeyboardEvent): void {
  if (event.key === 'Escape') {
    if (showSettings.value) showSettings.value = false
    else if (showShortcuts.value) showShortcuts.value = false
  }
}

onMounted(() => window.addEventListener('keydown', onKey))
onUnmounted(() => window.removeEventListener('keydown', onKey))

void drawerOpen
</script>

<template>
  <div class="app">
    <a class="skip-link" href="#main-content">跳到主要内容</a>

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

      <nav class="nav" aria-label="主导航">
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
        <p class="sidebar__hint">
          <kbd class="kbd">N</kbd> 新建 · <kbd class="kbd">?</kbd> 快捷键
        </p>
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
            :title="server.lastCheckAt ? `上次探测 ${new Date(server.lastCheckAt).toLocaleTimeString()}` : ''"
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
          <button class="btn btn--ghost btn--icon" type="button" title="刷新 (R)" @click="refreshAll">
            <AppIcon name="refresh" />
          </button>
          <button class="btn btn--primary" type="button" data-drawer-trigger @click="builder.openBuilder()">
            <AppIcon name="plus" />
            提交构建
            <kbd class="kbd kbd--on-accent">N</kbd>
          </button>
        </div>
      </header>

      <main id="main-content" class="page">
        <ErrorBoundary :label="String(title)">
          <RouterView />
        </ErrorBoundary>
      </main>
    </div>

    <!-- 全局构建器：任何页面都能唤起，保留当前上下文 -->
    <BuildDrawer />
    <AppToaster />

    <!-- 服务地址设置 -->
    <div v-if="showSettings" class="modal" @click.self="showSettings = false">
      <div class="modal__panel" role="dialog" aria-modal="true" aria-label="服务设置">
        <div class="modal__head">
          <h2>服务地址</h2>
          <button class="btn btn--ghost btn--icon" type="button" aria-label="关闭" @click="showSettings = false">
            <AppIcon name="close" :size="14" />
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

    <!-- 快捷键面板 -->
    <div v-if="showShortcuts" class="modal" @click.self="showShortcuts = false">
      <div class="modal__panel modal__panel--sm" role="dialog" aria-modal="true" aria-label="快捷键">
        <div class="modal__head">
          <h2>快捷键</h2>
          <button class="btn btn--ghost btn--icon" type="button" aria-label="关闭" @click="showShortcuts = false">
            <AppIcon name="close" :size="14" />
          </button>
        </div>
        <ul class="shortcuts">
          <li><span>新建构建</span><kbd class="kbd">N</kbd></li>
          <li><span>刷新</span><kbd class="kbd">R</kbd></li>
          <li><span>构建列表</span><kbd class="kbd">/</kbd></li>
          <li><span>总览</span><span><kbd class="kbd">G</kbd> <kbd class="kbd">D</kbd></span></li>
          <li><span>构建</span><span><kbd class="kbd">G</kbd> <kbd class="kbd">B</kbd></span></li>
          <li><span>工具链</span><span><kbd class="kbd">G</kbd> <kbd class="kbd">T</kbd></span></li>
          <li><span>关闭浮层</span><kbd class="kbd">Esc</kbd></li>
          <li><span>本面板</span><kbd class="kbd">?</kbd></li>
        </ul>
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
.dot.is-online { background: var(--ok); box-shadow: 0 0 0 3px var(--ok-soft); }
.dot.is-offline { background: var(--bad); box-shadow: 0 0 0 3px var(--bad-soft); }
.dot.is-unknown { background: var(--warn); box-shadow: 0 0 0 3px var(--warn-soft); }

.modal {
  position: fixed;
  inset: 0;
  z-index: 130;
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

.modal__panel--sm { width: min(380px, 100%); }

.modal__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--sp-4) var(--sp-5);
  border-bottom: 1px solid var(--border);
}

.modal__head h2 { font-size: var(--text-md); }
.modal__body { padding: var(--sp-5); }

.modal__foot {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-4) var(--sp-5);
  border-top: 1px solid var(--border);
  background: var(--surface-2);
  border-radius: 0 0 var(--radius-lg) var(--radius-lg);
}

.shortcuts {
  display: flex;
  flex-direction: column;
  padding: var(--sp-3) 0;
}

.shortcuts li {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--sp-2) var(--sp-5);
  font-size: var(--text-base);
  color: var(--text-2);
}

@keyframes fade { from { opacity: 0; } }
@keyframes rise { from { opacity: 0; transform: translateY(10px) scale(0.985); } }
</style>
