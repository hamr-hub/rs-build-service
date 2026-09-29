# Hotpot 平台使用手册

> 版本：v0.1（适用于 Hotpot 0.1.x，M0–M7）
> 读者：使用 Hotpot 提交构建的开发者、接入缓存的 CI 维护者、部署运维人员。

## 目录

1. [核心概念](#1-核心概念)
2. [服务端部署与配置](#2-服务端部署与配置)
3. [CLI 完整参考](#3-cli-完整参考)
4. [HTTP API 完整参考](#4-http-api-完整参考)
5. [SSE 日志流协议](#5-sse-日志流协议)
6. [接入 sccache 远程缓存](#6-接入-sccache-远程缓存)
7. [接入 Turborepo 远程缓存](#7-接入-turborepo-远程缓存)
8. [CI 集成](#8-ci-集成)
9. [本地热重载开发（hotpot-dev）](#9-本地热重载开发hotpot-dev)
10. [零停机部署（hotpot-agent）](#10-零停机部署hotpot-agent)
11. [配置参考汇总](#11-配置参考汇总)
12. [故障排查（FAQ）](#12-故障排查faq)
13. [已知限制](#13-已知限制)

---

## 1. 核心概念

| 概念 | 说明 |
|------|------|
| **Build（构建）** | 一次构建请求。ID 形如 `bld_<uuid>`，经历 `queued → dispatched → succeeded/failed/canceled/timeout` |
| **Source（源码来源）** | 当前版本支持 `local`（服务端可访问主机上的 Cargo 项目目录）；`git` / `upload` 为模型预留 |
| **Profile（构建档位）** | `mode`（debug/release）、`features`、`no_default_features`、`target`、`cargo_flags`、`toolchain` |
| **Event（事件）** | 构建过程中的 stdout/stderr/phase/status 记录，单构建内带单调递增序号 `seq` |
| **Artifact（产物）** | 构建成功后从 profile 目录采集的文件，按 blake3 内容哈希存入 CAS，不可变、天然去重 |
| **Session（会话）** | 每个构建独立的 `CARGO_TARGET_DIR`，位于数据目录 `sessions/<build-id>/target`，绝不跨项目共享 |
| **CAS** | 内容寻址存储（Content-Addressable Storage），构建产物与远程缓存载荷共用 |
| **Namespace / Tenant** | 远程缓存的协议命名空间（sccache/turbo）与租户标识（turbo 的 teamId/slug） |

构建状态机（设计）：

```text
queued ──claim──▶ dispatched ──执行中──▶ running ──┬──▶ succeeded
                    │                              ├──▶ failed
                    │（租约过期可被重新认领）        ├──▶ canceled
                    └──────────────────────────────┴──▶ timeout
```

> **当前实现说明**：执行期间状态保持为 `dispatched`，`running` 状态尚未单独落库；
> 耗时字段当前仅填充 `build_ms` 与 `total_ms`。详见 [功能设计文档](../design/feature-design.md)。

## 2. 服务端部署与配置

### 2.1 Docker Compose

```bash
docker compose up -d
curl -fsS http://127.0.0.1:7878/healthz   # ok
```

Compose 部署拓扑：

- 服务容器挂载 `/var/run/docker.sock`，通过 daemon 启动**兄弟构建容器**；
- 数据目录与项目根以**与宿主完全一致的绝对路径**同时挂载进服务容器与构建容器；
- 健康检查每 10s 访问 `/healthz`，连续 5 次失败标记 unhealthy。

### 2.2 容器镜像

预构建镜像：

```bash
docker build -t hotpot-server:latest .
docker run -d --name hotpot \
    -p 7878:7878 \
    -v /var/run/docker.sock:/var/run/docker.sock \
    -v "$PWD/hotpot-data:$PWD/hotpot-data" \
    hotpot-server:latest \
    --listen 0.0.0.0:7878 \
    --data-dir "$PWD/hotpot-data" \
    --executor docker \
    --docker-image rust:1.98-slim-bookworm
```

镜像默认 `ENTRYPOINT ["hotpot-server"]`，基础运行镜像为 `debian:bookworm-slim` + `ca-certificates` + `curl`。

### 2.3 本机运行

```bash
cargo run -p hotpot-api --bin hotpot-server
```

默认监听 `127.0.0.1:7878`，数据目录 `./hotpot-data`，内嵌 worker 数 = `min(CPU 并行度, 4)`，
执行后端为本机 cargo。

### 2.4 服务端启动参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--listen` | `127.0.0.1:7878` | HTTP 监听地址 |
| `--data-dir` | `./hotpot-data` | SQLite、CAS、会话目录根，启动时自动创建 |
| `--workers` | `0` | 内嵌 worker 数；`0` = CPU 并行度（上限 4） |
| `--executor` | `local` | 构建执行后端：`local` / `docker`（也可用 `HOTPOT_EXECUTOR`） |
| `--docker-image` | `rust:slim-bookworm` | docker 后端工具链镜像（也可用 `HOTPOT_DOCKER_IMAGE`） |
| `--docker-host` | 自动探测 | docker daemon 地址，如 `unix:///var/run/docker.sock`（也可用 `HOTPOT_DOCKER_HOST`） |

## 3. CLI 完整参考

CLI 名为 `hotpot`（crate：`hotpot-cli`）。

```bash
# 本地编译安装
cargo install --path crates/hotpot-cli
```

### 全局参数

| 参数 | 环境变量 | 默认值 | 说明 |
|------|----------|--------|------|
| `--server <URL>` | `HOTPOT_SERVER` | `http://127.0.0.1:7878` | 服务地址，全局参数须放在子命令前 |

### 3.1 `hotpot build` —— 提交构建并跟踪日志

```bash
hotpot build -p ./my-app [--release] [--features a,b] [--no-follow]
```

| 参数 | 说明 |
|------|------|
| `-p, --project <PATH>` | Cargo 项目根目录（必填）；CLI 自动 canonicalize 为绝对路径提交 |
| `--release` | release 模式（默认 debug） |
| `--features <LIST>` | 启用的 features，逗号分隔 |
| `--no-follow` | 只提交不跟踪日志 |

行为：

- 成功提交后打印 `submitted build <id> (queued)`；
- 默认附加 SSE 日志流，stdout 行显示为 `[ seq] …`，stderr 行显示为 `[ seq] ! …`，phase 行显示为 `---- …`；
- 构建成功退出码 `0`；构建失败/取消等非成功状态退出码 `1` 并打印原因。

示例：

```bash
hotpot --server http://builds.internal:7878 build -p . --release
hotpot build -p . --features jwt,metrics --no-follow
```

### 3.2 `hotpot logs` —— 附加日志流

```bash
hotpot logs <build-id> [--since <SEQ>]
```

- `--since`：从指定 seq（含）开始拉取，用于断线续传；
- 构建最终状态为 `failed` 时退出码 `1`。

### 3.3 `hotpot status` —— 查询状态

```bash
hotpot status <build-id>
```

输出构建 ID、状态、耗时（`build_ms` / `total_ms`）与错误信息。

### 3.4 `hotpot cancel` —— 取消构建

```bash
hotpot cancel <build-id>
```

- 排队中：直接置为 `canceled`；
- 执行中：向执行器发取消信号，cargo 进程被终止；
- 已是终态：原样返回当前记录，退出码 `0`。

### 3.5 `hotpot artifacts` —— 列出产物

```bash
hotpot artifacts <build-id>
```

逐行输出：`大小(字节)  digest  名称`。

### 3.6 `hotpot download` —— 下载全部产物

```bash
hotpot download <build-id> [-o DIR]
```

- `-o, --out <DIR>`：输出目录，默认 `./hotpot-out`；
- Unix 下若产物带 `executable` 属性，下载后自动恢复可执行位（`| 0111`）。

## 4. HTTP API 完整参考

- Base URL：默认 `http://127.0.0.1:7878`
- Content-Type：请求/响应均为 `application/json; charset=utf-8`（下载端点与缓存端点除外）
- 当前版本无认证（设计预留项目 PAT / OIDC，请勿直接暴露到公网）

### 4.1 端点总览

| Method | Path | 说明 |
|--------|------|------|
| GET | `/healthz` | 健康检查，返回 `ok` |
| POST | `/v1/builds` | 提交构建 |
| GET | `/v1/builds/{id}` | 查询构建 |
| GET | `/v1/builds/{id}/logs/stream` | SSE 日志流 |
| POST | `/v1/builds/{id}/cancel` | 取消构建 |
| GET | `/v1/builds/{id}/artifacts` | 产物清单 |
| GET | `/v1/artifacts/{digest}` | 下载产物 |
| ANY | `/sccache/{key}` | sccache WebDAV 兼容端点 |
| GET/HEAD/PUT | `/v8/artifacts/{hash}` | Turborepo v8 兼容端点 |
| GET | `/v8/artifacts/status` | Turbo 缓存状态 |

> `GET /v1/builds`（构建列表）当前返回 `405 Method Not Allowed`。

### 4.2 POST /v1/builds —— 提交构建

请求体：

```json
{
  "source": {
    "kind": "local",
    "path": "/abs/path/to/project"
  },
  "profile": {
    "mode": "release",
    "features": ["json"],
    "no_default_features": false,
    "target": null,
    "cargo_flags": [],
    "toolchain": null
  }
}
```

- `source` 必填；`profile` 可省略（省略时使用默认 profile：debug、无 features）。
- 响应：`202 Accepted`，响应体为完整 `BuildRecord`。

curl 示例：

```bash
curl -fsS -X POST http://127.0.0.1:7878/v1/builds \
    -H 'Content-Type: application/json' \
    -d "{\"source\":{\"kind\":\"local\",\"path\":\"$PWD/examples/demo-webapp\"}}"
```

边界校验（失败返回 `400`）：

- `local.path` 必须存在且为目录；
- 目录下必须包含 `Cargo.toml`；
- `git` / `upload` 来源当前返回 400（`source kind not supported yet`）。

幂等/合并：若已有 **相同 source 与 profile** 的 `queued` 构建，直接返回该构建，不新建。

### 4.3 GET /v1/builds/{id} —— 查询构建

```bash
curl -fsS http://127.0.0.1:7878/v1/builds/bld_xxxxxxxx
```

响应 `BuildRecord`：

```json
{
  "id": "bld_…",
  "source": { "kind": "local", "path": "…" },
  "profile": { "mode": "debug", "features": [], "no_default_features": false, "cargo_flags": [] },
  "status": "dispatched",
  "timings": { "queue_ms": 0, "fetch_ms": 0, "build_ms": 8500, "link_ms": null, "upload_ms": 0, "total_ms": 8500 },
  "created_at_ms": 1780000000000,
  "started_at_ms": null,
  "finished_at_ms": 1780000008500,
  "error": null
}
```

构建不存在返回 `404`。

### 4.4 POST /v1/builds/{id}/cancel

返回更新后的 `BuildRecord`。终态构建重复取消为幂等 no-op。

### 4.5 GET /v1/builds/{id}/artifacts

```json
[
  {
    "name": "demo-webapp",
    "digest": "f3c9…",
    "size": 15523192,
    "attrs": { "executable": "true" }
  }
]
```

### 4.6 GET /v1/artifacts/{digest} —— 下载产物

```bash
curl -fOJ http://127.0.0.1:7878/v1/artifacts/f3c9…?filename=demo-webapp
```

- 响应头 `Content-Disposition: attachment[; filename="…"]`，触发下载而非内联；
- digest 非法返回 `400`；对象不存在返回 `404`。

### 4.7 错误响应格式

```json
{ "error": "project path does not exist: /tmp/x" }
```

| 状态码 | 典型场景 |
|--------|----------|
| 400 | 请求体非法、路径不存在、无 Cargo.toml、digest 格式错误、不支持的 source kind |
| 404 | 构建 / 产物不存在 |
| 405 | 调用了未开放的列表端点 |
| 500 | 存储或数据库内部错误 |

## 5. SSE 日志流协议

`GET /v1/builds/{id}/logs/stream?since=<SEQ>`，标准 `text/event-stream`。

- 服务端每 **400ms** 从 SQLite 批量拉取（每批最多 512 条）并立即推送；
- SSE `event:` 字段取值：

| event | data |
|-------|------|
| `stdout` / `stderr` / `phase` / `status` | `BuildEvent` 的 JSON（含 `seq`、`timestamp_ms`、`payload`） |
| `end` | `{}`，构建进入终态，流随即关闭 |
| `error` | 错误文本，流随即关闭 |

- **游标/续传语义**：服务端返回 `seq >= since` 的事件；客户端记住收到的最后 seq，
  重连时传 `since = lastSeq + 1`（不带 since 则从 0 开始全量回放，事件持久化不会丢失）；
- 内置 keep-alive 注释帧，空闲时连接不被代理掐断；
- 构建不存在返回 `404` 后才升级为 SSE。

## 6. 接入 sccache 远程缓存

Hotpot 暴露与 sccache/opendal WebDAV 兼容的端点，载荷对服务端不透明（sccache 自己打包的 zstd 条目 ZIP），
按客户端 key 索引、载荷内容寻址存入 CAS。

### 6.1 使用原生 sccache（WebDAV 模式）

```bash
export SCCACHE_DIR="$HOME/.cache/sccache"
# sccache 通过 WebDAV 接入（endpoint 指向 /sccache/，末尾路径段即 key 前缀）
export CARGO_HOME_WEBDAV="http://127.0.0.1:7878/sccache/"

RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 cargo build
```

> 不同 sccache 版本 WebDAV 配置项名称略有差异；也可使用 opendal 配置
> `webdav` scheme，endpoint 填 `http://<host>:7878/sccache`。

### 6.2 HTTP 语义

| 方法 | 行为 |
|------|------|
| GET | 命中返回 200 + `application/octet-stream`；未命中 404 |
| HEAD | 命中 200；未命中 404 |
| PUT | 写入，返回 `204 No Content`；同 key 重复写为幂等 no-op |
| MKCOL | 一律 `201 Created`（集合是虚拟的，不落盘） |
| PROPFIND | 返回 `207 Multi-Status`，含 `resourcetype` / `getlastmodified` / `getcontentlength` |
| OPTIONS | 200 |

实测基线（demo-webapp，89 个 crate）：冷构建 11.8s、写穿 68.7 MB；
全新 `SCCACHE_DIR` 纯远端命中 100%，4.2s。

## 7. 接入 Turborepo 远程缓存

兼容 Vercel Turborepo v8 协议（以真实 `turbo` 客户端验证）。

```bash
# turbo 通过环境变量接入自定义缓存端点
TURBO_API=http://127.0.0.1:7878 \
TURBO_TEAM=team_demo \
TURBO_TOKEN=anything \
turbo run build --cache=remote:rw
```

- `GET /v8/artifacts/status` → `{"status":"enabled"}`；
- 租户：查询参数 `teamId` 或 `slug`（二选一，均无则为默认空租户），租户间互不可见；
- GET 命中时原样回传上传时的 `x-artifact-tag` 响应头；PUT 支持携带该头并持久化；
- PUT 成功返回 `201 Created`；
- `TURBO_TOKEN` 当前不校验（预留鉴权）。

实测：清空本地缓存后 `--cache=remote:rw` → `FULL TURBO`（32ms）。

## 8. CI 集成

### 8.1 GitHub Actions 调用 Hotpot

```yaml
name: build via hotpot
on:
  push:
    branches: [main]
jobs:
  remote-build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Build through Hotpot
        run: |
          curl -fsSL https://host-of-your-cli/hotpot -o hotpot || true  # 或直接 cargo run
          cargo run -p hotpot-cli -- --server ${{ secrets.HOTPOT_SERVER }} \
              build -p "$PWD" --release
      - name: Download artifacts
        run: |
          cargo run -p hotpot-cli -- --server ${{ secrets.HOTPOT_SERVER }} \
              download "${BUILD_ID}" -o dist
```

### 8.2 CI 直接接入 sccache 缓存

在任意使用 sccache 的 CI 中把 WebDAV endpoint 指向 Hotpot（见第 6 节），
无需更换工具链即可让全团队共享编译缓存。

### 8.3 部署流水线

构建产物下载后，在目标机器通过 `hotpot-agent deploy` 完成零停机发布（见第 10 节），
可串成 `build → download → deploy` 全自动流水线。

## 9. 本地热重载开发（hotpot-dev）

```bash
cargo run -p hotpot-dev -- ./my-app --addr 127.0.0.1:8080
```

| 参数 | 默认 | 说明 |
|------|------|------|
| `<PROJECT>` | `.` | Cargo 项目目录 |
| `--release` | 关 | release 模式构建 |
| `--features <LIST>` | 空 | cargo features，逗号分隔（可重复） |
| `--addr <HOST:PORT>` | 无 | 公共监听地址；启用 socket keeper |
| `--env KEY=VALUE` | 无 | 注入子进程的环境变量，可重复 |

行为约定：

1. watcher 在**初始构建之前**注册（消除 FSEvents/kqueue 异步注册空窗），
   200ms 静默窗口防抖；仅 `.rs` / `Cargo.toml` / `Cargo.lock` 变更触发重建，
   `target/`、`.git/` 下变更忽略；
2. 保存即增量 `cargo build`；构建失败时**旧进程继续服务**，控制台打印 cargo 诊断尾部，
   修复后自动收敛；
3. 启用 `--addr` 时：keeper 常驻公共端口，子进程经 `HOTPOT_BIND_ADDR` 绑定固定 loopback 后端；
   重启窗口内 keeper 持连重试，后端 10s 就绪窗口内恢复则继续服务，超时返回 503；
4. 子进程 stdout/stderr 直通终端；停止时 SIGTERM，5s 超时 SIGKILL。

对应用的要求：是一个 bin target；若需固定对外端口，请绑定 `HOTPOT_BIND_ADDR`
（未启用 keeper 时该变量不存在，应用按自身默认方式绑定即可）。

## 10. 零停机部署（hotpot-agent）

> 仅支持 Unix（Linux/macOS）：依赖 fd 传递、AF_UNIX 与信号。

### 10.1 启动 supervisor

```bash
hotpot-agent --data-dir /var/lib/hotpot-agent run \
    --listen 0.0.0.0:8080 \
    --app /usr/bin/my-app \
    --version v1
```

| 参数 | 默认 | 说明 |
|------|------|------|
| `--data-dir` | `./.hotpot-agent` | 控制 socket、`state.json`、版本副本目录 |
| run `--listen` | `127.0.0.1:9000` | 服务监听地址，**supervisor 只绑定一次并永久持有** |
| run `--app` | 无 | 初始二进制（仅数据目录为空、首次启动时使用） |
| run `--version` | `base` | 初始版本名 |
| `--env KEY=VAL` | 无 | 子进程环境变量，可重复；deploy 时可替换整套 env |

### 10.2 发布 / 回滚 / 查询

```bash
# 发布新版本：二进制被复制固化到 data-dir/versions/<version>
hotpot-agent --data-dir /var/lib/hotpot-agent \
    deploy --app ./my-app-v2 --version v2

hotpot-agent --data-dir /var/lib/hotpot-agent rollback   # 回到上一版本
hotpot-agent --data-dir /var/lib/hotpot-agent status     # 查看 current/previous/phase
```

### 10.3 发布状态机

```text
Fetch      复制不可变二进制副本到 versions/，写入 phase=preflighting
Preflight  启动新进程；轮询独立探针端口 /healthz（15s 超时；未 warm 返回 503）
Arm        控制管道下发 ARM，新进程应答 ARMED 后开始 accept 真实端口
Attach     再次确认新进程健康
Drain      旧版本 SIGTERM，8s 优雅窗口，超时 SIGKILL   ← point-of-no-return
Commit     下发 COMMIT、关闭探针，切换 current/previous
```

失败语义：

- Preflight/Arm 失败（含新进程崩溃、超时）：终止新进程，**旧版本继续服务**，phase 置 `failed`；
- Drain 期间新版本死亡：尽力把旧版本重新部署回来并报错；
- `rollback` 本质是对 `previous` 复用同一状态机。

### 10.4 被部署应用的接入契约

应用启动时检查环境变量 `HOTPOT_LISTEN_FDS=1`，若存在则从固定 fd 接管资源：

| fd | 资源 |
|----|------|
| 3 | 真实监听 socket（systemd 风格 LISTEN_FDS） |
| 4 | 预热探针 socket（127.0.0.1 随机端口） |
| 5 | 控制管道（收 ARM/COMMIT，回 ARMED） |

应用职责：预热完成前探针返回 503；收到 ARM 后再开始 accept 真实端口；
SIGTERM 后停止 accept、排空在飞请求再退出。`hotpot-agent` crate 的
`activate` 模块提供了参考实现，`echo_app` 为可运行示例。

### 10.5 崩溃恢复

`state.json` 每阶段落盘（原子 rename）。supervisor 重启后执行 `bootstrap`：
重新拉起 `current` 版本（Preflight → Arm → Commit），无需人工介入。

建议以 systemd（`Restart=always`）或容器方式托管 agent 进程本身。

## 11. 配置参考汇总

### 11.1 环境变量

| 变量 | 适用组件 | 默认 | 说明 |
|------|----------|------|------|
| `HOTPOT_SERVER` | CLI | `http://127.0.0.1:7878` | 服务地址 |
| `HOTPOT_EXECUTOR` | server | `local` | `local` / `docker` |
| `HOTPOT_DOCKER_IMAGE` | server | `rust:slim-bookworm`（compose 中为 `rust:1.98-slim-bookworm`） | 构建容器镜像 |
| `HOTPOT_DOCKER_HOST` | server | bollard 自动探测（含 `DOCKER_HOST`） | daemon 地址 |
| `HOTPOT_PROJECT_ROOT` | compose | 当前目录 | 挂载并允许构建的项目根 |
| `HOTPOT_DATA_DIR` | compose | `$PWD/hotpot-data` | 数据目录 |
| `HOTPOT_CARGO_BIN` | worker/dev | `cargo` | 覆盖 cargo 可执行文件（测试/定制用） |
| `RUST_LOG` | 全部服务端 | `info` | tracing 过滤指令，如 `debug,hotpot_worker=info` |

### 11.2 端口

| 端口 | 用途 |
|------|------|
| 7878 | hotpot-server 默认 HTTP 端口 |
| 9000 | hotpot-agent 默认监听（可改） |
| 随机 loopback | dev 后端端口（自动选取）、agent 探针端口（自动选取） |

## 12. 故障排查（FAQ）

**Q：构建容器内报 `could not find Cargo.toml`？**
路径在 Docker daemon 宿主上不存在，或服务容器内路径与宿主不一致。Compose 下必须用宿主绝对路径提交，
且 `HOTPOT_PROJECT_ROOT` 覆盖该路径的上层目录。

**Q：colima 下构建容器里目录是空的？**
项目路径不在 colima VM 的挂载范围内（默认仅挂载 `$HOME`）。把项目移到 `$HOME` 下，
或调整 colima mount 配置。

**Q：首次 docker 构建长时间无输出？**
正在拉取工具链镜像（镜像约数百 MB）。可预先 `docker pull rust:1.98-slim-bookworm`。

**Q：构建成功但状态是 failed？**
产物采集/入 CAS 失败会显式置 failed，错误信息在记录的 `error` 字段；检查数据目录磁盘空间与权限。

**Q：SSE 流意外断开？**
事件已持久化，用 `hotpot logs <id> --since <lastSeq+1>` 续传，不丢日志。

**Q：取消后构建仍在运行？**
取消先杀 cargo 子进程再回收；docker 后端会等待容器退出（10s 回收窗口）并清理。
若反复出现请查看 server 日志中的 cancel/renew 相关告警。

**Q：agent 提示 `no previous release to roll back to`？**
还没有任何一次成功完成的部署，因此没有 previous。先成功 deploy 一次。

**Q：如何确认缓存真的命中？**
sccache 用 `sccache --show-stats`；turbo 输出会显示 `FULL TURBO`；
也可观察构建耗时与数据目录 `cacheproto.db` 条目增长。

## 13. 已知限制

- 源码来源当前仅支持 `local`（git clone / 源码包上传在领域模型中预留，尚未实现）；
- 构建 API 暂无认证与多租户配额；缓存协议端点同样不校验 token；
- 执行期间状态保持 `dispatched`（未单独写入 `running`）；耗时仅 build/total 两段；
- 产物采集固定为 profile 目录**顶层文件**（跳过 `.d` 与隐藏文件），暂不支持自定义 glob；
- agent 的 fd 交接仅适用于 Unix；应用须遵循 `HOTPOT_LISTEN_FDS` 接入契约；
- 单 SQLite + 内嵌 worker 为小团队形态；高可用/横向扩展见架构文档演进路线。
