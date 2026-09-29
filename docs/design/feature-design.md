# Hotpot 功能设计文档

> 版本：v0.1（2026-09-29），对应 Hotpot 0.1.x / M0–M7。
> 本文逐项定义功能的**外部规格、交互流程、边界条件与异常处理**，并标注当前实现状态。
> 运行时结构见 [系统架构文档](system-architecture.md)，设计原则见 [总体架构设计](architecture.md)。

状态标记：✅ 已实现并实测；🟡 模型/接口预留，部分实现；🔮 规划中。

---

## 功能索引

| # | 功能 | Crate | 状态 |
|---|------|-------|------|
| F1 | 构建提交 | hotpot-api | ✅（local 来源） |
| F2 | 构建 Profile | hotpot-core | ✅ |
| F3 | 调度队列与租约 | hotpot-scheduler | ✅ |
| F4 | 构建执行器（local/docker） | hotpot-worker | ✅ |
| F5 | 事件模型与 SSE 日志流 | hotpot-core/api/scheduler | ✅ |
| F6 | 产物管理 | hotpot-api/scheduler/store | ✅ |
| F7 | sccache 兼容缓存 | hotpot-cacheproto | ✅ |
| F8 | Turborepo v8 兼容缓存 | hotpot-cacheproto | ✅ |
| F9 | 本地热重载开发循环 | hotpot-dev | ✅ |
| F10 | 零停机部署 supervisor | hotpot-agent | ✅（Unix） |
| F11 | 命令行客户端 | hotpot-cli | ✅ |
| F12 | 错误模型 | hotpot-core/api | ✅ |

---

## F1. 构建提交 ✅

### 功能说明

客户端通过 `POST /v1/builds` 提交一个 Cargo 项目的构建请求，服务端校验后入队。

### 请求/响应

- 请求：`{ "source": SourceSpec, "profile"?: BuildProfile }`
- 成功：`202 Accepted` + `BuildRecord`（已分配 `bld_<uuid>` 与 created_at_ms）
- 失败：`400/500` + `{ "error": "…" }`

### SourceSpec（tagged union，serde tag=`kind`）

| kind | 字段 | 状态 |
|------|------|------|
| `local` | `path: String` | ✅ |
| `git` | `url`, `ref_name`, `sha?` | 🟡 模型预留，提交返回 400 |
| `upload` | `upload_id`, `root?` | 🟡 模型预留，提交返回 400 |

### 边界校验规则（系统边界，fail fast）

1. local.path 必须存在且为目录，否则 400 `project path does not exist: …`；
2. 目录下必须有 `Cargo.toml` 文件，否则 400 `no Cargo.toml under: …`；
3. 非 local 来源一律 400 `source kind not supported yet: …`；
4. profile 缺省时使用默认值（debug、空 features），非法枚举/类型由 serde 拒绝（400）。

### 合并（coalescing）

入队前查询是否存在 **source_json 与 profile_json 完全相同** 的 `queued` 构建：
有则直接返回该已有构建（不新建、不报错）。仅合并排队态；已 dispatched 的构建不参与。

### 异常处理

- 数据库错误：映射为 500；
- 服务重启不影响已持久化的排队记录。

---

## F2. 构建 Profile ✅

### 字段（`BuildProfile`，全部可缺省）

| 字段 | 类型 | 默认 | 语义 |
|------|------|------|------|
| `mode` | `debug` \| `release` | `debug` | 映射 cargo `--release` |
| `features` | string[] | `[]` | 非空时加 `--features a,b` |
| `no_default_features` | bool | false | 加 `--no-default-features` |
| `target` | string? | null | 加 `--target <triple>`，并影响产物采集路径 |
| `cargo_flags` | string[] | `[]` | 原样追加的额外参数 |
| `toolchain` | string? | null | 预留（worker 默认工具链） |

### 行为约束

- cargo 参数由统一函数 `cargo_invocation(profile, …)` 生成，local 与 docker 后端共用；
- 执行环境固定附加 `CARGO_INCREMENTAL=0`、`CARGO_TERM_COLOR=never`
  （sccache 场景再附加 `RUSTC_WRAPPER` / `SCCACHE_DIR`）；
- profile 序列化为 JSON 参与合并判定与队列持久化，字段变更会被识别为不同构建。

---

## F3. 调度队列与租约 ✅

### 数据结构

`builds` 表一行一个构建；状态列存 JSON 引号标量（`'"queued"'` 等）。

### 认领（claim_next）

单条原子语句：

```sql
UPDATE builds SET status='"dispatched"', leased_by=?, leased_until_ms=?
 WHERE id = (SELECT id FROM builds
              WHERE status='"queued"'
                 OR (status='"dispatched"' AND leased_until_ms < ?)
              ORDER BY priority DESC, created_at_ms ASC LIMIT 1)
 RETURNING *
```

语义：

- FIFO（可被 priority 覆盖，当前全部写入 priority=0）；
- 同时认领 queued 任务与**租约过期的 dispatched 任务**（失活接管，at-least-once）；
- 无任务返回 None，worker 退避 500ms。

### 租约

- 认领时租约 60s；执行器每 15s `renew_lease`（WHERE 同时校验 id 与 leased_by）；
- `finish_build` 清空 leased_by/leased_until_ms。

### 取消传播

- `cancel_queued`：仅把 queued 行置 canceled（返回是否命中）；
- 未命中说明任务在执行 → 由 watch 通道通知执行器终止进程；
- 不抢占正常运行任务。

### 当前边界

- 无项目/全局并发配额、无 worker registry（worker 仅以内嵌形态存在）；
- 状态在执行期间保持 dispatched，未写 running。

---

## F4. 构建执行器 ✅

### 抽象

```text
BuildPlan { build_id, project_dir, target_dir, profile, timeout=1800s,
            sccache_dir?, sccache_bin?, cancel? }
ExecutorKind::Local | Docker { image, docker_host? }
run_build(plan, kind) → (mpsc::Receiver<BuildEvent>, JoinHandle<BuildResult>)
```

### 4.1 local ✅

- 在 project_dir spawn `cargo build [--release] [--no-default-features]
  [--features …] [--target …] [cargo_flags…]`；
- `CARGO_TARGET_DIR` 指向会话目录；stdin 丢弃，stdout/stderr 管道按行读取成事件；
- 三分支 `tokio::select!`：
  1. 进程退出：Completed + exit code + success；
  2. `timeout(plan.timeout)`：TimedOut；
  3. cancel 收到 true：Canceled；
- 非 Completed 时 start_kill 并 wait 回收，再发一条 phase 事件说明原因；
- cargo spawn 失败：SpawnFailed（不崩溃）。

### 4.2 docker ✅

- bollard 连接 daemon（可配 host，30s 连接超时）；镜像缺失自动 pull；
- 容器挂载：项目 → `/workspace`，会话 target → `/target`，sccache → `/sccache`；
- cargo 命令与环境与 local 完全一致（容器内 wrapper 用命令名而非宿主路径）；
- attach 输出按流解析为 stdout/stderr 事件；
- 结束/取消/超时后 kill，等待最多 10s 回收并 remove 容器；
- daemon 不可达、pull 失败、canonicalize 失败：均发 stderr 事件并返回 SpawnFailed。

### 共性规定

- 事件 seq 由 AtomicU64 在单构建内全局生成，stdout/stderr 交错仍有序；
- target 目录每构建独立，**绝不跨项目共享**。

---

## F5. 事件模型与 SSE 日志流 ✅

### BuildEvent

```text
{ build_id, seq, timestamp_ms, kind, payload }
kind: stdout | stderr | phase | status
```

- phase 事件由执行器在构建开始/终止时产生（`build started` / `build canceled` 等）；
- 事件持久化到 build_events，主键 (build_id, seq)，写入用 `INSERT OR REPLACE`。

### SSE 端点

`GET /v1/builds/{id}/logs/stream?since=N`

- 构建先查存在性，不存在 404；
- 每 400ms 批量拉取（≤512 条），即时逐条推送；event 名为 `stdout/stderr/phase/status`，
  data 为事件 JSON；
- 构建终态后发 `end`（data `{}`）并关闭；DB 错误发 `error` 后关闭；
- KeepAlive 默认帧；游标：服务端 `seq >= cursor`，处理器推进 cursor=seq+1；
  客户端重连 since=lastSeq+1，全量回放保证断连不丢日志。

---

## F6. 产物管理 ✅

### 采集（仅构建成功后）

- 目录：`sessions/<id>/target/[<triple>/]debug|release`；
- 仅顶层普通文件；跳过子目录、`.d` 文件、点开头隐藏文件；
- 每个文件：读入 → blake3 摘要 → CAS put → 记录 ArtifactMeta。

### ArtifactMeta

```text
{ name, digest(blake3 hex), size, attrs }
attrs: Unix 下 mode & 0o111 ≠ 0 → { "executable": "true" }
```

### 清单与下载

- `GET /v1/builds/{id}/artifacts` → ArtifactMeta[]（artifacts 表）；
- `GET /v1/artifacts/{digest}?filename=…`：
  - digest 非法 → 400；对象不存在 → 404；
  - 200 + `Content-Disposition: attachment[; filename="…"]`；
- CLI download：逐文件写出，按 attrs 恢复可执行位。

### 关键失败语义

- **采集/入 CAS 失败 → 构建整体 failed**（编译成功也不允许报 succeeded），
  error=`collect artifacts: …`；
- 产物不可变；同内容跨构建/跨类型自动去重。

### 已知边界 🟡

- 暂不支持自定义 ArtifactGlob（模型中提及，采集规则固定）；
- 不做 tarball 打包与 manifest 增量拉取（规划）。

---

## F7. sccache 兼容缓存 ✅

### 端点

单 any 入口：`/sccache/{*key}`（另注册 `/sccache`、`/sccache/` 集合根）。

### 方法语义

| 方法 | 响应 |
|------|------|
| GET | 200 + octet-stream（长度正确）；miss 404 |
| HEAD | 200 / 404 |
| PUT | 204；同 (ns, tenant='', key) 重复写幂等 |
| MKCOL | 201（虚拟集合，不落盘） |
| PROPFIND | 207 multistatus：含 resourcetype（集合带 collection）、getlastmodified、getcontentlength |
| OPTIONS | 200 |
| 其他 | 405 |

### 设计要点

- opendal 写前必发 PROPFIND/MKCOL；空响应体会触发其反序列化错误并**降级只读**，
  因此两个方法必须返回合法成功响应；
- 载荷不透明（sccache 的 zstd ZIP），仅按 client key 索引、内容寻址存储；
- 索引悬挂（CAS 对象丢失）时 GET 自动清理索引按 miss 处理。

---

## F8. Turborepo v8 兼容缓存 ✅

### 端点

| Method | Path |
|--------|------|
| GET | `/v8/artifacts/status` → `{"status":"enabled"}` |
| GET/HEAD/PUT | `/v8/artifacts/{hash}` |

### 租户隔离

- 查询参数 `teamId` 或 `slug`（兼容 turbo 的两种参数），取值作为 tenant；
  二者皆无 → 默认空租户；不同租户完全隔离。

### Tag 透传

- PUT 读 `x-artifact-tag` 请求头持久化；
- GET 命中时以同名响应头原样回传（turbo 签名 artifact 需要）。

### 状态码

PUT 201；GET 命中 200、miss 404；HEAD 200/404；内部失败 500。

---

## F9. 本地热重载开发循环 ✅

### 命令

`hotpot-dev [PROJECT] [--release] [--features …] [--addr H:P] [--env K=V …]`

### 流程

```text
1. watcher 递归监听项目（先于初始构建注册！），200ms 防抖
2. （可选）启动 socket keeper 于公共端口，选随机 loopback 端口作后端，
   注入 HOTPOT_BIND_ADDR
3. 初始 cargo build（增量，使用项目自身 target）；失败 → 直接退出
4. cargo metadata 解析首个 bin target 路径并 spawn，stdio 直通
5. 排空构建期间累积的变更：有则立即补一次重建（保证与磁盘一致）
6. 变更到达 → 重建：
     失败：保留旧进程，打印诊断尾部（最后 40 行）
     成功：SIGTERM 旧进程（5s 兜底 SIGKILL）→ spawn 新二进制
7. Ctrl-C：停止子进程后退出
```

### 变更过滤

仅 `.rs` / `Cargo.toml` / `Cargo.lock`；路径含 `target` / `.git` 段则忽略。

### Socket keeper

- 公共端口由独立 task 常驻 accept，后端重启期间端口不消失；
- 连接后端时在 10s 就绪窗口内每 50ms 重试；窗口内未连上回 HTTP 503；
- 连上后双向透传字节流。

### 当前边界

- 未接入 hot-lib-reloader 的 dylib 内存热换（模型设计中的 L2 热换，当前整进程重启）；
- 要求项目存在 bin target（由 cargo metadata 解析，不假设固定布局）。

---

## F10. 零停机部署 supervisor ✅（Unix）

### 组成

- `hotpot-agent run`：supervisor 常驻；
- 控制面：data-dir 下 AF_UNIX `agent.sock`，单行 JSON 命令
  （deploy/rollback/status/shutdown），CLI 子命令是其瘦客户端。

### 版本固化

- deploy 时把外部二进制复制到 `versions/<version>`（`/` 替换为 `_`）并 chmod 可执行；
- Release = { version, path, env(BTreeMap) }；deploy 可替换整套环境变量。

### 状态机（point-of-no-return = Drain）

```text
Fetch      stage 副本；phase=preflighting；生成 deploy_id
Preflight  spawn 子进程（dup2 注入 fd3/4/5, HOTPOT_LISTEN_FDS=1）
           轮询探针 /healthz：200=warm；503=预热中；15s 超时失败
Arm        控制管道写 ARM，收子进程回 ARMED 后真实端口开始接流
Attach     再确认一次健康
Drain      SIGTERM 旧版本；8s 窗口；wait_or_kill 兜底 SIGKILL
Commit     写 COMMIT（子进程关探针）；current/previous 交换；phase 经 committed 回 idle
```

### 失败语义矩阵

| 失败点 | 行为 |
|--------|------|
| Preflight 超时/子进程崩溃 | SIGKILL 新进程，phase=failed，**旧版本继续**，命令返回错误 |
| Arm 失败/异常应答 | 同上（杀新保旧） |
| Attach 复检失败 | 杀新保旧 |
| Drain 中新版本死亡 | 自动把旧版本经同一状态机 deploy 回来；报错 |
| 旧版本拒退 | 8s 后 SIGKILL，继续 Commit |

### 崩溃恢复

- state.json 在每个 phase 原子落盘（tmp+rename）；
- supervisor 重启：bind 控制 socket（先于 bootstrap）→ install_initial（仅首次）
  → bootstrap（对 current 走 Preflight→Arm→Commit）；
- 初始版本只登记不启动，杜绝双 spawn 幽灵进程。

### 应用接入契约（fd 固定布局）

| fd | 资源 | 时序职责 |
|----|------|----------|
| 3 | 真实 listen socket | ARM 后才 accept；SIGTERM 后停 accept、排空在飞请求 |
| 4 | 探针 socket | warm 前 /healthz 返 503，warm 后 200；COMMIT 后关闭 |
| 5 | 控制管道 | 收 ARM 回 ARMED；收 COMMIT 关探针 |

`hotpot_agent::activate` 提供参考实现，`echo_app` bin 为可运行样例。

### Rollback

= 对 `state.previous` 复用 deploy 全流程；无 previous 时返回错误
（`no previous release to roll back to`，CLI 退出码 1）。

---

## F11. 命令行客户端 ✅

### 全局

`--server` / `HOTPOT_SERVER`（默认 `http://127.0.0.1:7878`）。

### 子命令规格

| 命令 | 关键参数 | 退出码 |
|------|----------|--------|
| build | -p（必填，自动 canonicalize）、--release、--features、--no-follow | 成功 0；非成功终态 1 |
| logs | id、--since | failed 时 1 |
| cancel | id | 传输/处理失败 1；终态幂等 0 |
| status | id | — |
| artifacts | id | — |
| download | id、-o（默认 ./hotpot-out） | — |

### 实现约定

- HTTP 客户端 reqwest + rustls（不依赖系统 OpenSSL）；
- SSE 响应手写帧解析（`sse` 模块），遇 end 结束、error bail；
- 非 2xx 响应解析 `{error}` 文本作为错误信息；解析失败则用状态码文本；
- download 按 attrs 恢复可执行位。

---

## F12. 错误模型 ✅

### 服务端

`hotpot_core::Error`（thiserror）覆盖：无效参数、摘要不匹配、存储错误、其他内部错误；
API 层 `ApiError` 映射为状态码 + JSON：

| HTTP | 触发条件 |
|------|----------|
| 400 | 非法输入、不支持的 source、非法 digest/id |
| 404 | 构建/产物不存在 |
| 500 | DB、CAS、内部状态错误 |

构建 ID 解析容忍带/不带 `bld_` 前缀；非法 UUID → 400。

### 构建级错误（记录在 BuildRecord.error）

| 场景 | 终态 |
|------|------|
| cargo 返回非零 | failed（`cargo build failed`） |
| 超时（1800s 默认） | timeout |
| 收到取消 | canceled |
| 不支持的 source（执行期） | failed（unsupported source: …） |
| 产物采集失败 | failed（collect artifacts: …） |

### 不吞错误原则

- worker 执行/认领错误均 warn 日志并继续循环；
- SSE 拉取失败显式发 error 帧；
- 子进程 spawn 失败作为 BuildResult 返回而非 task panic。

---

## 附：功能 → 里程碑对照

| 里程碑 | 交付功能 |
|--------|----------|
| M0 | workspace、hotpot-core、LocalStore CAS |
| M1 | local 执行器真实 cargo 构建 |
| M2 | Scheduler + API（提交/状态/SSE/取消/产物） |
| M3 | CLI 全命令 |
| M4 | F7 sccache + F8 Turbo 缓存协议 |
| M5 | F10 hotpot-agent 零停机部署 |
| M6 | F9 hotpot-dev 热重载开发 |
| M7 | docker 执行器、compose 发行、CI、文档 |
