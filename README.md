# 🍲 Hotpot

**开源、可自托管的 Rust 构建服务。** 远程触发构建、多级内容寻址缓存、
本地热重载开发、生产零停机部署——一条工具链，覆盖从编码到上线的完整回路。

[![CI](https://github.com/hamr-hub/rs-build-service/actions/workflows/ci.yml/badge.svg)](https://github.com/hamr-hub/rs-build-service/actions/workflows/ci.yml)

## 为什么

Rust 的编译等待是真实的生产力损耗。现有方案要么是纯本地工具（sccache/`cargo watch`），
要么是重型企业系统。Hotpot 把构建变成**服务**：

> 完整论述（解决谁的什么问题、为什么用 Rust 写、和现有方案差在哪、诚实的边界）
> 见 **[Hotpot 的价值与意义](docs/why-hotpot.md)**。

- ☁️ **远程构建**：HTTP API + CLI，提交即走，SSE 实时日志；
- 🗃️ **多级内容寻址缓存**：依赖层（cargo-chef 风格）、crate 层（sccache 兼容，
  WebDAV/Turborepo v8 协议）、最终产物层（blake3 CAS）；
- ⚡ **增量构建**：会话隔离的 target 目录 + 缓存复用；
- 🔄 **热重载开发**（`hotpot-dev`）：保存即增量重建，构建失败保留旧进程，
  socket keeper 保证重启窗口不拒连；
- 🚀 **零停机部署**（`hotpot-agent`）：supervisor 常驻持有 listen socket，
  Fetch → Preflight → Arm → Drain → Commit，失败自动回滚；
- 🐳 **Docker 执行器**：构建在工具链容器内进行（bollard），一条
  `docker compose up` 完成自托管；
- 🔧 **工具链选择**：每个构建可指定 `stable` / `nightly-2026-01-15` / `1.98.0`，
  容器镜像版本自动匹配，未安装的工具链**明确失败**而非静默回落；
- 🔒 **缓存端点鉴权**：可选 Bearer token（`HOTPOT_CACHE_TOKEN`），
  关闭「任何人往缓存里写伪造产物」的供应链投毒面；
- 📊 **可观测**：`/metrics` 暴露构建状态分布、队列深度、五段耗时与**缓存命中率**。

## 快速开始

### 方式一：Docker Compose（推荐）

```bash
docker compose up -d
curl -fsS http://127.0.0.1:7878/healthz   # ok
```

服务以 docker executor 模式运行，首次构建会自动拉取工具链镜像。
服务容器通过 daemon socket 以「兄弟容器」方式启动构建容器，因此源码目录与
数据目录都以**与宿主相同的绝对路径**挂载——提交构建时直接使用宿主路径：

```bash
cargo run -p hotpot-cli -- --server http://127.0.0.1:7878 \
    build -p "$PWD/examples/demo-webapp"
```

可用环境变量：

| 变量 | 默认 | 说明 |
|------|------|------|
| `HOTPOT_PROJECT_ROOT` | 当前目录 | 允许构建的项目根，**必须为绝对路径** |
| `HOTPOT_DATA_DIR` | `./hotpot-data` | 会话与缓存目录，**必须为绝对路径** |
| `HOTPOT_DOCKER_IMAGE` | `rust:1.98-slim-bookworm` | 构建容器工具链镜像 |

> colima / Docker Desktop 用户：确保上述路径位于 VM 已挂载的宿主目录内
> （colima 默认挂载 `$HOME`），否则容器内会看到空目录。

### 方式二：本机运行

```bash
# 启动服务（SQLite、CAS 默认在 ./hotpot-data）
cargo run -p hotpot-api --bin hotpot-server

# 另一个终端：提交当前项目构建并跟踪日志
cargo run -p hotpot-cli -- build -p . --release
```

## CLI

```bash
hotpot build -p ./my-app                    # 构建并跟踪日志（HOTPOT_SERVER 指定服务）
hotpot build -p . --release --toolchain 1.98.0   # 指定模式与工具链
hotpot build --git-url https://host/r.git --git-ref main   # git 来源（需服务端开启）
hotpot list --status failed                 # 列出构建
hotpot status <build-id>                    # 查询状态
hotpot logs <build-id>                      # 附加日志流
hotpot cancel <build-id>                    # 取消
hotpot artifacts <build-id>                 # 列出产物
hotpot download <build-id> -o ./dist        # 下载全部产物
hotpot toolchains                           # 查看可用工具链与镜像
```

## 本地热重载开发

```bash
cargo run -p hotpot-dev -- ./my-app --addr 127.0.0.1:8080
```

保存源码即增量重建并优雅重启（子进程经 `HOTPOT_BIND_ADDR` 绑定后端）；
编译失败时旧进程继续服务，修复后自动收敛。

## 零停机部署

```bash
# supervisor 常驻，永久持有 listen socket
hotpot-agent --data-dir /var/lib/hotpot-agent run \
    --app /usr/bin/my-app --version v1 --listen 0.0.0.0:8080

# 部署新版本（旧版本优雅 drain，预检失败自动回滚）
hotpot-agent --data-dir /var/lib/hotpot-agent \
    deploy --app /usr/bin/my-app --version v2

hotpot-agent --data-dir /var/lib/hotpot-agent rollback   # 一键回滚
hotpot-agent --data-dir /var/lib/hotpot-agent status
```

## HTTP API

| Method | Path | 说明 |
|--------|------|------|
| `POST` | `/v1/builds` | 提交构建（`{"source":{"kind":"local","path":"…"},"profile":{…}}`） |
| `GET` | `/v1/builds` | 列出构建（`status` / `limit` / `offset`） |
| `GET` | `/v1/builds/{id}` | 查询构建 |
| `GET` | `/v1/builds/{id}/logs/stream` | SSE 日志流（`?since=`） |
| `POST` | `/v1/builds/{id}/cancel` | 取消构建 |
| `GET` | `/v1/builds/{id}/artifacts` | 产物清单 |
| `GET` | `/v1/artifacts/{digest}` | 下载产物 |
| `GET` | `/v1/toolchains` | 工具链发现（宿主 rustup + docker 镜像） |
| `GET` | `/metrics` | Prometheus 指标 |

另提供两套缓存协议端点，可选 Bearer 鉴权（`HOTPOT_CACHE_TOKEN`）：

| 协议 | 端点 | 说明 |
|------|------|------|
| sccache（WebDAV 兼容） | `/sccache/{*key}` | 面向 Rust/C/C++ 生态 |
| Turborepo v8 | `/v8/artifacts/{hash}` | 面向 JS/TS monorepo |

**协议文档**（含正确环境变量、交互时序、curl 实操与排障）：
[sccache WebDAV](docs/protocols/sccache-webdav.md) ·
[Turborepo v8](docs/protocols/turborepo-v8.md) ·
[双协议总览](docs/protocols/README.md)

> 最高杠杆的一处设计：Hotpot **自己的构建**也能通过自己的 sccache 端点走远端
> 缓存（`--self-sccache`）。协议实现的正确性因此每天被自己的构建验证一次。

## 架构

```
crates/
├── hotpot-core        # 领域模型、摘要/ID、错误
├── hotpot-store       # 内容寻址存储（256 桶、zstd、LRU）
├── hotpot-scheduler   # 任务队列、租约、事件持久化（SQLite）
├── hotpot-worker      # 构建执行：local + docker（bollard）
├── hotpot-cacheproto  # sccache / Turborepo 缓存协议
├── hotpot-api         # HTTP API + 内嵌 worker 驱动
├── hotpot-cli         # 命令行客户端
├── hotpot-agent       # 生产零停机部署 supervisor
└── hotpot-dev         # 本地热重载开发循环
```

深入阅读：

- [**价值与意义**](docs/why-hotpot.md) —— 解决谁的什么问题、为什么用 Rust 写
- [入门说明](docs/getting-started.md)
- [平台使用手册](docs/guide/user-manual.md)
- [双协议缓存说明](docs/protocols/README.md)（sccache WebDAV / Turborepo v8）
- [系统架构文档](docs/design/system-architecture.md)
- [功能设计文档](docs/design/feature-design.md)
- [总体架构设计（设计决策）](docs/design/architecture.md)
- [愿景](docs/VISION.md)
- [热重载与部署调研](docs/research/03-hot-reload-deployment.md)
- [基准基线与实测记录](docs/design/benchmark-baseline.md)

## 开发

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# Docker 执行器实测（daemon 不可达时自动跳过；colima 需把临时目录放在挂载路径内）：
HOTPOT_TEST_TMP=$HOME/.hotpot-tmp cargo test -p hotpot-worker --test docker_live
```

工具链：见 `rust-toolchain.toml`；MSRV 1.85。

## 协议

Dual-licensed under MIT or Apache-2.0，由你选择。
