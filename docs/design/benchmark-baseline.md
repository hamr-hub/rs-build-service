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

## M8：协议加固与能力补齐（2026-09-29）

M0–M7 记录的是「功能是否存在」；M8 记录的是「协议语义是否正确、失败是否可见」。

### 8.1 协议契约测试（16 项，此前为 0）

`crates/hotpot-cacheproto/tests/routes.rs` 在 HTTP 层锁死两套协议的语义。
此前只有 `RemoteCache` 的存储层测试，**协议兼容的失败模式全是静默的**，
没有回归网。

| 契约 | 为什么重要 |
|------|-----------|
| sccache `.sccache_check`：`GET`→404 / `PUT`→204，且不落盘 | PUT 非 2xx 会让 sccache **静默降级只读**，远端写入全丢且不报错 |
| sccache 三层分片 key `ab/cd/<60 hex>` 往返 | key 归一化（如把 `/` 换成 `_`）会**静默破坏**全部历史条目 |
| sccache `Content-Length` 与 body 严格一致 | sccache 把长度参与签名计算，不符即验签失败 |
| sccache `PROPFIND`→207 + `getlastmodified` + `href` XML 转义 | opendal 靠该元素存在性判断解析成功；非法 XML → 写前探测失败 → 只读 |
| turbo `x-artifact-tag` 必回显 | 签名客户端缺 tag 是**硬错误**（`ArtifactTagMissing`），不是 miss |
| turbo 已有条目补齐缺失 tag | 「先无签名上传、后启用签名」会导致该客户端永久硬错误 |
| turbo 租户隔离：不同 `teamId`/`slug` 一律 404 | 404 是**唯一** miss 信号；403/500 会让 turbo 中断构建 |
| turbo `HEAD` 返回真实 `Content-Length` | spec 为 HEAD 200 声明了该头 |
| turbo `OPTIONS` 预检 | `--preflight` 下 405 会让预检失败 |

### 8.2 实测：缓存端点鉴权

服务端 `HOTPOT_CACHE_TOKEN=secret-token`，端到端 curl：

| 场景 | 结果 |
|------|------|
| `/sccache/{key}` 无 token | `401` |
| `/sccache/{key}` 带正确 token（未命中） | `404` |
| `/v8/artifacts/{hash}` 无 token | `401` |
| `.sccache_check` GET / PUT | `404` / `204` |
| sccache 三层 key PUT / GET | `204` / `200`（body 逐字节一致） |
| sccache PROPFIND / MKCOL | `207` / `201` |
| turbo PUT team_a | `201` |
| turbo GET team_a | `200`，回显 `x-artifact-tag` / `x-artifact-duration` / `x-artifact-sha` |
| turbo GET team_b（不同租户） | `404` |
| turbo HEAD / OPTIONS | `200` / `204` |

### 8.3 实测：工具链选择（F13）

以无依赖的最小项目验证（排除网络与依赖解析噪声）：

| `profile.toolchain` | 结果 | 说明 |
|--------------------|------|------|
| `1.93` | succeeded | 本机已安装 |
| `stable` | succeeded | |
| `1.60`（未安装） | **failed** | `error: Missing manifest in toolchain '1.60-aarch64-apple-darwin'` |
| `1.98.x` | `400` | 边界校验：`invalid rust version '1.98.x'` |
| `nightly-2099-1-1` | `400` | 边界校验：日期必须是 `YYYY-MM-DD` |
| `target: "noseparator"` | `400` | 边界校验：target 三元组形状 |

**关键点**：未安装的工具链**明确失败**而不是静默回落到默认工具链——
工具链错配会让产物与缓存都不可移植，静默回落是最坏的失败方式。
同时 `error` 字段现在直接携带真实 rustc 诊断（此前只有 `cargo build failed`）。

### 8.4 实测：新端点

```
GET /v1/builds?limit=5
BUILD                                  STATUS       BUILD_MS  TOTAL_MS  SOURCE
bld_80730145-…                         succeeded       19862     20479  local:demo-webapp

GET /metrics   （节选）
hotpot_builds_by_status{status="succeeded"} 2
hotpot_queue_depth{} 0
hotpot_cache_hits_total{protocol="sccache"} 0
hotpot_cache_hit_ratio{protocol="turbo"} 0
hotpot_store_bytes{} 7087583
hotpot_info{version="0.1.0",toolchain="rustc 1.98.0 (88d9e12ae 2026-08-18)",executor="Local"} 1

GET /v1/toolchains
default: rustc 1.98.0 (88d9e12ae 2026-08-18)
local toolchains:  stable / 1.93 / 1.98.0
docker images:     rust:1.85-slim-bookworm / rust:1.98-slim-bookworm
```

### 8.5 质量门禁

| 检查 | 结果 |
|------|------|
| `cargo build --workspace` | 通过 |
| `cargo clippy --workspace --all-targets` | 0 warning |
| `cargo test --workspace` | 全部通过（新增协议契约测试 16 项） |
| `cargo fmt --all` | 通过 |

## M9：真实开源项目构建 + warm 复用两级加速（2026-09-29）

以三个开源项目（`~/.hotpot-e2e/oss.py`，走完整平台：git 就位 →
release 构建 → CAS 采集）验证**打包速度与产物正确性**。每个项目 cold/warm
连跑两次，校验：cold/warm 产物 blake3 摘要一致（可复现）、容器内实际
运行 `--version` 正确。

### 9.1 slim 镜像宿主工具链自动供给

`slim` 镜像刻意不含 cc/make：fd 的 `jemalloc-sys` 构建脚本执行 `make`
直接 panic（`No such file or directory`）。执行器现按镜像变体自动安装：
slim → `build-essential`（deb 归档 79MB 持久化复用），全量变体 → 无需，
Alpine → 显式报错。回归测试：`docker_live::docker_slim_image_auto_installs_build_essential`
（构建脚本内直接调用 `cc`）。

### 9.2 为什么 sccache 100% 命中仍然慢——warm target 卷

实测 sccache 全命中时 bat 仍需 ~120s。根因：**`build-script-build` 是
bin crate，sccache 无法缓存，cargo 每次重新编译并运行全部构建脚本**；
最终 bin crate 与链接同样每次发生。新增两级持久化复用：

1. **warm target 卷**：`tools/warm-targets/<hash>-<镜像tag>-<triple|host>-<mode>`
   挂载为容器 `/target`，cargo fingerprint 直接判定依赖单元（含构建脚本）
   为最新。20GiB LRU 配额（30 分钟宽限防删在用目录）。
2. **共享 git 工作区**：`tools/git-workspaces/<hash>/repo` 挂载为
   `/workspace`（`--filter=blob:none` 部分克隆，`fetch + checkout --force`）；
   否则会话路径每次变化，fingerprint 判定项目 crate 全部过期。按 URL 的
   全构建周期锁保证同项目构建进程内串行。

### 9.3 实测：构建速度（build_ms）

| 项目 | cold | warm | 加速比 | warm 起点对照* |
|------|-----:|-----:|-------:|------:|
| fd 10.2.0 | 113.5s | **4.3s** | **26.13x** | 87.7s |
| bat 0.25.0 | 38.7s | **7.6s** | **5.11x** | 119.0s |
| ripgrep 14.1.1 | 22.7s | **7.9s** | **2.87x** | 27.7s |

\* 起点对照 = M9 优化前、sccache 全命中但每次全新会话路径的 warm 耗时。

产物正确性：三个项目 cold/warm digest 全部一致；容器内 smoke 实测
`fd 10.2.0` / `bat 0.25.0` / `ripgrep 14.1.1` 输出正确。

交叉编译另测：aarch64 → x86_64，cold 315.9s → 全 warm（sccache）14.0s
（**22.6x**），产物 `ELF 64-bit LSB pie executable, x86-64`，amd64 容器
内实际执行输出正确。

### 9.4 多版本工具链矩阵（Docker 模式）

| 请求 spec | 选用镜像 | 产物实测 rustc |
|-----------|---------|----------------|
| `1.85` | `rust:1.85-slim-bookworm` | 1.85.1 |
| `1.93` | `rust:1.93-slim-bookworm` | 1.93.1 |
| `1.98.0` | `rust:1.98.0-slim-bookworm` | 1.98.0 |
| `stable` | `rust:slim-bookworm`（浮动跟踪 stable） | 1.98.1 |

官方 Docker Hub rust 镜像不发布任何 channel tag（无 stable/beta/nightly，
含 dated nightly）；beta/nightly 请求被显式拒绝并提示钉版本或使用 local
执行器。Docker 模式 spec 仅用于选镜像，容器内执行不带 `+spec` 的 cargo
（镜像 default 工具链即请求版本）。

### 9.5 docker_live 实测矩阵（7 项串行）

`crates/hotpot-worker/tests/docker_live.rs` 真实容器实测（非 mock），
共享持久 tools 目录、static 互斥串行（colima 上并行冷 apt 曾全部超时）：

| 用例 | 验证点 |
|------|--------|
| docker_build_success | 产物经挂载回宿主 |
| docker_build_failure | 真实 rustc 诊断（非空挂载假象） |
| docker_build_cancel | build.rs 死循环，cancel 后 EndReason::Canceled |
| docker_partial_toolchain… | 不触发 rustup 同步，复用镜像预装工具链 |
| docker_slim_image_auto_installs… | build.rs 调 cc，自动装 build-essential |
| docker_same_arch_musl… | musl-tools 供给，`file(1)` 确认静态链接 |
| docker_wasm32… | rustlib-cache 复用，`\0asm` 魔数合法 |

apt 步骤加固：dpkg-query 已装包整段跳过（warm 零 apt 开销）；安装前
清除 archives/lists 锁文件——cancel 杀掉 apt 进行中的容器后，bind 卷上
残留锁（colima virtiofs 上表现为 "held by process 0"），否则后续构建
全部 apt 失败。受限网络下 apt 相关用例经真实容器探测（`getent hosts
deb.debian.org`）秒级跳过。
