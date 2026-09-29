import { createRouter, createWebHistory, type RouteRecordRaw } from 'vue-router'

const routes: RouteRecordRaw[] = [
  {
    path: '/',
    name: 'dashboard',
    component: () => import('../views/DashboardView.vue'),
    meta: { title: '总览', subtitle: '构建队列、缓存与运行时状态', icon: 'grid' },
  },
  {
    path: '/builds',
    name: 'builds',
    component: () => import('../views/BuildsView.vue'),
    meta: { title: '构建', subtitle: '提交、跟踪与回溯所有构建', icon: 'stack' },
  },
  {
    // 构建器已改为全局抽屉；这个旧链接保留重定向，避免书签/历史失效。
    path: '/builds/new',
    redirect: { name: 'builds', hash: '#new' },
  },
  {
    path: '/builds/:id',
    name: 'build-detail',
    component: () => import('../views/BuildDetailView.vue'),
    meta: { title: '构建详情', subtitle: '实时日志、耗时分解与产物', hideInNav: true },
    props: true,
  },
  {
    path: '/toolchains',
    name: 'toolchains',
    component: () => import('../views/ToolchainsView.vue'),
    meta: { title: '工具链', subtitle: '宿主与容器里可用的 Rust 工具链', icon: 'cube' },
  },
  { path: '/:pathMatch(.*)*', redirect: '/' },
]

export const router = createRouter({
  history: createWebHistory(),
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

router.afterEach((to) => {
  const title = to.meta.title as string | undefined
  document.title = title ? `${title} · Hotpot` : 'Hotpot'
})

/** 侧边导航用：剔除 `hideInNav` 的详情页。 */
export const navRoutes = routes.filter((r) => r.meta?.title && !r.meta.hideInNav)
