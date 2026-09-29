# Hotpot Console

Hotpot 构建服务的 Web 控制台。TypeScript + Vite + Vue 3 单页应用，
直接对接 `hotpot-server` 的 HTTP API，不引入任何后端胶水层。

## 功能

| 页面 | 内容 |
|------|------|
| **总览** | 构建总数 / 成功率 / 队列深度 / 缓存命中率，状态分布，最近构建，运行时信息 |
| **构建** | 状态筛选、分页、就地取消；列表在有在途构建时自动轮询 |
| **提交构建** | 源码路径 + 档位（模式 / 工具链 / features / target / cargo 参数），带请求体实时预览 |
| **构建详情** | 五段耗时分解、**SSE 实时日志**、产物清单与下载 |
| **工具链** | 宿主 rustup 与容器镜像的合并清单、缓存占用 |

## 快速开始

```bash
# 1. 起一个 hotpot-server（另开终端）
cargo run -p hotpot-api --bin hotpot-server

# 2. 起前端
cd web
npm install
npm run dev          # http://localhost:5173
```

前端默认请求 `/api/*`，由 Vite 开发代理转发到 `http://127.0.0.1:7878`。
代理目标可用环境变量覆盖：

```bash
HOTPOT_SERVER=http://builds.internal:7878 npm run dev
```

复制 `.env.example` 为 `.env.local` 可以固化这些配置。

### 两种部署形态（基址自动区分）

| 形态 | 做法 | API 基址 |
|------|------|----------|
| **开发** | `npm run dev`（Vite） | `/api`，由开发代理转发到服务端 |
| **单进程** | `npm run build` + `hotpot-server --web-dir web/dist` | 同源（空基址），直接打 `/v1/...` |
| **分离部署** | 静态托管 `dist/` + 独立服务端 | 填 `VITE_HOTPOT_API`，或界面左下角改 |

基址由 `import.meta.env.DEV` 区分默认值，**不要写死 `/api`**：
同源部署下所有请求都会打到 `/api/v1/...` 而 404，开发模式一切正常、
一部署就"服务离线"。

### 单进程部署（推荐）

`hotpot-server` 可以直接托管前端产物，一个进程就是完整交付物 ——
API、日志流、界面同源，既免掉跨源配置，也省掉一层反向代理：

```bash
cd web && npm run build
cargo build --release -p hotpot-api --bin hotpot-server

./target/release/hotpot-server \
  --web-dir web/dist \
  --cors-origins 'https://console.internal' \
  --max-concurrent-requests 256 \
  --request-timeout-secs 30
```

前端路由（`/builds`、`/builds/:id` …）由 SPA 回退返回 `index.html`，
且**状态码是 200**（`ServeDir` 的 `not_found_service` 会保留 404，
这里自己接管了分支）。`/assets/*` 带内容哈希，用
`Cache-Control: immutable` 永久缓存；`index.html` 则是 `no-cache`，
否则部署后会被缓存住、一直指向已删除的旧资源，表现为白屏。

### 为什么开发需要代理

上游 `hotpot-server` 默认**没有** CORS 中间件，浏览器直接跨源调用会被拦。
开发时用同源代理转发；真要跨源部署，给服务端加
`--cors-origins https://your-console`（精确白名单）即可，
不要图省事用 `*`——构建 API 本身没有鉴权。

## 命令

```bash
npm run dev       # 开发服务器（含 /api 代理）
npm run build     # vue-tsc 类型检查 + 生产构建
npm run preview   # 预览生产产物
```

> `package-lock.json` 里的 `resolved` 指向**公共 registry**（`registry.npmjs.org`）。
> 本机若配了内网镜像源，`npm install` 会把内网地址写回 lockfile，导致 GitHub
> runner 装不上（`getaddrinfo ENOTFOUND`）。需要重新生成时显式指定：
>
> ```bash
> npm install --registry=https://registry.npmjs.org/
> ```

## 结构

```
src/
├── api/
│   ├── types.ts     # 与 Rust serde 类型一一对应的接口定义（保持 snake_case）
│   └── client.ts    # HTTP 客户端 + SSE 日志流 + Prometheus 指标解析
├── stores/          # Pinia：服务连接状态、构建列表
├── utils/format.ts  # 时长/字节/时间/工具链 spec 等展示层格式化
├── styles/main.css  # 设计系统：CSS 变量驱动的深/浅双主题
├── components/      # AppIcon / StatusBadge / StatCard / LogViewer / EmptyState
└── views/           # 四个页面
```

## 实现要点

**SSE 日志流与断线续传。** 日志走 `GET /v1/builds/{id}/logs/stream`。
客户端关掉了 `EventSource` 的自带重连（它重连时不带 `since`，会从头重放），
改为自行控制：记录已消费到的最大 `seq`，重连时作为 `since` 传回去，
服务端只补发更新的事件。重连采用指数退避（1s → 8s 封顶）。

**终态即停。** 构建进入终态后 SSE 会收到 `end` 事件，此时关闭流并停止轮询
记录 —— 终态不会再变，继续请求只是浪费。

**指标解析。** `/metrics` 是 Prometheus 文本格式，这里写了一个够用的小解析器。
两个容易踩的坑：

- 标签值里可能含 `{` / `}` —— `executor` 是 Rust 的 `Debug` 输出，形如
  `Docker { image: "rust:slim-bookworm", docker_host: None }`。
  用 `indexOf('}')` 找标签块结尾会截断，必须跳过引号内的花括号。
- 同一指标会按标签拆成多条。`hotpot_build_duration_ms_avg` 是
  `phase=queue|build|total` 三条，取"第一条"会拿到排队耗时；
  这里显式挑 `phase=total`。

**工具链 spec 推导。** 从 `rust:1.98.0-slim-bookworm` 里取工具链时，
`-slim-bookworm` 是镜像**变体**而非版本段。直接当 spec 提交会被服务端
`parse_toolchain` 以 400 拒掉。`imageToolchainSpec` 与服务端
`resolve_rust_image` 的推导保持一致：按首个 `-` 切开，只有前半段能解析成
工具链时才认；`rust:slim-bookworm` 这类浮动标签返回"等价 stable"而不产出 spec。

**渲染错误兜底。** `ErrorBoundary` 捕获子树渲染异常并给出可操作的出路
（重试渲染 / 回到总览 / 忽略并继续）。这不是装饰：具名路由一旦解析失败
就会在渲染期抛错，没有边界就是整页白屏，用户既不知道发生了什么也没法继续。

**日志行数上限。** 长构建的日志能到几万行，全量留在 DOM 里会让滚动卡死。
`LogViewer` 只保留最近 20 000 行；用户手动往上翻时自动暂停跟随，
避免日志把滚动条"抢"走。

## 已接入的 API

| 方法 | 路径 | 用途 |
|------|------|------|
| `GET` | `/healthz` | 顶栏在线状态（6s 心跳；标签页不可见时停摆） |
| `GET` | `/v1/builds` | 构建列表（一次拉全量，状态筛选在客户端做） |
| `GET` | `/v1/builds/{id}` | 构建详情 + 耗时回填 |
| `POST` | `/v1/builds` | 提交构建 |
| `POST` | `/v1/builds/{id}/cancel` | 取消构建 |
| `GET` | `/v1/builds/{id}/logs/stream` | SSE 实时日志 |
| `GET` | `/v1/builds/{id}/artifacts` | 产物清单 |
| `GET` | `/v1/artifacts/{digest}` | 产物下载 |
| `GET` | `/v1/toolchains` | 工具链发现 |
| `GET` | `/metrics` | 仪表盘指标 |

## 尚未覆盖

- **git / upload 源码来源。** 服务端 `SourceSpec` 已建模，提交表单目前只开放
  `local`（`git` 需服务端 `--allow-git-source`，`upload` 服务端尚未实现）。
- **鉴权。** `HOTPOT_CACHE_TOKEN` 只保护缓存协议端点，构建 API 本身无鉴权。
  公网暴露需要在反向代理层自行加认证。
