# Hotpot 总体架构设计

> 版本：v0.1（2026-09-28）
> 依据：`docs/research/01..04` 四份调研报告 + 本机实测基线
> 状态：02 报告已完成（协议对齐 REAPI 结论已纳入）；M0–M2 已实现

## 1. 设计原则

1. **内容寻址优先于时间戳**：跨机器/跨会话的一切复用都以输入闭包的内容哈希为
   key；mtime 仅作为单机快速路径。
2. **多级缓存，各司其职**：依赖物料层（CARGO_HOME/git db/sparse 缓存）→
   crate 编译层（sccache 协议）→ 构建产物层（版本化二进制）。
3. **隔离是构建服务的底线**：build.rs / proc-macro 会执行任意代码。MVP 用
   独立会话工作目录 + 子进程；多租户演进到容器 / Firecracker。
4. **单体内模块化**：api / scheduler / worker 在同一 workspace 内以 crate
   划分；单二进制只是部署形态，拆分时只换启动参数。
5. **生产热部署主打 L3 进程级切换**：不追求任意程序的内存级热升级；开发期
   热重载走显式 ABI 约定。
6. **零外部依赖起步**：SQLite（sqlx）+ 本地磁盘即可服务小团队，逐层演进到
   Postgres / S3 / K8s。

## 2. 逻辑架构

```
                    ┌──────────────────────────────┐
  hotpot CLI / CI   │         hotpot-api           │  HTTP/JSON, SSE
 ───────────────────▶│  builds / logs / artifacts   │
                    │  auth (PAT, OIDC later)      │
                    └──────────────┬───────────────┘
                                   │
                    ┌──────────────▼───────────────┐
                    │      hotpot-scheduler        │
                    │  queue · lease · priority ·  │
                    │  quota · worker registry     │
                    └──────────────┬───────────────┘
                                   │ pull (long poll)
                    ┌──────────────▼───────────────┐
                    │       hotpot-worker          │
                    │ source fetch → workspace →   │
                    │ cargo (sccache/mold) → pack  │
                    └───┬───────────────────┬──────┘
                        │                   │
             ┌──────────▼────────┐  ┌───────▼──────────────┐
             │   hotpot-store    │  │ cache protocol layer │
             │ CAS blobs (256    │  │ sccache WebDAV-like  │
             │ buckets, blake3,  │  │ Turborepo v8         │
             │ zstd, LRU, S3)    │  │ (Nx v1 / REAPI 评估) │
             └───────────────────┘  └──────────────────────┘

  热部署（独立链路）:
   dev:  hotpot-dev   notify watcher + hot-lib-reloader + socket keeper
   prod: hotpot-agent release supervisor: fd 交接 + deploy 状态机 + rollback
```

## 3. Cargo workspace 划分

| Crate | 类型 | 职责 |
|-------|------|------|
| `hotpot-core` | lib | 领域模型（Build/Job/Digest/Profile）、错误、配置、状态机、telemetry 初始化 |
| `hotpot-store` | lib | 内容寻址 blob 存储：本地磁盘（256 桶、blake3 校验、临时文件 rename、zstd、LRU/GC），S3 分层（object_store） |
| `hotpot-cacheproto` | lib | 缓存协议适配：sccache 兼容（WebDAV GET/PUT，key 分片）、Turborepo v8 端点 |
| `hotpot-scheduler` | lib | DB 支持的任务队列：lease、优先级、配额、worker 注册/心跳、失活接管、cancel 传播 |
| `hotpot-worker` | lib/bin | 构建执行器：源码获取、物料准备、cargo 子进程/容器执行、事件与日志采集、产物上传 |
| `hotpot-api` | lib | axum HTTP API：构建提交/查询/取消、SSE 日志、产物下载、协议兼容端点、认证中间件 |
| `hotpot-agent` | bin | 生产部署 agent：release supervisor（fd 传递、drain、preflight、commit/rollback） |
| `hotpot-dev` | lib/bin | 开发热重载：notify + hot-lib-reloader 集成 + socket keeper |
| `hotpot-cli` | bin | `hotpot` 统一入口（clap 子命令） |

## 4. 核心数据模型（hotpot-core，摘要）

```text
Build
  id: BuildId(uuid)
  project_id
  source: SourceSpec            # git{url,ref,sha} | upload{upload_id}
  profile: BuildProfile         # toolchain, opt-level, lto, codegen-units, features...
  resource: ResourceSpec        # cpu/mem/timeout, architecture
  cache_scope: CacheScope       # shared/project/private, read-only 选项
  artifacts: Vec<ArtifactGlob>
  status: BuildStatus           # Queued → Dispatched → Running →
                                #   Succeeded/Failed/Canceled/Timeout
  timings: BuildTimings         # 各阶段耗时
  created_at / started_at / finished_at

BuildEvent（日志/状态事件，带单调序号 seq）
  build_id, seq, timestamp, kind(stdout|stderr|phase|status), payload
```

## 5. 缓存层次设计（来自 01/02 调研的核心结论）

### L0 物料层（worker 主机持久化）

- `CARGO_HOME`：registry 包体、git bare db、sparse index 响应；
- 固定构建根路径（如 `/workspace/build`）以稳定路径相关指纹；
- **warm target 卷**：CARGO_TARGET_DIR 按「项目身份 × 镜像 tag × triple ×
  mode」键控复用（绝不跨键共享，静默误编译事故红线）；20GiB LRU 配额，
  30 分钟宽限保护在用目录。sccache 无法缓存 build-script-build / bin /
  dylib 单元，cargo fingerprint 命中是这些单元唯一的免重编手段；
- **共享 git 工作区**：同 URL 单一稳定检出路径（部分克隆 + fetch +
  detached checkout），避免会话路径变化使项目 crate 指纹失效；按 URL 互斥
  保证同项目构建串行；
- slim 镜像自动供给 `build-essential`，deb 归档与 apt 索引持久化复用。

### L1 crate 编译层（sccache 协议，跨主机）

- worker 以 `RUSTC_WRAPPER=sccache`、`CARGO_INCREMENTAL=0` 执行 cargo；
- 缓存对象对服务端不透明（内含 zstd 条目的 ZIP），按 sccache key 存取；
- key 由 sccache 客户端计算，含 rustc 版本、全部参数、源文件内容、features；
- 链接阶段（bin/dylib/proc-macro）不进缓存，由 mold/lld 压缩。

### L2 产物层（hotpot-store CAS）

- 最终二进制 / tarball 以 blake3 内容哈希存储，天然去重、不可变；
- 构建记录引用产物 digest，pin 期间不被 GC；
- 增量发布时客户端按 manifest 只拉缺失产物。

### 缓存正确性红线

- key 必须覆盖输入传递闭包（工具链 digest + flags + features + 源内容 + 镜像哈希）；
- 租户/项目 namespace 隔离；缓存仅允许控制面写，worker 不持有签名密钥；
- 对「读文件系统/依赖时间戳」的 proc-macro、build script 保守不缓存或要求声明。

## 6. 队列与调度（hotpot-scheduler）

- **MVP：SQLite 队列表**：`jobs` 行 + `leased_by/leased_until` 租约；worker
  长轮询认领；worker 失活后租约过期 → 任务重新入队（at-least-once）。
- 派发顺序：优先级（interactive > batch）→ 项目/全局并发配额 → FIFO。
- 取消传播：API 标记 cancel → 调度器跳过排队任务 → worker 收到信号给构建
  进程组发 SIGTERM（超时 SIGKIILL）→ 清理会话目录。
- 不试图抢占运行中任务（无主流系统支持）。
- 上下文合并：同一 (project, context) 排队中只保留最新构建（借鉴 Netlify）。

## 7. 构建执行流程（hotpot-worker）

```text
1. claim job（长轮询）
2. prepare: 复用主机 CARGO_HOME；按键选取 warm target 卷；创建会话目录
3. fetch source: 共享 git 工作区 fetch + detached checkout（同 URL 加锁）；
   或解压上传 tarball；按需自动安装系统包（slim → build-essential）
4. configure env: RUSTC_WRAPPER=sccache, CARGO_INCREMENTAL=0,
   键控 CARGO_TARGET_DIR，交叉编译 rustlib/cross 工具链
5. run cargo build --build-plan? 逐行采集 stdout/stderr 事件（带 seq）
6. collect artifacts: glob → 计算 blake3 → 上传 hotpot-store
7. report timings（fetch/build/link/upload 分段）与最终状态
8. cleanup: 会话 target 目录按配额保留短期 LRU 后删除
```

执行器抽象 `JobExecutor`：`process`（直接子进程，MVP）/ `docker`（bollard）/
`firecracker`（演进）。

## 8. API 概要（hotpot-api，详见 api-spec 文档）

| Method | Path | 说明 |
|--------|------|------|
| POST | `/v1/builds` | 提交构建（git/tarball），支持幂等键 |
| GET | `/v1/builds/{id}` | 状态与时间分解 |
| GET | `/v1/builds/{id}/logs/stream` | SSE 实时日志（cursor 续传） |
| POST | `/v1/builds/{id}/cancel` | 取消 |
| GET | `/v1/builds/{id}/artifacts` | 产物清单 |
| GET | `/v1/artifacts/{digest}` | 下载产物（短时 token） |
| * | `/sccache/{key...}` | sccache 兼容 GET/PUT |
| * | `/v8/artifacts/{hash}` | Turborepo v8 兼容 |

认证：项目 PAT（哈希存储，可吊销）；后续 OIDC 联邦。

## 9. 热部署链路

### 9.1 生产：hotpot-agent（L3 零停机）

状态机（持久化，崩溃可恢复；point-of-no-return 前自动回退）：

```text
Fetch → Preflight(预热启动, /healthz:warm) → Arm(fd 交接/REUSEPORT)
      → Attach(readiness + LB 注册) → Drain old(SIGTERM, 可配窗口, 强杀兜底)
      → Commit(permanent)   # 上一版本保留, rollback 复用同一状态机
```

- 单机：systemd socket activation / fdstore；无 systemd 用 AF_UNIX
  SCM_RIGHTS 自管交接；
- K8s：输出适配 rollingUpdate（maxSurge 1 / maxUnavailable 0），drain 窗口
  与 terminationGracePeriodSeconds、网格 terminationDrainDuration 嵌套对齐。

### 9.2 开发：hotpot-dev（L2 热重载）

- `hotpot dev`：notify 监听 → 兼容变更热换 dylib（hot-lib-reloader，
  状态保留）→ 不兼容变更回落到整进程快速重启（socket keeper 保端口）；
- 热重载 crate 约定：`crate-type = ["rlib","dylib"]`、`#[unsafe(no_mangle)]`
  的非泛型函数、共享类型布局冻结；
- 控制台说明每次走了热换还是重启及原因。

## 10. 可观测与安全

- 一个 Build 一条 OTel trace，阶段/步骤为 span（含 cache hit 属性）；
- 指标：队列延迟、阶段耗时直方图、成功率、**cache hit ratio（多维度）**、
  节省时间、worker 密度与资源、GC 回收量；
- provenance：控制面生成 in-toto attestation（后续 cosign/Sigstore 签名），
  worker 全程不触达签名密钥；OIDC token 不进入构建环境。

## 11. 里程碑（实现顺序）

| M | 内容 | 验收 |
|---|------|------|
| M0 ✅ | workspace + `hotpot-core` + `hotpot-store`（本地 CAS）+ 单测 | CAS put/get 去重、校验、LRU 测试通过 |
| M1 ✅ | `hotpot-worker` process 执行器：本地跑 cargo 构建 demo-webapp，事件采集 | 命令行直跑成功；冷 9.4s/暖 0.6s/release 12.9s；94 条有序事件；2 个真实 cargo 集成测试 |
| M2 ✅ | `hotpot-scheduler`（SQLite 队列/租约）+ `hotpot-api`（提交/状态/SSE/取消） | HTTP 提交→SSE 96 事件→succeeded→下载 15.5MB 二进制可运行；运行中取消生效 |
| M3 ✅ | `hotpot-cli`（`hotpot` bin）：build(+follow)/logs/cancel/status/artifacts/download，SSE 手写解析 | 全命令实测通过；失败构建 exit 1；since 游标续传；下载保留可执行位 |
| M4 ✅ | `hotpot-cacheproto`：sccache WebDAV 兼容端点（MKCOL/PROPFIND 207 语义）+ Turborepo v8 端点（tenantId/slug 租户隔离、x-artifact-tag） | sccache 89 个 crate 100% 远端命中、冷 11.8s→4.2s；真实 turbo 客户端本地缓存清空后 FULL TURBO（32ms） |
| M5 ✅ | `hotpot-agent`：fd 交接 supervisor + 部署状态机/回滚 | 压测下 0 丢连接（v1/v2 均接流），rollback 可用；修复初始版本双 spawn 幽灵进程缺陷 |
| M6 ✅ | `hotpot-dev`：热重载 + socket keeper | 改函数体 325ms 重建重启、构建失败保留旧进程、重启窗口 0 拒连；watcher 提前注册消除 FSEvents 注册空窗丢事件 |
| M7 ✅ | docker 执行器（bollard）、docker-compose 发行、CI、文档完善 | docker_live 3 测试实测通过（成功/失败/取消，产物经挂载回宿主）；compose 端到端实测：healthz→提交构建→兄弟容器冷构建 3m13s 成功→产物下载（可执行位保留）→Linux 容器内实际运行 API 通过；GitHub Actions CI；README 开源发布 |
| M8 ✅ | 协议层加固（缓存鉴权、`.sccache_check` 契约、turbo 元数据与签名回显、HTTP 层契约测试）；F13 工具链选择；F14 构建列表与 `running` 状态流转；F15 `/metrics`；F16 `/v1/toolchains`；F18 git 来源（默认关闭）；配置体系（TOML + 环境变量 + CLI 三级覆盖）；双协议文档与价值文档 | 协议契约测试 16 项（`.sccache_check` 404/204、Content-Length 严格一致、租户隔离、404 唯一 miss、签名回显与补齐）；工具链 `1.93`/`stable` 成功、`1.60` 明确失败且 `error` 带真实诊断；鉴权 401/404 实测；`/metrics` 与 `/v1/builds` 实测；`clippy -D warnings` 与全量测试通过 |
| M9 ✅ | 真实开源项目构建 + 两级 warm 复用：slim 镜像 build-essential 自动供给、warm target 卷（键控 CARGO_TARGET_DIR + 20GiB LRU）、共享 git 工作区（部分克隆 + 按 URL 全周期锁）；交叉编译 rustlib 持久化 | fd 冷 113.5s→暖 4.3s（26.1x）、bat 38.7→7.6s（5.1x）、ripgrep 22.7→7.9s（2.9x）；cold/warm blake3 全一致、smoke 版本正确；交叉 aarch64→x86_64 315.9s→14.0s（22.6x）；多版本矩阵 1.85/1.93/1.98.0/stable 全绿 |

## 12. 部署形态演进

A. 单二进制（api+scheduler+worker 同进程）+ SQLite + 本地盘
B. + S3/MinIO（object_store）+ 独立 worker 进程（outbound 长轮询注册）
C. 角色分离 + Postgres
D. K8s（Helm）+ Kata/Firecracker + SLSA L3
