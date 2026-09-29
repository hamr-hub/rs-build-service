<script setup lang="ts">
/**
 * 全局构建器抽屉。
 *
 * 刻意做成"浮层"而不是新页面：提交构建和浏览构建历史是交替发生的，浮层让
 * 用户在填参数时仍然看得见列表（改完直接关掉，配置草稿还在）。
 *
 * 无障碍：打开时把焦点移进抽屉并锁住 Tab 在抽屉内循环，关闭后焦点归还给
 * 触发它的按钮。浮层遮住下面的内容，不能只靠视觉暗示。
 */
import { computed, nextTick, onUnmounted, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import AppIcon from './AppIcon.vue'
import { useBuilderStore } from '../stores/builder'
import { useToastStore } from '../stores/toast'

const builder = useBuilderStore()
const toast = useToastStore()
const router = useRouter()

const panel = ref<HTMLElement | null>(null)
const pathInput = ref<HTMLInputElement | null>(null)
const templateName = ref('')
const showTemplates = ref(false)
let lastFocused: HTMLElement | null = null

const draft = computed(() => builder.draft)

/** 提交成功后关抽屉并跳到详情页——用户接下来要做的就是看日志。 */
async function onSubmit(): Promise<void> {
  const id = await builder.submit()
  if (id) {
    builder.close()
    await router.push({ name: 'build-detail', params: { id } })
  }
}

function saveTemplate(): void {
  if (builder.saveTemplate(templateName.value)) {
    templateName.value = ''
    showTemplates.value = true
  }
}

function copyId(): void {
  const id = builder.prefillLabel
  if (!id) return
  void navigator.clipboard?.writeText(id)
  toast.info('已复制', id)
}

// ---------- 焦点管理 ----------

watch(
  () => builder.open,
  async (isOpen) => {
    if (isOpen) {
      lastFocused = document.activeElement as HTMLElement | null
      await nextTick()
      pathInput.value?.focus()
    } else {
      // 焦点归还。合成事件打开抽屉时 lastFocused 可能没有意义，
      // 这时退回到"提交构建"按钮，而不是丢给 body（键盘用户会失去位置）。
      const target =
        lastFocused && lastFocused !== document.body && document.contains(lastFocused)
          ? lastFocused
          : document.querySelector<HTMLElement>('[data-drawer-trigger]')
      target?.focus()
      lastFocused = null
      showTemplates.value = false
    }
  },
)

function onKeydown(event: KeyboardEvent): void {
  if (!builder.open) return
  if (event.key === 'Escape') {
    event.preventDefault()
    builder.close()
    return
  }
  // 焦点陷阱：Tab 不应跑到被遮住的页面上去。
  if (event.key !== 'Tab' || !panel.value) return
  const focusables = panel.value.querySelectorAll<HTMLElement>(
    'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])',
  )
  if (focusables.length === 0) return
  const first = focusables[0]!
  const last = focusables[focusables.length - 1]!
  const active = document.activeElement
  if (event.shiftKey && active === first) {
    event.preventDefault()
    last.focus()
  } else if (!event.shiftKey && active === last) {
    event.preventDefault()
    first.focus()
  }
}

watch(
  () => builder.open,
  (isOpen) => {
    if (isOpen) document.addEventListener('keydown', onKeydown, true)
    else document.removeEventListener('keydown', onKeydown, true)
  },
  { immediate: true },
)

onUnmounted(() => document.removeEventListener('keydown', onKeydown, true))
</script>

<template>
  <Teleport to="body">
    <Transition name="drawer">
      <div v-if="builder.open" class="drawer-root" @click.self="builder.close()">
        <aside
          ref="panel"
          class="drawer"
          role="dialog"
          aria-modal="true"
          aria-labelledby="drawer-title"
        >
          <header class="drawer__head">
            <div>
              <h2 id="drawer-title" class="drawer__title">提交构建</h2>
              <p class="drawer__sub">
                <template v-if="builder.prefillLabel">
                  复用配置
                  <button class="linkish mono" type="button" @click="copyId">
                    {{ builder.prefillLabel.slice(0, 18) }}
                  </button>
                </template>
                <template v-else>本地 Cargo 项目 · 绝对路径</template>
              </p>
            </div>
            <button class="btn btn--ghost btn--icon" type="button" aria-label="关闭" @click="builder.close()">
              <AppIcon name="close" />
            </button>
          </header>

          <div class="drawer__body">
            <div v-if="builder.error" class="alert alert--bad">
              <AppIcon name="alert" />
              <div>
                <strong>服务端拒绝了这次提交</strong>
                <p>{{ builder.error }}</p>
              </div>
            </div>

            <!-- 模板 -->
            <section v-if="builder.templates.length || showTemplates" class="block">
              <div class="block__head">
                <h3 class="block__title">配置模板</h3>
                <button class="btn btn--ghost btn--sm" type="button" @click="showTemplates = !showTemplates">
                  {{ showTemplates ? '收起' : `全部 (${builder.templates.length})` }}
                </button>
              </div>
              <ul v-if="showTemplates" class="tpl-list">
                <li v-for="tpl in builder.templates" :key="tpl.id" class="tpl">
                  <button class="tpl__apply" type="button" @click="builder.applyTemplate(tpl)">
                    <span class="tpl__name">{{ tpl.name }}</span>
                    <span class="tpl__meta">
                      {{ tpl.profile.mode }}<template v-if="tpl.profile.toolchain"> · {{ tpl.profile.toolchain }}</template>
                      <template v-if="tpl.profile.features.length"> · {{ tpl.profile.features.join(',') }}</template>
                    </span>
                  </button>
                  <button
                    class="tpl__del"
                    type="button"
                    :aria-label="`删除模板 ${tpl.name}`"
                    @click="builder.removeTemplate(tpl.id)"
                  >
                    <AppIcon name="close" :size="13" />
                  </button>
                </li>
                <li v-if="!builder.templates.length" class="tpl tpl--empty">还没有模板</li>
              </ul>
            </section>

            <!-- 源码 -->
            <section class="block">
              <div class="block__head"><h3 class="block__title">源码</h3></div>
              <div class="field">
                <label class="field__label" for="builder-path">项目根目录</label>
                <input
                  id="builder-path"
                  ref="pathInput"
                  v-model="draft.path"
                  class="input input--mono"
                  placeholder="/Users/you/code/my-app"
                  autocomplete="off"
                  spellcheck="false"
                  @keyup.enter="onSubmit"
                />
                <p class="field__hint">必须存在且含 <code class="code">Cargo.toml</code>，且在服务端允许的项目根内。</p>
              </div>
            </section>

            <!-- 档位 -->
            <section class="block">
              <div class="block__head"><h3 class="block__title">构建档位</h3></div>

              <div class="field">
                <span class="field__label">编译模式</span>
                <div class="segmented" role="group" aria-label="编译模式">
                  <button
                    type="button"
                    :class="{ 'is-active': draft.mode === 'debug' }"
                    :aria-pressed="draft.mode === 'debug'"
                    @click="draft.mode = 'debug'"
                  >
                    debug
                  </button>
                  <button
                    type="button"
                    :class="{ 'is-active': draft.mode === 'release' }"
                    :aria-pressed="draft.mode === 'release'"
                    @click="draft.mode = 'release'"
                  >
                    release
                  </button>
                </div>
              </div>

              <div class="form-grid">
                <div class="field">
                  <label class="field__label" for="builder-toolchain">工具链</label>
                  <select id="builder-toolchain" v-model="draft.toolchain" class="select">
                    <option value="">
                      {{ builder.toolchainsLoading ? '正在发现…' : '使用 worker 默认' }}
                    </option>
                    <option v-for="opt in builder.toolchainOptions" :key="opt" :value="opt">{{ opt }}</option>
                    <option v-if="draft.toolchain && !builder.toolchainOptions.includes(draft.toolchain)" :value="draft.toolchain">
                      {{ draft.toolchain }}（未在清单中）
                    </option>
                  </select>
                  <p v-if="builder.toolchainInvalid" class="field__error">
                    格式不被服务端接受：只支持 stable / beta / nightly / nightly-日期 / 1.98 / 1.98.0
                  </p>
                  <p v-else-if="builder.toolchainsError" class="field__error">
                    工具链发现失败：{{ builder.toolchainsError }}
                  </p>
                </div>

                <div class="field">
                  <label class="field__label" for="builder-target">目标三元组</label>
                  <input
                    id="builder-target"
                    v-model="draft.target"
                    class="input input--mono"
                    placeholder="宿主默认"
                    autocomplete="off"
                    spellcheck="false"
                  />
                </div>
              </div>

              <div class="form-grid">
                <div class="field">
                  <label class="field__label" for="builder-features">Features</label>
                  <input
                    id="builder-features"
                    v-model="draft.features"
                    class="input input--mono"
                    placeholder="jwt, metrics"
                    autocomplete="off"
                    spellcheck="false"
                  />
                </div>
                <div class="field">
                  <label class="field__label" for="builder-flags">Cargo 参数</label>
                  <input
                    id="builder-flags"
                    v-model="draft.cargoFlags"
                    class="input input--mono"
                    placeholder="--offline --locked"
                    autocomplete="off"
                    spellcheck="false"
                  />
                </div>
              </div>

              <label class="switch">
                <input v-model="draft.noDefaultFeatures" type="checkbox" />
                <span class="switch__track" />
                <span>关闭默认 features</span>
              </label>
            </section>

            <!-- 存模板 -->
            <section class="block">
              <div class="block__head"><h3 class="block__title">存为模板</h3></div>
              <div class="row">
                <input
                  v-model="templateName"
                  class="input"
                  placeholder="如：CI release 常用"
                  @keyup.enter="saveTemplate"
                />
                <button class="btn" type="button" :disabled="!templateName.trim()" @click="saveTemplate">
                  保存
                </button>
              </div>
            </section>

            <!-- 预览 -->
            <details class="block">
              <summary class="block__head block__head--summary">
                <span class="block__title">请求预览</span>
                <code class="code">POST /v1/builds</code>
              </summary>
              <pre class="preview mono">{{ builder.payloadPreview }}</pre>
            </details>
          </div>

          <footer class="drawer__foot">
            <button class="btn btn--ghost" type="button" @click="builder.reset()">清空</button>
            <div class="spacer" />
            <button class="btn" type="button" @click="builder.close()">取消</button>
            <button class="btn btn--primary" type="button" :disabled="!builder.canSubmit" @click="onSubmit">
              <AppIcon :name="builder.submitting ? 'clock' : 'play'" />
              {{ builder.submitting ? '提交中…' : '开始构建' }}
            </button>
          </footer>
        </aside>
      </div>
    </Transition>
  </Teleport>
</template>

<style scoped>
.drawer-root {
  position: fixed;
  inset: 0;
  z-index: 120;
  display: flex;
  justify-content: flex-end;
  background: var(--overlay);
  backdrop-filter: blur(3px);
}

.drawer {
  display: flex;
  flex-direction: column;
  width: min(520px, 100vw);
  height: 100%;
  border-left: 1px solid var(--border);
  background: var(--surface);
  box-shadow: var(--shadow-lg);
}

.drawer__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--sp-3);
  padding: var(--sp-4) var(--sp-5);
  border-bottom: 1px solid var(--border);
  flex: none;
}

.drawer__title {
  font-size: var(--text-md);
}

.drawer__sub {
  margin-top: 2px;
  font-size: var(--text-sm);
  color: var(--text-3);
}

.drawer__body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: var(--sp-5);
  display: flex;
  flex-direction: column;
  gap: var(--sp-5);
}

.drawer__foot {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-4) var(--sp-5);
  border-top: 1px solid var(--border);
  background: var(--surface-2);
  flex: none;
}

.block {
  display: flex;
  flex-direction: column;
  gap: var(--sp-3);
}

.block__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-3);
}

.block__head--summary {
  cursor: pointer;
  list-style: none;
}

.block__head--summary::-webkit-details-marker {
  display: none;
}

.block__title {
  font-size: var(--text-sm);
  font-weight: 600;
  letter-spacing: 0.06em;
  text-transform: uppercase;
  color: var(--text-3);
}

.tpl-list {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
}

.tpl {
  display: flex;
  align-items: stretch;
  gap: 2px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--surface-2);
  overflow: hidden;
}

.tpl--empty {
  padding: var(--sp-3);
  font-size: var(--text-sm);
  color: var(--text-3);
  justify-content: center;
}

.tpl__apply {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 1px;
  padding: var(--sp-2) var(--sp-3);
  border: 0;
  background: transparent;
  text-align: left;
  cursor: pointer;
  min-width: 0;
}

.tpl__apply:hover {
  background: var(--surface-3);
}

.tpl__name {
  font-size: var(--text-base);
  font-weight: 600;
}

.tpl__meta {
  font-size: var(--text-xs);
  color: var(--text-3);
  font-family: var(--font-mono);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.tpl__del {
  flex: none;
  width: 30px;
  border: 0;
  border-left: 1px solid var(--border);
  background: transparent;
  color: var(--text-3);
  cursor: pointer;
}

.tpl__del:hover {
  background: var(--bad-soft);
  color: var(--bad);
}

.linkish {
  border: 0;
  background: none;
  padding: 0;
  color: var(--accent);
  cursor: pointer;
  font-size: var(--text-xs);
}

.preview {
  margin: 0;
  padding: var(--sp-3) var(--sp-4);
  max-height: 260px;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--log-bg);
  font-size: var(--text-xs);
  line-height: 1.6;
  color: var(--text-2);
}

/* 从右侧滑入 */
.drawer-enter-active,
.drawer-leave-active {
  transition: opacity 220ms var(--ease);
}
.drawer-enter-active .drawer,
.drawer-leave-active .drawer {
  transition: transform 260ms var(--ease);
}
.drawer-enter-from,
.drawer-leave-to {
  opacity: 0;
}
.drawer-enter-from .drawer,
.drawer-leave-to .drawer {
  transform: translateX(24px);
}
</style>
