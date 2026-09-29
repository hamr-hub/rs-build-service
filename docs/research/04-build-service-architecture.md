# 商业化构建服务 / CI 构建基础设施架构调研报告

> 调研时间：2026 年 9 月
> 调研目标：为「开源 Rust 打包构建服务」（HTTP/gRPC 提交构建、远程缓存、构建队列、可观测）做立项前的架构调研与竞争分析
> 调研方法：Vercel / Netlify 官方工程博客、Docker BuildKit 官方文档与源码 proto、GitHub 官方文档、SLSA 官方规范、GitHub API 实时 star 数据（2026-09-28 拉取）

---

## 0. 阅读指引与五个核心结论

1. **商业化头部（Vercel、Netlify）在 2023–2026 年间全部收敛到同一套架构**：裸金属 KVM + Firecracker microVM 做单次构建强隔离，构建前预热 VM 池把冷启动压到秒级甚至毫秒级，缓存走「只读层叠层 + 网络存储（FSx/快照）+ S3 冷层」。这不是巧合，而是多租户执行任意代码的唯一安全解。
2. **远程缓存是开放协议的天下**：Turborepo v8 HTTP API、Nx v1 OpenAPI、Bazel REAPI（gRPC）都是公开规范，缓存键必须是「全部输入（含编译器版本/编译参数）传递闭包」的哈希。2025 年 TanStack 攻击与 Nx CVE-2025-36852 证明：缓存隔离做到 Build L3 比签名 provenance 更重要。
3. **BuildKit 是事实上的构建执行引擎标准**：LLB 内容寻址 DAG + solver 并行调度 + 多类缓存后端（registry/inline/local/gha/s3/cache mount）+ history API + 原生 OTLP。Earthly、Depot、Dagger 全部构建在它之上，我们不应重写执行器，而应做它的上层服务/可替代引擎。
4. **队列语义的关键认知**：优先级只影响「派发顺序」，没有任何主流系统（Temporal/Celery）能抢占正在运行的构建；取消传播必须自己做（cancel scope → 进程组信号 → VM 销毁）。日志实时性（SSE/WS）与持久化（分片上传对象存储）是两套互补的数据平面。
5. **Rust 生态空白明确**：NativeLink（Rust，1.6k star）只做 Bazel RBE 后端；sccache（7.7k）只做编译缓存；cargo-remote（212，已停更）是 SSH hack。**没有一个「面向打包场景、HTTP API 提交、自带队列与可观测、单二进制自托管」的 Rust 项目**，立项空间成立。

---

## 1. 商业化构建平台基础设施剖析

### 1.1 Vercel：Hive 构建基础设施

Vercel 于 2024-10 公开了代号 **Hive** 的构建平台（2023-11 上线），设计前提是「在多租户硬件上执行潜在恶意代码」。上线后构建性能提升约 30%，Secure Compute 环境供给从约 90s 降到约 5s。

分层模型：

| 概念 | 含义 |
|---|---|
| **Hive** | 区域级集群，一个 region 有多个，各自是独立故障域（blast radius 控制） |
| **Box** | 运行 KVM 的裸金属宿主机，上面跑多个 Firecracker 进程；做了 Docker 镜像缓存（启动从约 2 分钟省掉约 45s）和块设备快照 |
| **Cell** | 一个 Firecracker microVM，与 Firecracker 进程 1:1；独占 vCPU/内存，磁盘与网络被限速；每个 cell 至少跑一个 build container |
| **Control Plane** | 任务放置（placement）、自动扩缩容、实例生命周期、监控、集群健康 |
| **Hive API** | 极简 API，核心只接受「run cell」请求；构建流水线负责提供（已缓存、已预载的）构建镜像 |
| **Box daemon / Cell daemon** | 宿主机 daemon 负责块设备与 Firecracker 进程；通过专用 socket 与 VM 内 cell daemon 通信，由后者启停构建容器 |

调度链路：构建开始 → 构建流水线按客户/配置选 hive → 调 Hive API 在某 cell 的容器里跑 → **Vercel 常驻一个预热 cell 池**，无可用 cell 时约 5s 新建；构建结束即销毁 cell。

2026 年的演进：**VHS（Vercel Hive Snapshot）** 优化的启动/快照格式，让环境「恢复（resume）」而非「引导（boot）」，支撑 Dockerfile 部署与 Sandbox 自定义镜像。Vercel Sandbox（2026 GA）复用同一模式：每次 agent 请求一个 Firecracker microVM，Functions + Workflows 充当「领取排队请求 → 供给 worker → 监控会话 → 清理」的持久控制面。

用户侧并发模型（队列的产品化）：

| 套餐 | 并发构建 |
|---|---|
| Hobby | 1，其余串行排队 |
| Pro | 基线 3，on-demand 最多到 500；生产构建可优先于 preview 构建 |
| Enterprise | 自定义；`vc deploy --turbo` 选 30 vCPU / 60 GB Turbo 机型 |

### 1.2 Netlify：2026 年从 Kubernetes 全面迁移到 Firecracker

Netlify 在 2026 年 4 月宣布弃用 Kubernetes 构建体系，新系统处理 **约 45 万次构建/天、约 300 万次/周**，某大型企业客户端到端构建时间改善 33%。

架构要点：

- **拉取式任务分发**：没有中央调度器 push，每个 worker 自行从 **Redis** 拉任务，消除调度瓶颈与重协调，worker 按自身处理能力取活。
- **VM 预热**：空闲 worker 提前启动 Firecracker、跑 init、sandbox 待命；任务到达时只需配置文件系统 + 发启动命令，每次构建省数百毫秒。
- **自研 autoscaler**：同时监控 Redis 队列流量与宿主机利用率，维持空闲 worker 缓冲吸收尖峰。
- **VM 内通信**：最小化 Linux，init（PID 1）挂载目录/配网络/拉起 sandbox；sandbox 经 **vsock** 连宿主机；持久连接上传 **JSON 消息**（启动消息含命令 + 环境变量，VM 实时回传 stdout/stderr）。
- **Overlay 文件系统缓存**：只读下层（Ubuntu、Node.js、构建工具）全局共享；构建写入独立 upper 层。构建结束 upper 层存到 **FSx** 并登记元数据；同站同分支下次构建把它挂为只读下层，`package.json` 未变时 `npm install` 近乎瞬时。
- **两层存储做成本/速度平衡**：热层在 FSx（快、贵），冷层归档 S3（便宜、延迟高）；缓存在共享网络存储上，跨宿主机可挂载。

P95 提速实测：

| 阶段 | 改造前 | 改造后 | 降幅 |
|---|---|---|---|
| 排队等待 | 约 40s | < 2s | 95% |
| 缓存获取 | 59.4s | 13.4s | 77% |
| 依赖安装 | 55.8s | 24.2s | 56% |
| 缓存保存 | > 4 分钟 | 26s | 约 90% |

三级排队语义（非常值得借鉴的产品设计）：

1. **System queue**：全平台超容量，触发自动扩容；
2. **Team queue**：团队并发超套餐（"Awaiting Capacity"），Enterprise 可提优先级；
3. **Context queue**：同站同部署上下文（同一 preview 编号/分支）串行，且当前构建结束后**只跑队列中最新的一个，丢弃过期请求**——天然的构建合并（coalescing）。

### 1.3 Vercel vs Netlify 对比

| 维度 | Vercel Hive | Netlify（2026） |
|---|---|---|
| 隔离 | KVM 裸金属 + Firecracker cell | Firecracker microVM |
| 分发 | 构建流水线 → Hive API 放置 | worker 从 Redis 自拉 |
| 冷启动 | 预热池；冷供给约 5s；VHS 快照恢复 | 预热 VM，任务到达只剩配 FS + 发令 |
| 缓存 | 块设备快照 + 预载镜像 | overlay 层 + FSx/S3 两层 |
| 跨主机缓存 | 区域镜像/块缓存 | 网络存储天然跨主机 |
| 排队特色 | 套餐并发 + 生产优先 | 三级队列 + context 合并去重 |
| 控制面 | 区域 hive 独立故障域 | 自研 autoscaler |

### 1.4 可迁移到我们系统的模式

- **预热池 + 快照恢复**是冷启动优化的标准答案（Firecracker 快照恢复实测可低至约 28ms 级，社区有 booting sandbox in 28ms 的实践）。
- **缓存 = 只读层 + 每构建可写层**，构建后固化为新版本只读层；层放网络存储换取跨主机调度自由。
- **拉取式队列**（worker pull）比中央 push 调度器更抗瓶颈、更易水平扩。
- **同一上下文只保留最新构建**，丢弃/取消排队中的过期请求。

---

## 2. 远程缓存协议：Turborepo / Nx / Bazel

### 2.1 Turborepo Remote Cache（v8 HTTP API）

开放 HTTP 协议，任何实现该规范的服务器都可作为远程缓存，`https://api.vercel.com` 只是官方参考实现；所有 `turbo` 客户端走 **`/v8` 前缀**。2025 年 Vercel Remote Cache 已免费，并支持 OIDC。

认证：全部 `Authorization: Bearer <token>`；自托管时 token 校验完全由实现方决定；用 `?teamId=<id>` 或 `?slug=<slug>` 区分租户。

| 方法 | 路径 | 用途 |
|---|---|---|
| GET | `/v8/artifacts/status` | 缓存状态（enabled/disabled/over_limit/paused） |
| HEAD | `/v8/artifacts/{hash}` | 工件是否存在（200/404） |
| GET | `/v8/artifacts/{hash}` | 下载：gzip tar，`application/octet-stream`，校验 Content-Length |
| PUT | `/v8/artifacts/{hash}` | 上传二进制流 |
| POST | `/v8/artifacts` | 批量查询 |
| POST | `/v8/artifacts/events` | （可选）命中/未命中分析事件 |

关键自定义头：

- `x-artifact-tag`：工件 **HMAC-SHA256** 签名，上传时客户端给、下载回传，验签失败视为 miss（`turbo.json` 开 `remoteCache.signature`，密钥走 `TURBO_REMOTE_CACHE_SIGNATURE_KEY`）；
- `x-artifact-duration`（生成耗时 ms）、`x-artifact-sha`（commit）、`x-artifact-dirty-hash`（脏工作区摘要）。

客户端指向自托管：CLI `--api/--token/--team`，或环境变量 `TURBO_API / TURBO_TOKEN / TURBO_TEAM`。

### 2.2 Nx 自托管远程缓存（v1 OpenAPI）

Nx v20.8 起开放 **OpenAPI 规范**：

- 单一资源：`PUT/GET /v1/cache/{hash}`，Bearer token，tar 包 octet-stream；
- **条目不可变**：重复 PUT 返回 409；PUT 必带 Content-Length；
- 环境变量：`NX_SELF_HOSTED_REMOTE_CACHE_SERVER`、`NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN`；
- 官方提供 `@nx/s3-cache`、`@nx/gcs-cache`、`@nx/azure-cache`、`@nx/shared-fs-cache`（19.8+，需 activation key）；
- 旧的 custom task runner API 已废弃。

安全警示：**CVE-2025-36852（"CREEP"）**——基于裸 bucket 的自托管缓存（官方 bucket 包与社区 S3/GCS 实现）存在严重缓存中毒：任何有 PR 权限者可向生产构建投毒。规避方式是按 OpenAPI 自建带鉴权的服务器，或用托管 Nx Cloud。

### 2.3 Bazel Remote Execution API（REAPI）

gRPC（也有 HTTP 转码），核心三块：**Content Addressable Storage（CAS，ByteStream 上传下载）**、**Action Cache（AC，按 Action 摘要查结果）**、**Execution API（远程执行 Operations）**。兼容客户端：Bazel、Buck2、Pants、reclient、moon（moonrepo v1.30+ 走 REAPI，zstd 压缩、完整性校验）。

### 2.4 三套协议对比与我们的兼容策略

| 协议 | 传输 | 标识 | 不可变 | 自托管生态 |
|---|---|---|---|---|
| Turborepo v8 | HTTP REST | 内容 hash | 事实上不可变 | ducktors（Node）、多个 Rust 实现 |
| Nx v1 | HTTP REST | hash | 强制 409 | 官方 bucket 包 + 自建 |
| Bazel REAPI | gRPC | Digest(hash+size) | 是 | NativeLink、bazel-remote、BuildBarn 等 |

**建议**：我们的缓存内核做一套内容寻址 blob store，协议层先实现 **Turborepo v8 兼容端点**（生态最大、规范最简单、可直接蹭 JS/TS 用户），中期评估 Nx v1 与 REAPI 适配；这样既被现有客户端直接使用，又服务自己的打包任务。

---

## 3. GitHub Actions Runner 架构

### 3.1 GitHub 托管与作业派发

- GitHub 托管 runner 是 **Azure 上的一次性 VM**（一 job 一 VM，作业结束即销毁），每个作业有独立的 **job token**（短时、按作业授权，用于 Actions API、artifact 上传等）。
- Runner 与 GitHub 之间不是消息队列，而是 **HTTPS 长轮询（long poll）**：runner 挂起连接直到收到消息。

### 3.2 Actions Runner Controller（ARC，K8s operator）

2025–2026 的主流自托管模式是 autoscaling runner scale sets：

| 组件 | 职责 |
|---|---|
| AutoScalingRunnerSet controller | 取 runner group id，建 scale set 与 Listener |
| **RunnerScaleSet Listener Pod** | 对 GitHub 开长轮询，收到 `Job Available` 即驱动扩缩；本质是「队列订阅器」 |
| EphemeralRunnerSet | 一次性 runner Pod 集合 |
| EphemeralRunner controller | 向 GitHub 申请 **JIT（just-in-time）配置令牌**注册 runner（替代老式长效 PAT/registration token）；建 Pod 失败重试 5 次；无人接单 24h 后 GitHub unassign |
| Runner Pod | 用 JIT token 注册，自己再开长轮询收作业；**一 job 即弃**，结束后控制器确认并销毁 |

部署产物走 OCI Helm chart（`ghcr.io/actions`），runner 镜像内置 runner 二进制 + container hooks + DinD；认证推荐 GitHub App。

### 3.3 对我们的启示

- **长轮询是「自建 worker 接入 SaaS 控制面」最轻的队列形态**：worker 只要能出网 HTTPS 即可，无需入站端口（Earthly Satellites、ARC 同理，均 outbound 注册 + mTLS）。
- **JIT 短时令牌**优于长效注册凭证：worker 启动时换一次性 token，缩小泄露窗口。
- 一次性执行环境 + 控制器确认后销毁，是多租户 runner 的标准生命周期。

---

## 4. BuildKit 架构深入

### 4.1 LLB：内容寻址构建 DAG

BuildKit 的中心是 **LLB（Low-Level Build format）**——定义**内容寻址依赖图**的二进制中间格式。前端（Dockerfile frontend 本身以镜像分发；Earthly、HLB、Dagger 等）把人类可读定义编译成 LLB，类似源码编译成汇编。

顶点类型：

- **SourceOp**：从镜像/Git/本地导入文件系统（图根）；
- **ExecOp**：在输入文件系统上执行命令，支持多输入挂载；
- **CopyOp/FileOp**：跨文件系统拷贝/文件操作；
- **MergeOp / DiffOp**（v0.10+）：组合/切分文件系统避免数据复制；Netflix 报告复杂构建从 >1 小时降到 3 分钟。

### 4.2 Solver：并发 DAG 调度器

BuildKit 本质是**并发 DAG solver**：执行顶点前检查输入是否变化，未变则复用缓存结果；无依赖阶段并行、未使用阶段跳过、只传输变化的 context 文件。内部用基于顶点的缓存键（op metadata + 输入摘要）做复用决策。

### 4.3 Worker：OCI 与 containerd

`buildkitd.toml` 配两类后端：

- `[worker.oci]`：原生 runc，snapshotter `auto`/overlayfs/native，支持 rootless；
- `[worker.containerd]`：containerd worker（namespace + socket）。

每个 worker 可设 GC 策略、`max-parallelism`（并发步骤上限）、CNI 网络池、多平台；gRPC 暴露 `ListWorkers`。

### 4.4 缓存后端全景

| 后端 | 形态 | 要点 |
|---|---|---|
| **cache mount** | `RUN --mount=type=cache,target=...` | 持久包缓存（如 `/go/pkg/mod`），不进镜像层；**默认不随 registry/GHA 缓存导出**（社区用 buildkit-cache-dance 绕） |
| **inline** | 缓存元数据塞进镜像 | 只支持 `mode=min`（最终阶段层） |
| **registry** | 独立推缓存镜像 | `mode=max` 含全部中间阶段，任意主机可按需拉取 |
| **local** | 本地目录 | 离线/单机 |
| **gha** | GitHub Actions Cache API | scope/mode/ignore-error |
| **s3 / azblob** | 对象存储直连 | 较新版本加入，适合自托管分层 |

### 4.5 Control / History API

gRPC `Control` 服务：`Solve`、`Status`、`Session`、`DiskUsage`、`Prune`、`ListWorkers`、`Info`；

**History API**：`ListenBuildHistory`（流式推送 STARTED/COMPLETE/DELETED 事件）、`UpdateBuildHistory`；`[history]` 配置 `maxAge`（默认 48h）、`maxEntries`（默认 50，0 为关闭）。`SolveRequest.CacheOptions` 携带类型化的缓存导入/导出条目。

### 4.6 对我们的定位建议

**不重写 BuildKit**。打包服务的执行层可以：① 直接以 BuildKit 为引擎（构建镜像类任务）；② 对非镜像打包（cargo/npm 包、tarball 产物）用自有的「容器/ microVM + 步骤执行器」，但复用 LLB 式的内容寻址 DAG 思想（步骤 hash + 输入闭包）与 OTLP trace 模型。Earthly/Depot/Dagger 已验证「BuildKit 即引擎、上层做产品」路线。

---

## 5. 隔离技术对比

### 5.1 为什么构建任务必须强隔离

- 构建会执行**任意第三方代码**：npm postinstall、cargo `build.rs`、makefile、setup.py、gradle plugin、容器构建的 RUN；
- 典型供应链攻击就是在安装/构建阶段植入后门、窃取环境变量中的密钥（云凭证、registry token、OIDC token）；
- 多租户共享宿主时，租户间需要内核级边界；同一租户的不同构建之间也需要 build-to-build 隔离（防止上一次构建留下被污染的 `make`/编译器/缓存）。

### 5.2 技术谱系

| 技术 | 隔离层 | 冷启动（参考实测） | 单实例内存开销 | 运行时开销 | 需要 KVM | 适用场景 |
|---|---|---|---|---|---|---|
| **runc / namespace** | 共享内核容器 | 约 20ms | 约 7 MB | 基线 | 否 | 可信代码、内网 |
| **gVisor (runsc)** | 用户态内核拦截 syscall | 约 50ms | 约 18 MB | syscall 密集型 10–30% | **否** | 无嵌套虚拟化环境、中等对抗 |
| **Kata + QEMU** | 硬件虚拟化 VM | 约 500ms | 约 52 MB | 5–15% | 是 | K8s 强隔离 |
| **Kata + Firecracker** | 轻量 VMM VM | 约 125ms | 约 28 MB | 5–15% | 是 | K8s 强隔离提速 |
| **Firecracker** | 极简设备模型 microVM | < 125ms（**快照恢复可低至约 28ms 级**） | < 5 MiB；单机 150 VM/s | 约 2–8% | 是 | 高密度、函数/构建池 |
| **Cloud Hypervisor** | 精简 VMM | < 100ms | 低 | 约 2–8% | KVM + MSHV | 需要热插拔/实时迁移/GPU VFIO |

补充：

- **Firecracker**：仅 5 个模拟设备、内置磁盘/网络限速器、jailer 进程做宿主机侧加固；支撑 AWS Lambda 每日数十亿调用；
- **Cloud Hypervisor**：同源于 Intel（rust-vmm 社区），是 **Kata 在 K8s 中的默认 VMM**（替代 QEMU 显著减重），支持 CPU/内存/PCI 热插拔、实时迁移、VFIO GPU 直通；
- **Kata 3.x** 已支持机密容器（Intel TDX、AMD SEV-SNP）；
- gVisor 在「云主机不允许嵌套虚拟化」时不可替代，但文件系统/系统调用兼容性与开销是其短板。

### 5.3 我们的隔离选型路线

- **MVP**：Docker/containerd 容器（runc）+ 每构建独立容器、独立用户、禁特权；
- **V2（多租户/公网）**：Firecracker microVM，一构建一 VM（或一 VM 内顺序多步骤），预热池 + 快照恢复；
- K8s 环境内可直接用 Kata（Cloud Hypervisor VMM）作为节点级 RuntimeClass，免自管裸金属。

---

## 6. 构建队列、调度与日志流

### 6.1 Temporal vs Celery（2026 年视角）

Temporal 的 **Task Queue Priority and Fairness** 于 2026-05 GA；Celery 仍是经典 Python 任务队列。

| 关注点 | Temporal | Celery |
|---|---|---|
| 优先级 | 整数键 1–5（1 最高），分层子队列，按分区近似严格 | broker 级 `priority`，行为随 broker |
| 租户公平性 | **fairness key 虚拟队列 + 加权 round-robin**，防大租户饿死他人，可做 80/20 容量带 | 需自建多队列/路由 |
| 抢占运行中任务 | **不支持**，优先级只管派发；抢占要靠 Cancel（协作）或 Terminate（杀整树） | **不支持**；revoke 只跳过未启动任务 |
| 取消传播 | **cancel scope 递归传播**到 activity/timer/child workflow，优雅清理、子级回执；Terminate 为不可处理的强杀 | revoke 广播名单（内存、默认 3h），`terminate=True` 杀 worker 子进程，官方禁止程序化调用 |
| 长流程持久性 | 事件溯源 + 确定性 replay，崩溃/发布后自动恢复，免手写状态表 | 任务中间态不是一等公民，需自建 checkpoint |

关键架构认知：**没有任何主流队列提供「运行中构建」的抢占原语**。高优先级构建只能插队等待；真要让位只能 kill。CI 场景的 cancel 必须端到端打通：API 标记 → 调度器忽略排队中的该 job → worker 收到信号给构建进程组发 SIGTERM（超时 SIGKILL）→ 容器/VM teardown → 释放资源。

### 6.2 调度需要解决的问题清单

- 优先级（交互式 release 构建 > 批量 nightly）；
- 并发控制：全局/团队/项目三级配额；
- 租户公平性：加权 fair queue，防 noisy neighbor；
- 放置（placement）：按架构（x86/Arm）、缓存亲和（缓存层所在主机/机架）、资源机型（CPU/RAM/磁盘）匹配；
- 超时、重试（区分可重试错误 vs 构建失败）、幂等（相同输入合并）；
- 抢占：通过「优先级 + 取消低优先级可牺牲任务」自行实现；
- cancel 传播（见上）；
- worker 心跳/失活接管（worker 死掉后其 in-flight job 重新入队）。

### 6.3 日志流式传输

实时面（构建运行中）：

| 方案 | 特点 | 适用 |
|---|---|---|
| **SSE**（`text/event-stream`） | 纯 HTTP、单向推送、浏览器自动重连 | tail 式日志，简单首选 |
| **WebSocket** | 双向，客户端可发 cancel/控制帧 | 交互式控制台、多任务复用 |

工程要点：日志以结构化事件（时间戳、步骤、流别 stdout/stderr）传输而非裸字符串；worker 端捕获子进程输出按行/按块推送，带序号便于断线续传。

持久化面（构建后）：

- S3 类对象存储**不可追加**，不能当活日志文件反复覆盖（竞态、读到半截）；
- 标准模式：构建写本地日志 → shipper（如 Fluent Bit）tail + 内存/磁盘缓冲 + gzip/zstd 压缩 → 按大小/时间（如 5MB/60s）**multipart upload**（每片 ≥5MB、≤10000 片，失败单片独立重传）；
- 对象按时间前缀分区（`build-logs/YYYY/MM/DD/`），便于 Athena/Select 查询；
- 实时面与持久面互补：SSE/WS 保证看得到，对象分片保证存得下、查得到。

---

## 7. 内容寻址存储（CAS）实践

### 7.1 核心设计

- **身份 = 内容哈希**（BLAKE3 / SHA-256），天然去重、完整性可校验；
- 磁盘布局：哈希前 2 位十六进制成 **256 个分桶目录**（`objects/ab/cdef...`），避免单目录 inode 爆炸；
- blob 不可变；上传先写临时文件再原子 rename，读时校验哈希；
- 小对象打包成 **pack 文件**（多个 blob 合一个）+ 有序 hash→offset 索引，降低文件系统元数据开销；
- **zstd 压缩**（loose blob 可选、pack 默认；注意已压缩内容（gz/png）跳过或低等级）。

### 7.2 去重与 GC 的两条路线

| 路线 | 机制 | 适用 |
|---|---|---|
| **引用计数** | 写时建引用、重复写只增计数；计数归零且未 pin 才删除 | 引用关系简单（如 Trove：Pin/Unpin + RefCount） |
| **Mark–Sweep** | blob 不透明，宿主定期提供 live set，扫描出不可达再删（dry-run/trash/delete 三模式，可重写 pack） | 存在复杂引用图、跨租户共享（如 cas-kit、git 式模型） |

### 7.3 分层与淘汰

- **分层**：RAM（最热元数据/小 blob）→ 本地 NVMe（LRU 热缓存）→ S3/MinIO（冷层）；读穿透（read-through），miss 时后台下载，先腾空间再落盘；
- 冷层把小块攒成大对象（如数 MB–64MB 不可变 blob）摊薄 PUT/GET 成本；
- **淘汰策略与机制解耦**：存储层只提供 put/get/delete，LRU/TTL/配额由上层策略决定；
- 租户级配额 + 全局 LRU + TTL（缓存层）共存；pin 机制保护 release 产物；
- 大文件上传：客户端分片 + 预签名 URL 直传对象存储，服务端只做哈希校验与清单合并。

### 7.4 可直接参考的 Rust 实现

- **cas-kit**（Rust crate）：BLAKE3、256 桶、zstd、pack + idx、mark-sweep GC（Tokio + CLI），设计完整但体量小、生产验证少；
- **NativeLink** 的 tiered store（RAM→NVMe→对象存储）是更重的工业实现；
- **object_store** crate 是对接 S3/GCS/Azure/本地的事实标准抽象。

---

## 8. 可观测性

### 8.1 构建耗时分解（timing breakdown）

一次构建至少拆成：排队等待 → 调度/放置 → 环境启动（拉镜像/起 VM）→ **输入获取（git clone / 缓存拉取）→ 依赖安装 → 编译 → 链接 → 产物打包 → 上传/推送 → 清理**。

落地方式：

- **OpenTelemetry trace**：一个构建一条 trace，每个阶段/步骤一个 span；BuildKit **原生 OTLP 导出**（每 layer 一个 span，含指令、耗时、**cache hit/miss**）；
- 通过 W3C `traceparent` 把构建 trace 挂到上游 CI/发布流水线的 trace 下；
- 自定义执行器则用 wrapper/中间件创建 root span + 每步骤 span（`tracing` + `opentelemetry` crate）。

### 8.2 关键指标（Metrics）

| 类别 | 指标 |
|---|---|
| 队列 | 排队长度、等待时长（p50/p95/p99）、入队/出队速率、过期丢弃数 |
| 执行 | 各阶段耗时直方图、总构建时长、成功率、超时率、重试率 |
| 缓存 | **cache hit ratio**（按租户/项目/步骤/缓存类别打标签）、命中/未命中计数、缓存读写延迟、缓存节省时间、上传/下载字节量 |
| 资源 | worker 数、空闲/忙碌、microVM 密度、CPU/内存/磁盘、GC 回收字节 |
| 安全 | provenance 生成成功率、验签失败数、拒绝的跨租户访问数 |

cache hit ratio 用 hits/(hits+misses) 滑动窗口计算；批量恢复按条目记录并汇总；把 `cache.hit` 同时挂到 span 属性，trace 与指标可互相印证。

### 8.3 诊断工具

- **构建火焰图/时间线**：前端把阶段耗时渲染成甘特/火焰视图，快速发现「一个早期 COPY 失效打穿下游全部层」「依赖安装慢」「context 过大」等问题；
- 历史构建对比（同项目不同 commit 的阶段差异）；
- Prometheus + Grafana / Jaeger（或任意 OTLP 后端）；日志按 build_id 关联 trace_id。

---

## 9. 安全：provenance、SLSA 与缓存中毒防护

### 9.1 SLSA 模型与「毒化构建缓存」威胁

SLSA v1.1 威胁文档把缓存中毒列为 **Build L3** 对抗项：

- 威胁：攻击者把恶意工件放进缓存，后续良性构建取用；
- L3 缓解要求：
  - 缓存**构建间隔离**；
  - 缓存键**覆盖被缓存工件的全部输入传递闭包**（不仅源码摘要，还含编译器版本/摘要、编译参数、环境）；
  - 缓存**仅允许可信控制面写**，或每条缓存携带与 key 匹配的 L3 provenance；
  - **签名密钥只在控制面**，绝不下发 worker；build-to-build 隔离（理想情况每构建独立 VM + 干净镜像）。

文档给的两个教科书攻击：① key 不含编译参数 → 攻击者用 `-DCheckAuth(ctx)=true` 编译投毒；② 租户直接向合法 key 写恶意 `auth.o`。

### 9.2 真实案例：TanStack「Mini Shai-Hulud」攻击（2025）

攻击链：

1. `pull_request_target` 让 fork 代码在基仓可信上下文运行；
2. 投毒 **pnpm 包仓缓存**，而 GitHub Actions 跨触发类型共享缓存作用域，`release.yml` 恢复了毒缓存；
3. release job 带 `id-token: write`，毒代码从 runner 进程内存**刮取 OIDC token**，绕过发布条件判断直发 npm。

结果：84 个恶意包，且携带**密码学有效**的 npm provenance（Sigstore Fulcio 用被盗 OIDC 签发、Rekor 记账），builder/repo/workflow 字段全部真实，与合法包无法区分。

教训：npm 内置 provenance 只达 **SLSA Build L2**（认证了平台身份，没有隔离保证）；有效 attestation 只证明「平台看到了什么」，不证明代码可信。L3 三要素（缓存隔离、签名身份对构建代码不可达、构建间不留存）全部被破坏。

### 9.3 标准与我们的落地

- **in-toto attestations**：声明式 provenance（builder、材料、参数、产物摘要）；
- **Sigstore**：Fulcio（基于 OIDC 的短生命周期证书）+ Rekor（透明日志）+ cosign（签名/打包）；
- 建议：**签名动作放在控制面**，worker 不接触长期签名密钥；OIDC 仅用于入站联邦认证，worker 内不暴露可发布的身份令牌；
- 缓存：**租户 key 前缀/独立 namespace**、控制面统一写、键含输入闭包（编译器 digest + flags + 依赖闭包）、下载校验哈希与可选 HMAC；
- 消费端策略：pin 期望的 builder identity，而不是「有合法 attestation 就放行」；
- 其他基线：构建参数在系统边界校验、secret 短期化/按步骤注入、产物保留期与访问审计、依赖签名校验（cargo 等原生弱，需配套 lockfile/来源策略）。

---

## 10. 开源参考与 Rust 生态竞争分析

### 10.1 全功能 CI 类

| 项目 | Star（2026-09） | 语言/活跃度 | 架构 | 与我们差异 |
|---|---|---|---|---|
| **Woodpecker CI** | 7.9k | Go，极活跃（日级提交） | server + agent（gRPC），SQLite/Postgres，容器每步骤，自动伸缩云主机；Gitea/Forgejo 亲和 | 是「完整 CI」（webhook → 流水线），非「打包即服务」；容器级隔离；非 Rust |
| **Drone** | 已并入 **harness/harness（38.4k）** | Go，活跃 | 容器原生 CI 的鼻祖；Drone + Gitness 合并为 Harness Open Source | 平台化后偏重；自托管小团队流向 Woodpecker |
| **Agola** | 1.6k | Go，低活跃（最后提交 2025-09） | gateway/scheduler/run/config 服务 + executor，etcd + 对象存储；Run 是可从失败点重启的 DAG，at-most-once 部署 | DAG/重启模型优秀，但社区萎缩；非打包导向 |

### 10.2 远程构建 / Runner 增强类

| 项目 | 规模 | 形态 | 关键点 |
|---|---|---|---|
| **Earthly** | 12k star | Go，活跃 | Earthfile → LLB，BuildKit 执行；**Satellites** 提供常驻暖缓存（增量构建 12min→<3min 的案例）；卫星为商业产品，自托管 Beta（特权镜像、outbound 注册、无自动更新/休眠） |
| **Actuated** | 商业（开源组件如 SlicerVM） | Firecracker runner | 为 GitHub Actions/Jenkins 提供一次性 microVM（约 1s 启动）；CNCF/Arm 生态用其替代 ARC（ARC 的特权 Pod + DinD VFS 慢 5–10 倍）；需 KVM 主机 |
| **Depot** | 商业 | 远程 BuildKit | 带持久缓存的远程 BuildKit 构建，云原生加速 Docker 构建 |
| **Dagger** | 开源（大型社区） | 可编程 CI 引擎 | 以代码 SDK 写 pipeline，底层 BuildKit；定位是 CI SDK 而非打包服务 |

### 10.3 RBE（远程执行/缓存）后端

| 项目 | Star | 语言 | 特点 |
|---|---|---|---|
| **NativeLink** | 1.6k | **Rust，极活跃** | REAPI v2（CAS+AC+Execution），分层存储 RAM→NVMe→S3，多租户 namespace，Prometheus，单二进制 | 
| **bazel-remote** | 775 | Go，活跃 | 极简 REAPI cache（也含部分执行），成熟稳定 |
| BuildBarn / BuildFarm / BuildGrid | 数百–上千 | Go / Java / Python | 完整分布式 RBE 生态，部署运维重 |

### 10.4 Rust 生态盘点（立项竞争分析，star 为 2026-09-28 实时值）

| 项目 | Star | 活跃度 | 是什么 | 与我们设想的差异 / 缺口 |
|---|---|---|---|---|
| **TraceMachina/nativelink** | 1.6k | **极活跃**（每日提交） | Rust 的 Bazel RBE 服务器（CAS/AC/执行） | 绑定 Bazel REAPI 生态，概念重（action/platform/property）；**没有面向用户的打包构建 API、构建队列产品层、git/tarball 提交流、日志 UI**；是「后端引擎」而非「打包服务」 |
| **mozilla/sccache** | 7.7k | **极活跃**（本周提交） | 编译缓存守护（Rust/C/C++），S3/GCS/Redis 后端 | 只缓存编译单元，**不缓存/不执行整个构建**；无法缓存链接产物（bin/dylib/proc-macro）、不支持增量；没有队列 |
| **sgeisler/cargo-remote** | 212 | **已停更**（最后提交 2024-06） | SSH+rsync 同步到单机远程构建 | hacky、无隔离、无队列、无 API、单点；作者自陈测试不足 |
| **matthiaskrgr/cargo-cache** | 994 | **停更**（2023-06） | 本地 `~/.cargo` 清理工具 | 纯本地维护，无服务概念 |
| **Swatinem/rust-cache** | 1.9k | 活跃 | GitHub Action（TS）：生成 cargo/target 缓存键 | 只是 CI 缓存策略 action，不是服务 |
| **brunojppb/turbo-cache-server** | 223 | **极活跃**（本周提交） | Rust 实现 Turborepo v8 缓存（S3/R2/MinIO），Action/Docker 分发 | 只做缓存端点，**不执行构建、无队列/产物管理**；可直接作为协议兼容参照 |
| **salamaashoush/turbo-remote-cache-rs** | 37 | 中低活跃 | actix-web 实现 turbo 缓存（object_store 多后端） | 同上，仅缓存 |
| **moonrepo/moon** | 4.1k | **极活跃** | Rust 的 monorepo 任务运行器；远程缓存走 REAPI | 是**客户端工具**，不自研服务端；不提供托管构建 |
| **facebook/buck2** | 4.4k | **极活跃** | Rust 的大规模构建引擎（Starlark） | 构建工具，远程执行依赖 REAPI 后端；非服务 |
| **axodotdev/cargo-dist** | 2.1k | **极活跃** | 发布/打包编排（产物组装、CI 生成、announce） | 面向「发布流水线」，构建仍跑在既有 CI，**不提供远程执行/缓存服务**；可作为打包场景的需求参考 |
| **OpenCz/wizo** | 0（beta） | 实验（2026-08 首发） | 轻量 Rust CI/CD，YAML，本地优先 | 极早期，无缓存/隔离/多租户 |
| **WyattAu/cas-kit** | crate 级（百次下载） | 小 | BLAKE3/zstd/pack/GC 的 CAS 库 | 是库不是服务；可作为存储层参考/依赖评估对象 |
| **xetdata/xet** | 中大型（XetHub/GitHub 背书） | 活跃 | Rust 的分块去重 CAS 协议（XET/IETF draft，Git XFLT 背后） | 解决「大文件传输去重」，不是构建服务；其分块协议值得借鉴 |
| **Firecracker / Cloud Hypervisor** | 数万 | 极活跃 | **Rust 写的 VMM 本体** | 是我们要集成的底座组件，不是竞品 |
| **apalis** | crate 级 | 活跃（1.0 rc） | Rust 后台任务/工作流库（Redis/PG/SQLite，DAG，tower 中间件） | 队列「库」，可直接用；无构建领域能力 |

**竞争结论**：

1. 与我们正面重叠度最高的是 **NativeLink**，但它是 Bazel 生态后端，用户画像、协议、交互层完全不同；
2. 缓存层有多个 Rust 轮子（turbo-cache-server、cas-kit），可借鉴/依赖而非对打；
3. **「面向打包/发布场景 + HTTP/gRPC 提交 + 队列调度 + CAS 缓存 + 强隔离执行 + 可观测 + 单二进制自托管」的完整 Rust 产品处于空白**；
4. 差异化定位：不做「又一个 CI 系统」，而做 **CI 与开发者都能调用的「构建/打包后端」（build backend as a product）**，兼容开放缓存协议，单二进制即可起步。

---

## 11. API 设计

### 11.1 源码输入：git URL vs tarball 上传

| 方式 | 优点 | 缺点 |
|---|---|---|
| **git URL + ref** | 无需搬源码；天然 provenance（commit SHA、远程地址） | worker 需出网 + 持有仓库 deploy key/token；私有仓库凭证管理复杂；浅克隆/子模块策略 |
| **源码 tarball 上传** | 支持内网/气隙、客户端任意来源；服务端不存代码托管凭证 | 需处理大文件分片、去重；丢失 git 元数据（可单独传 commit 信息） |

建议两者都支持：`POST /v1/builds` 中给 `source.git{url,ref,sha}` 或 `source.upload{upload_id/分片清单}`；最终 worker 拿到的都是「一份落盘源码 + 来源元数据」。

### 11.2 构建提交参数（示例字段）

- builder/镜像（如带特定工具链版本的构建镜像）或构建步骤清单（image + command + workdir）；
- 环境变量（区分普通 env 与 secret 引用，secret 不下日志）；
- 资源机型（CPU/RAM/磁盘/架构）、超时；
- 缓存作用域（项目默认 / 自定义 key / 只读共享缓存）；
- 产物定义（输出路径 glob、打包格式、保留期）；
- 幂等键（idempotency key）与合并策略；
- 回调 webhook、是否可被抢占。

### 11.3 状态、日志、产物、取消

- **状态获取**：`GET /v1/builds/{id}`（轮询）+ `POST /v1/builds/{id}/cancel`；
- **实时**：`GET /v1/builds/{id}/logs/stream`（SSE，带 cursor 续传）；
- **回调**：构建终态（成功/失败/取消/超时）→ 注册的 webhook（带签名、重试）；
- **产物**：`GET /v1/builds/{id}/artifacts` 列表；下载走短时预签名 URL（大文件支持断点/分片）；
- 列表接口支持按项目/状态/时间过滤与分页。

### 11.4 认证

- **Token**：项目/团队级 PAT，限 scope 与过期时间，管理面可吊销；worker 间通信用短时 JIT token / mTLS（证书轮换）；
- **OIDC 联邦**：CI（GitHub/GitLab）用 OIDC token 换本服务短时 token，信任策略 pin issuer + repo + branch；
- API 审计日志（who/build_id/动作）；
- gRPC 内部服务可选（scheduler↔worker），外部 API 以 HTTP/JSON 为主（gRPC 可作二期，提供 codegen 友好面）。

---

## 12. 部署形态：从单二进制到 K8s

### 12.1 形态分级

| 阶段 | 形态 | 存储 | 适用 |
|---|---|---|---|
| A | **单二进制**（api+scheduler+worker 同进程，容器后端用本机 Docker） | 嵌入式元数据库（SQLite/redb）+ 本地磁盘 CAS | 小团队/个人，一台 VPS，5 分钟跑起来 |
| B | 单二进制 + 对象存储（MinIO/S3）+ 远程 worker 进程 | SQLite/Postgres，CAS 走 S3 | 单机控制面、多台构建机 |
| C | 组件分离（api / scheduler / worker 独立伸缩） | Postgres + Redis（队列/状态） | 中型团队，HA |
| D | K8s（Helm chart）：Deployment + 构建节点 DaemonSet/独立裸金属池 + Kata/Firecracker | Postgres HA + S3 + 监控栈 | 大规模多租户、多区域 |

### 12.2 SQLite 单二进制的持久性工程

- SQLite WAL 模式（`journal_mode=WAL`、`synchronous=NORMAL`、`busy_timeout`）；
- 备份/复制：可用 **Litestream 侧车**持续把 WAL 流式复制到 S3（单写者约束）；K8s 上对应 StatefulSet replicas=1（或 `Recreate`）+ PVC + init 容器 restore + sidecar replicate；
- Rust 内嵌 KV 可选 **redb**（纯 Rust、无 C 依赖、B+tree、适合元数据/队列），或直接 SQLite（sqlx，生态/查询能力强）；
- 演进时让仓储层同时支持 SQLite 与 Postgres（sqlx 的好处），迁移只换连接串 + 少量方言；
- 关键纪律：**从第一天起让 api / scheduler / worker 逻辑在同一 crate 内模块化**，单体只是「部署形态」而非「耦合形态」，拆分时不改代码只改启动参数。

### 12.3 配置与分发

- 单二进制 + TOML/YAML 配置 + 环境变量覆盖；容器镜像与 Helm chart 官方维护；
- worker/agent 支持 outbound-only 接入（NAT 友好），自动注册、心跳、自升级；
- 提供 docker-compose 一键形态（含 MinIO + 监控），覆盖 80% 自托管场景。

---

## 13. 推荐的目标架构

### 13.1 组件划分（Cargo workspace）

```
rs-build/
├── crates/
│   ├── rsbuild-api          # HTTP API（axum）：构建提交、状态、日志 SSE、产物、认证
│   ├── rsbuild-scheduler    # 队列与调度：优先级/配额/公平、放置、worker 心跳、cancel 传播
│   ├── rsbuild-worker       # 执行器：拉任务、管容器/ microVM、步骤执行、日志采集、产物上传
│   ├── rsbuild-store        # CAS blob 存储：本地磁盘 + S3、BLAKE3、zstd pack、LRU/GC
│   ├── rsbuild-cache-proto  # 缓存协议适配：Turborepo v8（首选）、Nx v1、（评估 REAPI）
│   ├── rsbuild-proto        # 内部 gRPC 定义（tonic）：scheduler <-> worker
│   ├── rsbuild-core         # 领域模型、状态机、错误类型、配置、可观测初始化
│   └── rsbuild-agent        # CLI（clap）：提交/查看/tail 日志/下载产物/注册 worker
└── bin/
    └── rsbuild              # 单体入口：--role api|scheduler|worker|all
```

数据流：

1. `agent` / HTTP 客户端 → **api** 提交构建（git 或上传分片）；
2. api 落库（build 记录 + 输入清单）→ **scheduler** 入队；
3. **worker** 拉取任务 → 准备隔离环境（容器 → microVM）→ 挂载/拉取缓存层 → 执行步骤；
4. 日志：worker → 消息/直连 → api SSE 推客户端；同时写本地 JSONL → 分片上传 S3；
5. 产物：worker 写 **store（CAS）** → 登记引用 → 客户端预签名下载；
6. 每个阶段发 OTel spans + Prometheus 指标；终态触发 webhook；
7. 控制面在构建结束后生成 in-toto provenance（可选 cosign 签名），worker 全程不触达签名密钥。

### 13.2 技术选型建议

| 关注点 | 选型 | 理由 |
|---|---|---|
| 异步运行时 | **tokio** + **tower**（层/中间件） | 生态标准 |
| HTTP API | **axum 0.8** | 与 tower/hyper 一体，类型安全，SSE/WS 原生 |
| gRPC | **tonic 0.12** | 内部服务双向流（日志/心跳/控制）成熟 |
| 数据库 | **sqlx**（编译期校验），先 SQLite 后 Postgres | 单二进制零外部依赖，演进无缝 |
| 嵌入式 KV（可选） | **redb** | 纯 Rust，适合本地队列/索引，免 C 依赖 |
| 队列 | MVP：DB 表 + 行级锁 / Redis；V2：Redis Streams；重型工作流评估 Temporal（Rust SDK） | 与部署形态分级匹配；避免一上来背 Temporal 运维 |
| 对象/文件抽象 | **object_store**（S3/GCS/Azure/本地） | 一套接口覆盖分层 |
| 哈希 / 压缩 | **blake3** + **zstd** | CAS 身份与打包；读时校验 |
| 容器执行 | **bollard**（Docker API）→ containerd/自管 | MVP 最快落地 |
| microVM | Firecracker REST API（jailer + 快照恢复）；K8s 内用 Kata RuntimeClass | 强隔离路线 |
| 可观测 | **tracing** + **tracing-opentelemetry** + **opentelemetry**（OTLP）+ metrics | 与 BuildKit trace 同协议 |
| CLI | **clap** + tokio | 标准 |
| 配置/序列化 | serde + TOML | 标准 |
| 认证 | 自管 PAT（哈希存储）+ OIDC 联邦；worker mTLS/JIT | 参考 ARC/Netlify |

### 13.3 MVP → 完整版分期路线

**Phase 0 — MVP（约 4–6 周）：本地能跑的单二进制**

- 单二进制（`--role all`）+ SQLite；
- 提交构建（git URL 与本地 tarball 两种输入）；runc 容器执行多步骤；
- SSE 实时日志；状态查询/取消（进程组信号 + 容器销毁）；
- 产物落本地目录 + 下载接口；项目 token 认证；
- 基础 OTel trace + Prometheus 指标 + 阶段耗时。

**Phase 1 — 缓存与自托管体验（约 6–8 周）**

- CAS 存储：BLAKE3 去重、zstd、本地 LRU + GC（先引用计数）；
- 构建缓存：步骤级 hash（输入闭包）命中复用；
- **Turborepo v8 兼容缓存端点**，直接被 turbo 客户端使用；
- S3/MinIO 后端（object_store）；webhook 回调；docker-compose 发行；
- 构建时间线视图（timing breakdown）。

**Phase 2 — 分布式与多租户（约 8–12 周）**

- api/scheduler/worker 角色分离部署；Postgres + Redis；独立 worker 池（outbound 注册、心跳、失活接管）；
- 三级配额、优先级、租户公平调度、缓存亲和放置；幂等与构建合并；
- 日志分片上传 S3；产物预签名 URL；OIDC 联邦认证；
- Helm chart；审计日志。

**Phase 3 — 强隔离与供应链安全（约 8–12 周）**

- Firecracker microVM 执行后端：预热池 + 快照恢复（目标冷启动秒级、恢复毫秒级）；Kata 适配 K8s；
- 控制面签发 **in-toto/SLSA provenance**（cosign/Sigstore），对标 Build L3；
- 租户缓存 namespace 与中毒防护体系化（key 输入闭包、仅控制面写、验签）；
- 多区域/故障域、缓存层跨主机网络存储、容量规划与成本指标。

### 13.4 立项一句话定位

> **用 Rust 做「打包/构建后端」而不是「又一个 CI」**：单二进制 + SQLite 让小团队 5 分钟自托管，开放缓存协议（turbo v8 起步）让生态直接接入，Firecracker 强隔离 + SLSA L3 让多租户与公网服务可信，MVP 只做「提交 → 执行 → 看日志 → 取产物」一条直线。

---

## 14. 参考资料

- Vercel Blog：[A deep dive into Vercel's build infrastructure（Hive）](https://vercel.com/blog/a-deep-dive-into-hive-vercels-builds-infrastructure)
- Vercel Docs：[Managing Builds](https://vercel.com/docs/builds/managing-builds)；Vercel Blog：[Finishing Turborepo's migration from Go to Rust](https://vercel.com/blog/finishing-turborepos-migration-from-go-to-rust)
- Netlify Blog：[Your builds just got faster（Firecracker 新构建系统，2026）](https://www.netlify.com/blog/your-builds-just-got-faster/)
- Turborepo：[Remote Caching](https://turborepo.org/repo/docs/core-concepts/remote-caching)、[Remote Cache API / OpenAPI](https://turborepo.dev/docs/openapi)
- Nx：[Self-hosted remote cache（OpenAPI v1）](https://nx.dev/recipes/running-tasks/self-hosted-caching)、[Cache Security](https://nx.dev/ci/concepts/cache-security)
- GitHub Docs：[Actions Runner Controller](https://docs.github.com/en/actions/concepts/runners/actions-runner-controller)
- Docker Docs：[BuildKit](https://docs.docker.com/build/buildkit/)、[Architecture](https://docs.docker.com/build/architecture/)、[Cache backends](https://docs.docker.com/build/cache/backends/)、[buildkitd.toml](https://docs.docker.com/build/buildkit/toml-configuration)
- Docker Blog：[Merge+Diff: Building DAGs More Efficiently](https://www.docker.com/blog/mergediff-building-dags-more-efficiently-and-elegantly/)
- Temporal Docs：[Task Queue Priority and Fairness](https://docs.temporal.io/develop/task-queue-priority-fairness)、[Priority Task Queues](https://docs.temporal.io/design-patterns/priority-task-queues)、[Cancellation](https://docs.temporal.io/develop/rust/workflows/cancellation)
- Celery Docs：[Workers Guide — Revoking tasks](https://docs.celeryq.dev/en/v5.6.2/userguide/workers.html)
- SLSA：[Specification v1.1 Threats & mitigations](https://slsa.dev/spec/v1.1/threats)、[Mini Shai-Hulud: Where SLSA's Boundaries Fall（2026）](https://slsa.dev/blog/2026/05/mini-shai-hulud-what-slsa-can-and-cannot-do)
- 项目官网/仓库：[Woodpecker CI](https://woodpecker-ci.org/)、[Agola](https://agola.io/)、[Earthly Satellites](https://docs.earthly.dev/earthly-cloud/satellites/self-hosted)、[Actuated](https://actuated.com/)、[NativeLink](https://github.com/TraceMachina/nativelink)、[sccache](https://github.com/mozilla/sccache)、[cargo-remote](https://github.com/sgeisler/cargo-remote)、[turbo-cache-server](https://github.com/brunojppb/turbo-cache-server)、[moon](https://github.com/moonrepo/moon)、[cargo-dist](https://github.com/axodotdev/cargo-dist)
- Litestream：[Kubernetes Guide](https://litestream.io/guides/kubernetes/)
- 隔离对比参考：[Northflank: Kata vs Firecracker vs gVisor](https://northflank.com/blog/kata-containers-vs-firecracker-vs-gvisor)、[Onidel 2025 guide](https://onidel.com/blog/gvisor-kata-firecracker-2025)
