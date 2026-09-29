# 构建耗时基线（本机实测）

机器：macOS / Apple Silicon (aarch64)，Rust 1.98.0
样本：`examples/demo-webapp`（axum + tokio + serde + clap 等，约 200 个传递依赖）

## Baseline：裸 cargo（2026-09-28）

| 场景 | 耗时 |
|------|------|
| 冷构建 debug（target 清空，registry 已预热） | 11.2s |
| 冷构建 release（默认 release，无 LTO） | 13.0s |
| 首次冷构建（含依赖下载） | 47.9s |
| 空操作 cargo build | 1.6s |
| touch 源码（内容未变） | 1.05s |
| 改一行源码后增量 | 2.35s |

## 关键观察

1. 依赖下载是冷启动最大头（47.9s 中超过一半），稀疏 registry + git 缓存价值大。
2. 本机小型项目依赖编译 11s 量级；中大型项目（500+ 依赖）在 CI 上常见
   3–10 分钟。远程缓存命中后这些全部消失。
3. cargo 自身 fingerprint 可靠（touch 不触发重编），但粒度是 crate 级，
   一行改动也要重编整个 crate + 链接，约 2s（链接占大头）。

## M1：hotpot-worker 执行器（2026-09-28）

会话级独立 CARGO_TARGET_DIR，`CARGO_INCREMENTAL=0`，事件经 mpsc 流式采集。

| 场景 | 耗时 | 事件 |
|------|------|------|
| 冷构建 debug（新会话目录） | 9.44s | 94 条（1 phase + 93 cargo stderr 行） |
| 暖构建（复用会话目录） | 0.61s | 2 条 |
| 冷构建 release | 12.86s | 94 条 |

观察：执行器冷构建与裸 cargo 基线基本一致（9.4s vs 11.2s，差异在机器波动/
registry 状态），说明进程封装与事件采集零额外开销；后续加速应全部来自缓存层
而非执行器本身。

## M2：HTTP 服务全链路（2026-09-28）

hotpot-server（单二进制，2 个内嵌 worker，SQLite WAL）。

| 场景 | 结果 |
|------|------|
| POST /v1/builds（local，debug） | 202，返回 queued 记录 |
| SSE 日志 | 96 个事件（94 cargo + phase + end），`since` 可续传 |
| 构建状态 | succeeded，build_ms≈8500（与裸 cargo 冷构建一致） |
| 产物 | demo-webapp 15.5MB，blake3 入 CAS，executable attr |
| 下载产物 | 二进制下载后实际启动，notes API 正常 |
| 运行中取消 release | 2s 时 cancel → 最终 status=canceled，SSE 正常结束 |

## M3：hotpot CLI（2026-09-28）

| 命令 | 结果 |
|------|------|
| `hotpot build -p`（默认 follow） | 提交后 SSE 跟踪至 succeeded，exit 0 |
| 失败构建 | CLI 输出编译错误后 exit 1 |
| `hotpot logs <id> --since N` | 游标续传，canceled 构建也能完整回放 |
| `hotpot cancel/status/artifacts` | 状态与服务端一致 |
| `hotpot download -o` | 产物落地，可执行位按 attrs 保留，二进制可运行 |

注：本机在后台研究 agent 负载时，服务构建耗时在 11.7–15.4s 间波动，
缓存层（M4）落地后以固定机器重测。

## M4：远端缓存协议（2026-09-28）

hotpot-cacheproto 挂载于 hotpot-server，索引为 SQLite `kv_entries`，
对象复用本地 CAS（blake3 去重）。

### sccache（WebDAV，opendal）

demo-webapp 全量依赖，89 个 Rust crate，SCCACHE_DIR 每次清空。

| 场景 | 耗时 | 结果 |
|------|------|------|
| 冷构建 + 写穿远端 | 11.8s | 89 misses，0 write errors，68.7MB 入 CAS |
| 清 target 重建 | 4.5s | 89 hits |
| **全新 SCCACHE_DIR + 清 target**（纯远端命中） | 4.2s | **89 hits / 0 miss，命中率 100%** |

关键兼容点：opendal 写前依次 PROPFIND（必须返回带 `getlastmodified`
的 207 multistatus，空体会把存储降级只读）→ MKCOL（201）→ PUT（204），
服务端对集合路径做虚拟应答，客户端无需 `disable_create_dir`。

### Turborepo v8（真实客户端，npm 安装的 turbo latest）

| 场景 | 结果 |
|------|------|
| `turbo build`（cold） | miss 执行，gzip-tar（156B）PUT 至 /v8/artifacts/{hash} |
| 删除全部本地缓存后 `turbo build --cache=remote:rw` | **1 cached，FULL TURBO，32ms**，日志完整回放 |
| 租户隔离 | teamId/slug 不匹配一律 404（serde rename teamId） |
| x-artifact-tag | PUT 写入后 GET 原样回显 |

## M5：生产零停机部署（2026-09-28）

hotpot-agent：supervisor 常驻持有 listen socket，子版本经 fd 交接对外服务。

| 场景 | 结果 |
|------|------|
| v1 服务中压测并发连接，同时 deploy v2 | **0 连接被拒/中断**，v1、v2 各自的请求均收到对应版本响应 |
| `rollback` | 回到上一版本，同一状态机反向执行 |
| 初始版本启动 | 修复了 supervisor 与子进程双 spawn 的幽灵进程缺陷（端口竞争） |

## M6：本地热重载开发循环（2026-09-29）

hotpot-dev：notify（macOS FSEvents）200ms 防抖，socket keeper 常驻公共端口，
子进程经 `HOTPOT_BIND_ADDR` 绑定后端。验收测试 `tests/dev_loop.rs` 实测。

| 场景 | 结果 |
|------|------|
| 改函数体 → 增量重建 + 重启完成 | **325ms**（保存到新版本对外响应） |
| 重建期间持续 TCP 连接探测（4s） | **0 次 refused**（keeper 代理等待后端 ready） |
| 写入无法编译的代码 | 构建失败，**旧进程继续服务**；错误尾部打印到控制台 |
| 修复后 | 自动重建收敛到新版本 |
| 启动后立刻编辑 | watcher 在初始构建**之前**注册 + 启动后收敛待处理变更，无丢事件 |

关键缺陷修复：FSEvents 的 watch 注册是异步的，若先构建后注册 watcher，
紧随启动的一次编辑可能落在注册空窗里永久丢失。

## M7：Docker 执行器与发行（2026-09-29）

hotpot-worker docker executor（bollard）：项目/target/sccache 三个 bind
挂载，容器内产物经 target 挂载直接回到宿主。镜像 `rust:1.85-slim-bookworm`
（已预拉），colima VM，临时目录在 `$HOME/.hotpot-tmp`（VM 挂载范围内）。

| 场景 | 结果 |
|------|------|
| `docker_live` 3 测试总耗时 | **15.2s**（成功 / 编译失败 / 取消） |
| 构建成功 | debug 二进制出现在宿主 `session/target/debug/fixture` |
| 编译失败 | 事件含真实 rustc 诊断（`error[E…]`），非挂载空目录假象 |
| 取消 | build.rs 死循环场景 cancel 后 EndReason::Canceled，容器被 SIGKILL 回收 |
| compose 端到端冷构建 | 3m13s（空 CARGO_HOME：下载 + 编译约 200 依赖），产物 58MB Linux aarch64 |
| 产物下载后实测 | 于 debian:bookworm-slim 容器内启动，notes GET/POST 正常 |
| 发行 | `docker compose up -d`（兄弟容器同路径挂载）；GitHub Actions 三 job CI |

（后续 Hotpot 的所有加速效果都以该基线为对照记录。）
