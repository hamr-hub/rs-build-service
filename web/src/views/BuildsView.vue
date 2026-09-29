<script setup lang="ts">
/**
 * 构建列表：搜索 + 状态筛选 + 分页 + 行内操作（取消 / 以此配置重建）。
 *
 * "以相同配置重建"直接调起全局抽屉并预填该构建的档位——构建器与页面在这里
 * 真正合为一体：不用记下路径和参数再手动重填一遍。
 */
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import EmptyState from '../components/EmptyState.vue'
import StatusBadge from '../components/StatusBadge.vue'
import { useBuildsStore, type StatusFilter } from '../stores/builds'
import { useHotkeys } from '../composables/useHotkeys'
import { useBuilderStore } from '../stores/builder'
import { useServerStore } from '../stores/server'
import { isTerminal, type BuildRecord } from '../api/types'
import { formatDuration, formatRelative, formatTime, shortId, shortenPath } from '../utils/format'

const builds = useBuildsStore()
const builder = useBuilderStore()
const server = useServerStore()
const route = useRoute()
const router = useRouter()

const searchInput = ref<HTMLInputElement | null>(null)

const FILTERS: { key: StatusFilter; label: string }[] = [
  { key: 'all', label: '全部' },
  { key: 'queued', label: '排队中' },
  { key: 'dispatched', label: '执行中' },
  { key: 'succeeded', label: '成功' },
  { key: 'failed', label: '失败' },
  { key: 'timeout', label: '超时' },
  { key: 'canceled', label: '已取消' },
]

const search = computed({
  get: () => builds.query,
  set: (value: string) => builds.setQuery(value),
})

const isFiltering = computed(
  () => builds.status !== 'all' || builds.query.trim().length > 0,
)

/** 在途构建的实时刻度，让"正在跑"这件事一眼可见。 */
const activeCount = computed(() => builds.filtered.filter((b) => !isTerminal(b.status)).length)

function countFor(key: StatusFilter): number {
  return key === 'all' ? builds.builds.length : (builds.statusTally[key] ?? 0)
}

function sourceLabel(build: BuildRecord): string {
  if (build.source.kind === 'local') return shortenPath(build.source.path, 2)
  if (build.source.kind === 'git') return build.source.url
  return build.source.upload_id
}

function modeLabel(build: BuildRecord): string {
  return build.profile.mode === 'release' ? 'release' : 'debug'
}

/** 复用该构建的档位重新提交一次——最常见的重复动作。 */
function rebuild(build: BuildRecord): void {
  builder.openBuilder({
    path: build.source.kind === 'local' ? build.source.path : '',
    profile: build.profile,
    label: build.id,
  })
}

function clearFilters(): void {
  builds.setStatus('all')
  builds.setQuery('')
}

// 只在本页生效：App 层的 `/` 是"去构建列表"，到了列表页它就该变成"聚焦搜索"。
useHotkeys([
  {
    combo: '/',
    handler: () => searchInput.value?.focus(),
    allowInInput: true,
    description: '聚焦搜索',
  },
])

onMounted(() => {
  void builds.load()
  // 从"新建构建"旧链接跳过来时带 #new，直接把抽屉打开。
  if (route.hash === '#new') {
    builder.openBuilder()
    void router.replace({ name: 'builds' })
  }
})
</script>

<template>
  <div class="stack">
    <div v-if="server.state === 'offline'" class="alert alert--bad">
      <AppIcon name="alert" />
      <span>
        无法连接 <code class="code">{{ server.base }}</code> —— 确认
        <code class="code">hotpot-server</code> 已启动，或点击左下角切换服务地址。
      </span>
    </div>

    <!-- 工具栏 -->
    <div class="row row--wrap toolbar">
      <div class="search">
        <AppIcon name="search" :size="15" class="search__icon" />
        <input
          ref="searchInput"
          v-model="search"
          type="search"
          class="input"
          placeholder="搜索 ID、路径、URL、档位…"
          aria-label="搜索构建"
        />
        <button
          v-if="search"
          class="search__clear"
          type="button"
          aria-label="清除搜索"
          @click="search = ''"
        >
          <AppIcon name="close" :size="13" />
        </button>
      </div>

      <div class="chips">
        <button
          v-for="filter in FILTERS"
          :key="filter.key"
          class="chip"
          :class="{ 'is-active': builds.status === filter.key }"
          type="button"
          @click="builds.setStatus(filter.key)"
        >
          {{ filter.label }}
          <span class="chip__count">{{ countFor(filter.key) }}</span>
        </button>
      </div>

      <div class="spacer" />

      <button v-if="isFiltering" class="btn btn--ghost btn--sm" type="button" @click="clearFilters">
        清除筛选
      </button>
      <button class="btn btn--primary" type="button" @click="builder.openBuilder()">
        <AppIcon name="plus" />
        提交构建
      </button>
    </div>

    <p v-if="activeCount > 0" class="row active-note">
      <span class="pulse-dot" />
      {{ activeCount }} 个构建进行中，列表会自动刷新
    </p>

    <!-- 列表 -->
    <section class="card">
      <div class="card__body card__body--flush">
        <div v-if="builds.loading && !builds.builds.length" class="stack">
          <div v-for="i in 8" :key="i" class="skeleton-row">
            <span class="skeleton" style="height: 12px; width: 130px" />
            <span class="skeleton" style="height: 12px; width: 200px; flex: 1" />
            <span class="skeleton" style="height: 12px; width: 80px" />
            <span class="skeleton" style="height: 12px; width: 70px" />
            <span class="skeleton" style="height: 12px; width: 60px" />
          </div>
        </div>

        <EmptyState v-else-if="builds.error" icon="alert" title="加载失败" :description="builds.error">
          <button class="btn" type="button" @click="builds.load()">
            <AppIcon name="refresh" />
            重试
          </button>
        </EmptyState>

        <EmptyState
          v-else-if="!builds.visible.length"
          icon="stack"
          :title="isFiltering ? '没有匹配的构建' : '队列还是空的'"
          :description="
            isFiltering
              ? '换个关键词或状态筛选看看。'
              : '提交一次构建，就能在这里看到实时状态。'
          "
        >
          <button v-if="isFiltering" class="btn" type="button" @click="clearFilters">清除筛选</button>
          <button v-else class="btn btn--primary" type="button" @click="builder.openBuilder()">
            <AppIcon name="plus" />
            提交构建
          </button>
        </EmptyState>

        <div v-else class="table-wrap">
          <table class="table">
            <thead>
              <tr>
                <th>构建 ID</th>
                <th>项目</th>
                <th>档位</th>
                <th>状态</th>
                <th class="num">耗时</th>
                <th>提交时间</th>
                <th class="right">操作</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="build in builds.visible"
                :key="build.id"
                class="is-clickable"
                tabindex="0"
                @click="$router.push({ name: 'build-detail', params: { id: build.id } })"
                @keydown.enter="$router.push({ name: 'build-detail', params: { id: build.id } })"
              >
                <td class="mono truncate" :title="build.id">{{ shortId(build.id, 16) }}</td>
                <td class="truncate" :title="sourceLabel(build)">{{ sourceLabel(build) }}</td>
                <td>
                  <span class="code">{{ modeLabel(build) }}</span>
                  <span v-if="build.profile.toolchain" class="code tag-gap">{{ build.profile.toolchain }}</span>
                </td>
                <td><StatusBadge :status="build.status" /></td>
                <td class="num mono">{{ formatDuration(build.timings.total_ms || null) }}</td>
                <td class="dim" :title="formatTime(build.created_at_ms)">
                  {{ formatRelative(build.created_at_ms) }}
                </td>
                <td class="actions" @click.stop @keydown.stop>
                  <div class="row-actions">
                    <button
                      class="btn btn--sm btn--ghost btn--icon"
                      type="button"
                      :title="`以相同配置重新构建 ${build.id.slice(0, 8)}`"
                      @click="rebuild(build)"
                    >
                      <AppIcon name="refresh" :size="14" />
                    </button>
                    <button
                      v-if="!['succeeded', 'failed', 'canceled', 'timeout'].includes(build.status)"
                      class="btn btn--sm btn--danger"
                      type="button"
                      :disabled="builds.cancelling.has(build.id)"
                      @click="builds.cancel(build.id)"
                    >
                      {{ builds.cancelling.has(build.id) ? '…' : '取消' }}
                    </button>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-if="builds.visible.length" class="pagination">
          <span>
            共 {{ builds.filtered.length }} 条 · 第 {{ builds.page }} / {{ builds.pageCount }} 页
          </span>
          <div class="row">
            <button
              class="btn btn--sm"
              type="button"
              :disabled="builds.page <= 1"
              @click="builds.setPage(builds.page - 1)"
            >
              <AppIcon name="arrowLeft" :size="14" />
              上一页
            </button>
            <button
              class="btn btn--sm"
              type="button"
              :disabled="builds.page >= builds.pageCount"
              @click="builds.setPage(builds.page + 1)"
            >
              下一页
              <AppIcon name="arrowRight" :size="14" />
            </button>
          </div>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.toolbar {
  gap: var(--sp-3);
}

.chips {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
}

.tag-gap {
  margin-left: 4px;
}

.right {
  text-align: right;
}

.active-note {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  font-size: var(--text-sm);
  color: var(--text-2);
}

.pulse-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--active);
  animation: pulse-dot 1.8s var(--ease) infinite;
}

@keyframes pulse-dot {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.35; }
}
</style>
