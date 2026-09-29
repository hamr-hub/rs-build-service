<script setup lang="ts">
/** 构建列表：按状态筛选、翻页、就地取消。 */
import { computed, ref } from 'vue'
import { RouterLink } from 'vue-router'
import AppIcon from '../components/AppIcon.vue'
import EmptyState from '../components/EmptyState.vue'
import StatusBadge from '../components/StatusBadge.vue'
import { cancelBuild } from '../api/client'
import type { BuildRecord, BuildStatus } from '../api/types'
import { useBuildsStore } from '../stores/builds'
import { useServerStore } from '../stores/server'
import { formatDuration, formatRelative, formatTime, shortId, shortenPath } from '../utils/format'
const builds = useBuildsStore()
const server = useServerStore()

const FILTERS: { key: BuildStatus | 'all'; label: string }[] = [
  { key: 'all', label: '全部' },
  { key: 'queued', label: '排队中' },
  { key: 'dispatched', label: '执行中' },
  { key: 'succeeded', label: '成功' },
  { key: 'failed', label: '失败' },
  { key: 'timeout', label: '超时' },
  { key: 'canceled', label: '已取消' },
]

/** 各状态在当前全量结果里的计数，用于筛选芯片上的数字。 */
const tally = computed(() => {
  const out: Record<string, number> = {}
  for (const build of builds.builds) {
    out[build.status] = (out[build.status] ?? 0) + 1
  }
  return out
})

const cancelling = ref<string | null>(null)
const actionError = ref<string | null>(null)

async function onCancel(build: BuildRecord): Promise<void> {
  if (cancelling.value) return
  cancelling.value = build.id
  actionError.value = null
  try {
    builds.patch(await cancelBuild(build.id))
  } catch (error) {
    actionError.value = error instanceof Error ? error.message : String(error)
  } finally {
    cancelling.value = null
  }
}

function sourceLabel(build: BuildRecord): string {
  if (build.source.kind === 'local') return shortenPath(build.source.path, 2)
  if (build.source.kind === 'git') return build.source.url
  return build.source.upload_id
}

function modeLabel(build: BuildRecord): string {
  return build.profile.mode === 'release' ? 'release' : 'debug'
}
</script>

<template>
  <div class="stack">
    <div v-if="actionError" class="alert alert--bad">
      <AppIcon name="alert" />
      {{ actionError }}
    </div>

    <div v-if="server.state === 'offline'" class="alert alert--bad">
      <AppIcon name="alert" />
      <span>
        无法连接 <code class="code">{{ server.base }}</code> —— 确认
        <code class="code">hotpot-server</code> 已启动，或点击左下角切换服务地址。
      </span>
    </div>

    <!-- 筛选 -->
    <div class="row row--wrap">
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
          <span class="chip__count">{{ filter.key === 'all' ? builds.builds.length : (tally[filter.key] ?? 0) }}</span>
        </button>
      </div>
      <div class="spacer" />
      <RouterLink class="btn btn--primary" :to="{ name: 'build-new' }">
        <AppIcon name="plus" />
        提交构建
      </RouterLink>
    </div>

    <!-- 列表 -->
    <section class="card">
      <div class="card__body card__body--flush">
        <div v-if="builds.loading && !builds.builds.length" class="table-wrap">
          <div v-for="i in 6" :key="i" class="skeleton-row">
            <span class="skeleton" style="height: 12px; width: 120px" />
            <span class="skeleton" style="height: 12px; width: 180px" />
            <span class="skeleton" style="height: 12px; width: 70px" />
            <span class="skeleton" style="height: 12px; width: 90px" />
          </div>
        </div>

        <EmptyState
          v-else-if="builds.error"
          icon="alert"
          title="加载失败"
          :description="builds.error"
        >
          <button class="btn" type="button" @click="builds.load()">
            <AppIcon name="refresh" />
            重试
          </button>
        </EmptyState>

        <EmptyState
          v-else-if="!builds.visible.length"
          icon="stack"
          title="没有符合条件的构建"
          :description="
            builds.status === 'all'
              ? '队列还是空的。提交一次构建，就能在这里看到实时状态。'
              : '换个状态筛选看看，或者清除筛选。'
          "
        >
          <RouterLink v-if="builds.status === 'all'" class="btn btn--primary" :to="{ name: 'build-new' }">
            <AppIcon name="plus" />
            提交构建
          </RouterLink>
          <button v-else class="btn" type="button" @click="builds.setStatus('all')">查看全部</button>
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
                <th aria-label="操作" />
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="build in builds.visible"
                :key="build.id"
                class="is-clickable"
                @click="$router.push({ name: 'build-detail', params: { id: build.id } })"
              >
                <td class="mono truncate" :title="build.id">{{ shortId(build.id, 16) }}</td>
                <td class="truncate" :title="sourceLabel(build)">{{ sourceLabel(build) }}</td>
                <td>
                  <span class="code">{{ modeLabel(build) }}</span>
                  <span v-if="build.profile.toolchain" class="code" style="margin-left: 4px">
                    {{ build.profile.toolchain }}
                  </span>
                </td>
                <td><StatusBadge :status="build.status" /></td>
                <td class="num mono">{{ formatDuration(build.timings.total_ms || null) }}</td>
                <td class="dim" :title="formatTime(build.created_at_ms)">
                  {{ formatRelative(build.created_at_ms) }}
                </td>
                <td class="actions" @click.stop>
                  <button
                    v-if="!['succeeded', 'failed', 'canceled', 'timeout'].includes(build.status)"
                    class="btn btn--sm btn--danger"
                    type="button"
                    :disabled="cancelling === build.id"
                    @click="onCancel(build)"
                  >
                    {{ cancelling === build.id ? '取消中' : '取消' }}
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-if="builds.visible.length" class="pagination">
          <span>共 {{ builds.filtered.length }} 条 · 第 {{ builds.page }} / {{ builds.pageCount }} 页</span>
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
.chips {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-2);
}

.skeleton-row {
  display: grid;
  grid-template-columns: 140px 1fr 90px 110px;
  align-items: center;
  gap: var(--sp-4);
  padding: var(--sp-4) var(--sp-5);
  border-bottom: 1px solid var(--border);
}
</style>
