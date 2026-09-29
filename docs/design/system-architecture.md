# Hotpot 系统架构文档

> 版本：v0.1（2026-09-29），对应 Hotpot 0.1.x / M0–M7 已实现系统。
> 本文从**实现视角**描述运行时结构、持久化、并发控制与故障恢复；
> 设计原则与决策依据见 [总体架构设计](architecture.md)，功能级规格见 [功能设计](feature-design.md)。

## 1. 架构总览

```text
                     ┌────────────────────────────────────────────┐
   hotpot CLI  ───▶  │              hotpot-server (单进程)         │
   curl / CI         │                                            │
   sccache/opendal ─▶│  axum Router                                │
   turbo client   ──▶│   ├─ /v1/*        → build routes (AppState) │
                     │   └─ /sccache /v8 → cacheproto (CacheState) │
                     │                  (merge 为 Router<()>)      │
                     │                                            │
                     │  tokio tasks:                              │
                     │   ├─ N × embedded worker loop              │
                     │   └─ (per build) lease renew task           │
                     └───┬───────────────┬───────────────┬────────┘
                         │               │               │
                  ┌──────▼──────┐ ┌──────▼──────┐ ┌──────▼──────────┐
                  │ hotpot.db   │ │cacheproto.db│ │  store/ (CAS)   │
                  │ builds      │ │ kv_entries  │ │  objects 256桶  │
                  │ build_events│ │  (key→CAS)  │ │  blake3 + zstd  │
                  │ artifacts   │ └─────────────┘ │  tmp/ LRU       │
                  └──────────────┘                └─────────────────┘
                         │
                  ┌──────▼──────────────────────────────────────┐
                  │ sessions/<build-id>/target (CARGO_TARGET_DIR)│
                  └─────────────────────────────────────────────┘
                         │ local: spawn cargo
                  ┌──────┴──────────────────────────────────────┐
                  │ docker: 兄弟容器（bollard, bind mounts）     │
                  │  /workspace ← 项目  /target ← 会话 target     │
                  └─────────────────────────────────────────────┘

  独立链路（不在服务进程内）:
   hotpot-dev   本地开发循环：watcher + cargo + socket keeper
   hotpot-agent 部署 supervisor：持有 listen socket + 部署状态机
```

单体内模块化：所有组件在同一 Cargo workspace 内以 crate 划分。
`hotpot-server` 只是**部署形态 A**（api+scheduler+worker 同进程），拆分时无需改动领域代码。

## 2. Cargo workspace 与依赖方向

```text
hotpot-core ◀──── hotpot-store
     ▲                  ▲
     ├──── hotpot-scheduler
     ├──── hotpot-worker ◀──── hotpot-api ◀──── bin: hotpot-server
     ├──── hotpot-cacheproto
     ├──── hotpot-cli
     ├──── hotpot-agent   (独立 bin/lib)
     └──── hotpot-dev     (独立 bin/lib)
```

| Crate | 关键依赖 | 产物 |
|-------|----------|------|
| `hotpot-core` | serde, blake3/sha2, uuid, chrono, thiserror | lib：模型/摘要/ID/错误 |
| `hotpot-store` | zstd, walkdir | lib：`BlobStore` trait + `LocalStore` |
| `hotpot-scheduler` | sqlx (SQLite/WAL) | lib：`Scheduler` |
| `hotpot-worker` | tokio, bollard | lib + bin：执行器 |
| `hotpot-cacheproto` | axum, sqlx | lib：路由 + `RemoteCache` |
| `hotpot-api` | axum, tower-http, reqwest(间接) | lib + bin `hotpot-server` |
| `hotpot-cli` | clap, reqwest (rustls) | bin `hotpot` |
| `hotpot-agent` | libc, clap, serde | bin `hotpot-agent` + lib |
| `hotpot-dev` | notify, clap, tokio | bin `hotpot-dev` + lib |

## 3. 运行时模型

### 3.1 hotpot-server 进程

- 异步运行时：Tokio multi-thread（`tokio = full`）；
- 启动顺序：

  ```text
  1. 初始化 tracing（EnvFilter，默认 info）
  2. create_dir_all(data_dir)
  3. Scheduler::open(data_dir)          → hotpot.db（建表、WAL）
  4. LocalStore::open(data_dir/store)  → CAS
  5. RemoteCache::open(data_dir, store)→ cacheproto.db
  6. spawn N × worker_loop（默认 min(available_parallelism, 4)）
  7. Router::new().merge(build router).merge(cacheproto router)
        .layer(TraceLayer) → axum::serve(TcpListener::bind(listen))
  ```

- 路由器 merge：build 路由状态为 `AppState`，缓存路由状态为 `CacheState`，
  各自 with_state 后合并为 `Router<()>`，共享同一监听端口。

### 3.2 worker 循环

每个内嵌 worker 是一个常驻 tokio task：

```text
loop {
  claim_next(worker_id, lease=60s)
    Some(rec) → handle_build(rec)   // 串行：一个 worker 同时只跑一个构建
    None      → sleep 500ms
    Err       → 记日志后 sleep 500ms
}
```

`handle_build` 内部并发结构：

```text
register_cancel(build_id)          → watch::Receiver<bool>
spawn lease-renew task             → 每 15s renew_lease
run_build(plan, executor)          → (mpsc::Receiver<BuildEvent>, JoinHandle<BuildResult>)
loop: events.recv() → append_event 逐事件持久化
await JoinHandle（构建结束）
abort renew task; remove_cancel
成功 → collect_artifacts → add_artifacts
finish_build(终态, timings, error)
```

### 3.3 线程模型

- 构建事件序号 `seq` 由执行器内两个 stdout/stderr 读取 task 通过
  `AtomicU64::fetch_add` 生成（Relaxed 序），单构建内全局单调；
- docker.rs 的 bollard 调用全部在 tokio task 内；
- hotpot-dev 的 watcher 防抖在独立 OS 线程中运行（std mpsc → tokio mpsc 桥接）；
- hotpot-agent 的控制循环为单线程阻塞轮询（非 async），supervisor 逻辑直接、确定。

## 4. 关键流程时序

### 4.1 提交 → 执行 → 完成

```text
CLI/API               Scheduler(SQLite)        Worker              Store/CAS
  │ POST /v1/builds      │                       │                    │
  │ validate source ─────┼───────────────────────┼────────────────────┤
  │ enqueue(record) ─────▶ 查 queued 同 source/  │                    │
  │                      │ profile：有则合并返回 │                    │
  │ 202 BuildRecord ◀────┘ INSERT builds         │                    │
  │                      │ claim_next ◀──────────│ UPDATE...RETURNING │
  │                      │  dispatched+租约60s ─▶│                    │
  │                      │        (每15s renew)──│ renew_lease        │
  │                      │ append_event ◀────────│ stdout/stderr 行   │
  │ SSE 400ms 轮询 ──────▶ list_events ──────────│                    │
  │ frame(event) ◀───────┘                       │ cargo 结束         │
  │                      │ collect_artifacts ────┼──put(blake3)──────▶│
  │                      │ add_artifacts ◀───────┤                    │
  │                      │ finish_build ◀────────┤ 终态+timings       │
  │ SSE end 帧后关闭 ◀────┘                       │                    │
```

### 4.2 取消

```text
POST /cancel → cancel_queued(id)
               rows_affected=1（排队中）   → 直接 canceled
               0（非排队）→ signal_cancel(id)
                 watch::send(true) → 执行器 select! 命中取消分支
                   local: start_kill + wait
                   docker: kill 容器，等回收窗口，remove 容器
               → finish_build(canceled)
```

### 4.3 远程缓存写入（sccache PUT / turbo PUT）

```text
client PUT → Cache handler → RemoteCache.put(ns, tenant, key, bytes, tag)
   lookup 同 (ns,tenant,key) 已存在 → 幂等返回
   store.put(bytes) → CAS digest
   INSERT kv_entries（digest/size/tag）
sccache: 204    turbo: 201
```

读取时若 CAS 对象丢失（索引悬挂），删除该索引并按未命中处理（404）。

## 5. 持久化设计

数据目录（默认 `./hotpot-data`）：

```text
hotpot.db (+wal/+shm)   构建控制面
cacheproto.db (+wal)    缓存索引
store/objects/<xx>/<62hex>   CAS 对象文件
store/tmp/                   写入临时目录
sessions/<build-id>/target   会话 target
```

### 5.1 SQLite 配置（两库一致）

- `journal_mode = WAL`、`synchronous = NORMAL`、`busy_timeout = 5s`；
- `create_if_missing`；启动时执行幂等 `schema.sql`（`CREATE TABLE IF NOT EXISTS`）。

### 5.2 hotpot.db

```sql
builds(id PK, project_id, source_json, profile_json, status,
       timings_json DEFAULT '{}', created_at_ms, started_at_ms,
       finished_at_ms, error, leased_by, leased_until_ms, priority DEFAULT 0)
build_events(build_id, seq, timestamp_ms, kind, payload,
             PRIMARY KEY(build_id, seq))
artifacts(build_id, name, digest, size, attrs_json DEFAULT '{}',
          PRIMARY KEY(build_id, name))
INDEX idx_builds_claim(status, priority, created_at_ms)
INDEX idx_build_events_build(build_id, seq)
```

**JSON 引号约定**：`status` 与 `kind` 列存的是**带引号的 JSON 标量**
（如 `'"queued"'`、`'"stderr"'`），枚举序列化/反序列化均经 serde JSON；
source/profile/timings/attrs 直接存 JSON 文档。这样避免枚举与裸字符串的双重表示，
SQL 中比较状态时必须写 `status = '"queued"'`。

### 5.3 cacheproto.db

```sql
kv_entries(namespace, tenant DEFAULT '', cache_key, digest, size, tag,
           created_at_ms, PRIMARY KEY(namespace, tenant, cache_key))
INDEX idx_kv_lookup(namespace, tenant, cache_key)
```

### 5.4 SSE 游标语义

`list_events(id, after_seq, limit)` 返回 **`seq >= after_seq`** 的事件
（SQL 用 `>=` 而非 `>`）。SSE 处理器每收到一批就推进 `cursor = maxSeq + 1`，
因此「DB 含 ≥ 语义 + 调用方 +1」组合保证不重不漏；客户端续传传 `since = lastSeq+1`。

## 6. 内容寻址存储与缓存体系

### 6.1 LocalStore

- key：`blake3(bytes)` 的 64 hex；路径 `objects/<前2位>/<后62位>`，天然 256 桶分片；
- 写入：先写 `tmp/<digest>.tmp` 再原子 `rename`，杜绝半写对象；同 digest 已存在直接返回；
- 文件格式：首字节 flag，`0`=RAW、`1`=zstd；
  - ≥128 字节且非已压缩内容（gzip/zstd/zip/png/CAB 魔数启发式）才压缩，zstd level 3；
- 读取：解码后重新计算 blake3 校验，不符返回 `DigestMismatch`；
- LRU GC：配置 `max_bytes` 时，写后若超限，按**创建时间**（rename 保留 birth time；
  atime 有 relatime/lazy 语义不可靠，已踩坑）升序删除直到达标，
  同时间按路径确定性打破平局。

### 6.2 三级缓存的落地映射

| 层级 | 载体 | 现状 |
|------|------|------|
| L0 物料 | 构建主机环境（docker 镜像内 cargo registry；执行器挂载点） | 工具链镜像固化；会话 target 独立 |
| L1 crate 编译 | `RemoteCache`（sccache namespace）→ CAS | ✅ sccache/opendal WebDAV 客户端实测 100% 远端命中 |
| L2 产物 | `LocalStore` CAS + `artifacts` 表 | ✅ blake3、可执行位属性、去重 |

Turbo namespace 与 sccache 共用同一 CAS 但索引相互独立。

### 6.3 产物采集规则

- 根目录：`sessions/<id>/target/[<triple>/]debug|release`；
- 仅取**顶层普通文件**：跳过子目录、`.d` 依赖文件、隐藏文件；
- 属性：Unix 下文件 mode 含任一可执行位（`& 0o111`）则 `attrs.executable = true`。

## 7. 并发控制

| 机制 | 实现 | 保证 |
|------|------|------|
| 任务认领 | 单条 `UPDATE ... WHERE id = (SELECT ...) RETURNING *` | SQLite 写锁串行化，多 worker 只有一个成功 |
| 执行租约 | `leased_by` + `leased_until_ms`，60s | worker 失活后任务可被接管（at-least-once） |
| 租约续期 | 每 15s `UPDATE ... WHERE id AND leased_by` | 长构建不被误接管；worker 名不匹配则续不上 |
| 取消信号 | `watch::channel<bool>` per running build | 信号与构建生命周期绑定，结束即移除 |
| 事件序号 | 执行器内 AtomicU64 | stdout/stderr 两 task 合并后仍单构建单调 |
| 状态落盘 | agent：tmp + rename；store：tmp + rename | 崩溃不留半写文件 |

并发上限：worker 总数即同时执行构建数；默认 min(CPU,4)，可 `--workers` 调整。

## 8. Docker 执行拓扑

采用**兄弟容器（sibling containers）**而非 docker-in-docker：

```text
hotpot-server 容器 ──挂载── /var/run/docker.sock
   │ bollard 请求 daemon
   ▼
daemon 在宿主上创建构建容器（rust:1.98-slim-bookworm）
   bind mounts（全部用宿主绝对路径）:
     <project>        → /workspace
     <session/target> → /target
     <sccache dir>    → /sccache（如启用）
```

关键正确性条件：

1. 宿主路径必须真实存在且已挂载进 daemon VM（colima/Docker Desktop 的 VM 挂载范围）；
2. 服务容器内该路径与宿主**绝对路径一致**（compose 用同一字符串双挂载），
   构建容器写回 target 的产物才能被服务进程读到；
3. 容器内执行与本地执行共用 `cargo_invocation()` 生成 cargo 参数与环境变量，
   采集路径完全一致，两种后端行为对齐。

容器生命周期：连接 daemon（30s 超时）→ 确保镜像（自动 pull）→ create/attach →
执行 → kill/等待回收（10s）→ remove。

## 9. 故障模型与恢复

| 故障 | 检测 | 恢复行为 |
|------|------|----------|
| Worker panic / handle_build 出错 | worker_loop 捕获错误日志 | 循环继续；若租约未释放，60s 后被其他 worker 认领重跑 |
| Worker 整个失活（进程/机器挂） | `leased_until_ms < now` | claim_next 可认领过期 dispatched 任务（at-least-once） |
| 长构建被误接管 | 15s 续期失败（worker 仍活） | 仅日志告警；租约在则不会被接管 |
| CAS 对象损坏/被外部删除 | get 重算 digest 不符 / 读取 NotFound | DigestMismatch 报错；缓存索引悬挂时自动删索引按 miss 处理 |
| 构建成功但产物采集失败 | collect_artifacts Err | 构建**显式置 failed**（不允许假成功），error 写明原因 |
| SQLite 忙锁 | busy_timeout 5s | sqlx 自动等待；仍失败则上层返回错误并告警 |
| agent 在部署中途崩溃 | state.json 每阶段落盘 | 重启后 bootstrap 拉起 current；point-of-no-return 前失败已保旧版本 |
| Drain 期间新版本死亡 | drain 后 is_alive 检查 | 自动把旧版本重新 deploy 回来并报错 |
| 旧版本不响应 SIGTERM | wait_or_kill 8s | SIGKILL 兜底后继续 Commit |
| agent 控制 socket 启动空窗 | bind 在 bootstrap 之前 | 消除竞态：控制面先就位再接流量 |

**at-least-once 含义**：认领不排除 worker 在认领后、执行前崩溃，
另一 worker 会在租约过期后重跑同一构建。构建副作用（写 CAS）幂等，
但调用方不应假设构建业务副作用恰好一次。

## 10. 安全边界

当前（M0–M7）：

- **local 执行器**：构建以服务进程相同权限直接执行 cargo。build.rs/proc-macro 可执行任意代码，
  仅适合可信内网与可信源码；
- **docker 执行器**：构建在工具链容器内，与服务环境隔离，但共享 daemon socket
  （daemon 等价宿主 root，服务容器因此不适合暴露给不可信用户）；
- **输入校验**：边界检查路径存在性与 Cargo.toml；仅接受 local 来源；
- **API 无鉴权**：PAT/OIDC 在设计中预留，部署时请置于内网或加反向代理鉴权；
- **缓存隔离**：turbo 按 teamId/slug 做租户 key 隔离；sccache 按 key 路径；
  缓存仅经控制面写。

红线（沿用总体设计）：key 必须覆盖输入闭包；会话 target 绝不跨项目共享；
签名密钥不出控制面。

## 11. 可观测性

现状：

- `tracing` + `tracing-subscriber`（EnvFilter/RUST_LOG）；HTTP 层挂 `TraceLayer`；
- 构建关键节点结构化日志（worker、build id short、mode、status、artifact 数）；
- 事件表本身构成持久化构建日志（SSE 可任意回放）。

设计预留（未接线）：OTel trace/span（workspace 已含 opentelemetry 依赖）、
命中率与队列延迟指标、in-toto provenance。

## 12. 部署形态演进

| 形态 | 组成 | 适用 |
|------|------|------|
| A（当前） | 单二进制 hotpot-server + SQLite + 本地盘/LocalStore，内嵌 worker | 小团队 / 单机自托管 |
| A′（当前） | 同 A，`--executor docker` + compose，兄弟容器执行 | 单机隔离执行 |
| B | RemoteCache/S3 分层（object_store 已在依赖中）+ 独立 worker 进程 | 多机、共享缓存 |
| C | api/scheduler/worker 角色分离 + Postgres（sqlx 已抽象） | 高并发 |
| D | K8s + Helm + Kata/Firecracker + SLSA L3 | 多租户/不可信构建 |

## 13. 关键设计取舍记录

1. **SQLite + 单二进制先行**：零外部依赖，`UPDATE...RETURNING` 认领足够支撑小团队；
2. **枚举 JSON 引号存储**：宁要 SQL 里的笨拙引号，不要裸字符串与枚举两套表示；
3. **LRU 用 birth time 不用 atime**：atime 在 APFS/Linux 上语义不可靠（实测顺序反转）；
4. **WebDAV 集合虚拟化**：MKCOL/PROPFIND 直接成功而不落盘，避免 opendal 写前探测失败降级只读；
5. **agent 单线程控制循环**：部署流程确定性优先于并发，状态每阶段持久化；
6. **dev watcher 先于构建注册**：初始构建耗时覆盖异步注册窗口，杜绝首编期间丢事件。
