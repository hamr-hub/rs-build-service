# Hotpot 入门说明

> 本文档帮助你在 10 分钟内部署 Hotpot、提交第一次构建并拿到产物。
> 完整功能说明见 [平台使用手册](guide/user-manual.md)。

## 1. Hotpot 是什么

Hotpot 是一个开源、可自托管的 Rust 构建服务，一条工具链覆盖从编码到上线的完整回路：

| 能力 | 组件 | 说明 |
|------|------|------|
| 远程构建 | `hotpot-server` + `hotpot` CLI | HTTP API 提交构建，SSE 实时日志，产物内容寻址存储 |
| 多级缓存 | `hotpot-cacheproto` | sccache（WebDAV 兼容）与 Turborepo v8 双协议，原生客户端直接接入 |
| 容器执行 | `hotpot-worker` | 构建在工具链容器内进行（bollard），与宿主隔离 |
| 热重载开发 | `hotpot-dev` | 保存即增量重建，失败保留旧进程，socket keeper 重启窗口不拒连 |
| 零停机部署 | `hotpot-agent` | supervisor 常驻持有 listen socket，部署失败自动回滚 |

## 2. 系统要求

### 方式一：Docker Compose

- Docker Engine ≥ 20.10（支持 Compose v2）；macOS 用户可用 Docker Desktop 或 colima
- 可拉取 `rust:1.98-slim-bookworm` 工具链镜像（首次构建时拉取）
- 磁盘：建议预留 5 GB（工具链镜像 + 缓存 + 会话目录）

> colima 用户：确保项目与数据目录位于 VM 已挂载的宿主路径内（colima 默认挂载 `$HOME`）。

### 方式二：本机运行

- Rust 工具链 ≥ 1.85（见仓库根 `rust-toolchain.toml`）
- macOS 或 Linux
- 本机已安装 `cargo`（自托管构建本地执行器时使用）

## 3. Docker Compose 部署（推荐）

```bash
# 克隆仓库后，在仓库根目录执行
docker compose up -d

# 健康检查
curl -fsS http://127.0.0.1:7878/healthz   # 输出 ok
```

服务容器通过 Docker daemon socket 以「兄弟容器」方式启动构建容器（不是 docker-in-docker）。
因此源码目录与数据目录均以**与宿主相同的绝对路径**挂载——提交构建时直接使用宿主路径：

```bash
# 安装/编译 CLI（也可直接用 cargo run，见下）
cargo run -p hotpot-cli -- --server http://127.0.0.1:7878 \
    build -p "$PWD/examples/demo-webapp"
```

可用环境变量（在 `docker compose up` 前导出）：

| 变量 | 默认 | 说明 |
|------|------|------|
| `HOTPOT_PROJECT_ROOT` | 当前目录 | 允许构建的项目根，**必须为绝对路径** |
| `HOTPOT_DATA_DIR` | `$PWD/hotpot-data` | 会话与缓存目录，**必须为绝对路径** |
| `HOTPOT_DOCKER_IMAGE` | `rust:1.98-slim-bookworm` | 构建容器工具链镜像 |
| `RUST_LOG` | `info` | 服务日志级别 |

## 4. 本机运行（无需 Docker）

```bash
# 终端 1：启动服务（SQLite、CAS 默认落在 ./hotpot-data）
cargo run -p hotpot-api --bin hotpot-server

# 终端 2：提交当前项目构建并实时跟踪日志
cargo run -p hotpot-cli -- build -p . --release
```

服务端启动参数：

```bash
cargo run -p hotpot-api --bin hotpot-server -- \
    --listen 127.0.0.1:7878 \
    --data-dir ./hotpot-data \
    --workers 2 \
    --executor local          # 或 docker（--docker-image 指定工具链镜像）
```

## 5. 第一次构建

以内置示例 `demo-webapp`（一个 axum 笔记服务）为对象：

```bash
# 1. 提交构建并跟踪日志（-p 指向 Cargo 项目根）
cargo run -p hotpot-cli -- build -p "$PWD/examples/demo-webapp"
# 输出形如：submitted build bld_xxxxxxxx (queued)，随后实时打印 [seq] 日志行

# 2. 查询状态 / 列出产物 / 下载
cargo run -p hotpot-cli -- status   <build-id>
cargo run -p hotpot-cli -- artifacts <build-id>
cargo run -p hotpot-cli -- download  <build-id> -o ./dist

# 3. 运行产物
./dist/demo-webapp --listen 127.0.0.1:3000
curl http://127.0.0.1:3000/health    # ok
```

取消构建：

```bash
cargo run -p hotpot-cli -- cancel <build-id>
```

## 6. 数据目录结构

`HOTPOT_DATA_DIR`（默认 `./hotpot-data`）下：

```text
hotpot-data/
├── hotpot.db            # 构建队列库：builds / build_events / artifacts（SQLite WAL）
├── cacheproto.db        # 远程缓存索引：kv_entries（client key → CAS digest）
├── store/
│   ├── objects/<xx>/…   # 内容寻址对象（blake3，256 桶，zstd 压缩）
│   └── tmp/             # 写入临时目录，原子 rename 落库
└── sessions/<build-id>/target   # 会话级 CARGO_TARGET_DIR（不跨项目共享）
```

## 7. 继续探索

- 📖 [平台使用手册](guide/user-manual.md) —— CLI/API 完整参考、缓存接入、CI 集成、部署运维、故障排查
- 🏛️ [系统架构文档](design/system-architecture.md) —— 运行时模型、持久化设计、并发控制、故障恢复
- 🧩 [功能设计文档](design/feature-design.md) —— 各功能的规格、边界条件与异常处理
- 📐 [总体架构设计（设计决策）](design/architecture.md)
- 🔭 [项目愿景](VISION.md)
- 📊 [基准基线](design/benchmark-baseline.md)

## 8. 停止与卸载

```bash
docker compose down          # 停止服务（数据目录保留）
docker compose down -v       # 停止并清理（本项目未使用命名卷，数据仍在宿主目录）
rm -rf ./hotpot-data         # 如需彻底删除构建记录、缓存与会话数据
```
