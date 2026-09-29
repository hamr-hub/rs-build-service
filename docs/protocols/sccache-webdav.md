# sccache 远端缓存协议（WebDAV 兼容）

> 面向使用者与维护者。目标是让你**照着就能配好**，并且知道每个坑藏在哪。
> 结论先行：Hotpot 的 `/sccache` 端点与真实 sccache 的 WebDAV 后端行为一致
> （实测 89 个 crate 100% 纯远端命中），但**配置用的环境变量名极易记错**，
> 且配错是**静默失败**。请重点看 §3 与 §7。

---

## 1. 这是什么

[sccache](https://github.com/mozilla/sccache) 是编译缓存服务：它作为 `rustc` 的
wrapper，按「编译输入的哈希」缓存编译产物。命中时直接回放产物，**跳过编译**。

它支持多种远端后端（S3、GCS、Azure、Redis、Memcached、**WebDAV** 等）。
Hotpot 实现的是 **WebDAV 后端**：一个够用、无需云厂商账号、自托管友好的
「不透明字节存储 + 极薄的方法语义」。

Hotpot 提供的价值是：把 WebDAV 后端从「要一台 WebDAV 服务器」变成
「你的构建服务本来就有的一个 HTTP 端点」。

## 2. 快速开始

```bash
# 1) 启动 Hotpot（假设监听 127.0.0.1:7878）
cargo run -p hotpot-api --bin hotpot-server

# 2) 让 sccache 指向它
export SCCACHE_DIR=/tmp/sccache-local
export SCCACHE_WEBDAV_ENDPOINT="http://127.0.0.1:7878/sccache"

# 3) 照常构建
export RUSTC_WRAPPER=sccache
export CARGO_INCREMENTAL=0     # sccache 与增量编译互斥
cargo build
```

验证是否真的生效（**务必做**）：

```bash
sccache --show-stats
# 看 "Remote storage" 相关的 hits/misses；全是 0 或没有该段 = 没走远端
```

## 3. 环境变量（**最容易配错的地方**）

| 变量 | 是否必填 | 说明 |
|------|---------|------|
| `SCCACHE_WEBDAV_ENDPOINT` | 二选一 | WebDAV 服务根，如 `http://host:7878/sccache` |
| `SCCACHE_REMOTE_STORAGE` | 二选一 | 统一 URL 形式：`webdav+http://host:7878/sccache` |
| `SCCACHE_WEBDAV_KEY_PREFIX` | 否 | key 前缀，**独立变量**，默认空 |
| `SCCACHE_WEBDAV_USERNAME` / `_PASSWORD` | 否 | Basic 鉴权（Hotpot 用 Bearer，见下） |
| `SCCACHE_WEBDAV_TOKEN` | 否 | Bearer 鉴权，对应 Hotpot 的 `HOTPOT_CACHE_TOKEN` |
| `SCCACHE_WEBDAV_RW_MODE` | 否 | `ReadWrite`（默认）/ `ReadOnly` |
| `SCCACHE_WEBDAV_DISABLE_CREATE_DIR` | 否 | `true` 可跳过写前 PROPFIND/MKCOL |
| `SCCACHE_DIR` | 建议 | 本地盘缓存目录，**与远端叠加而非替代** |

> ### ⚠️ 不存在的变量名
>
> - **`SCCACHE_WEBDAV_URL` 不存在**。真实名是 `SCCACHE_WEBDAV_ENDPOINT`。
> - **`SCCACHE_WEBDAV_PREFIX` 不存在**。真实名是 `SCCACHE_WEBDAV_KEY_PREFIX`，
>   且它是**独立变量**，不从 endpoint 尾部解析。
> - `CARGO_HOME_WEBDAV` 在 sccache 任何版本里都不存在。
>
> 配错这些名字的后果：sccache **完全不报错**，只是安静地退回本地盘缓存。
> 表现是「远端缓存好像没生效」，没有任何错误信息。排查第一步永远是
> `sccache --show-stats` 看远端计数。

## 4. URL 布局：三层分片

sccache 的缓存 key 是 64 位十六进制（32 字节）。WebDAV 后端把它切成
**三层路径**：

```
<SCCACHE_WEBDAV_ENDPOINT>/<SCCACHE_WEBDAV_KEY_PREFIX>/<a>/<b>/<hash>
                                                 │   │   └─ hex[4..]  (60 字符)
                                                 │   └───── hex[2..4]
                                                 └───────── hex[0..2]
```

最小配置（endpoint 即根、prefix 为空）时，一个真实请求长这样：

```
GET http://127.0.0.1:7878/sccache/ab/cd/0123456789abcdef0123456789abcdef…
```

**服务端约束**：通配路由必须保留 `/`，不能把 key 归一化（例如把 `/` 换成 `_`），
否则会**静默破坏**所有历史缓存条目。Hotpot 的 `strip_prefix("/sccache/")`
原样保留整段路径（见 `crates/hotpot-cacheproto/src/routes.rs`）。

> 注意区分：sccache 的**本地磁盘**后端是两层分片，**WebDAV** 后端是三层。
> Hotpot 两层三层都能正确处理，因为 key 是不透明的。

## 5. 方法与状态码

| Method | 路径 | 成功状态码 | 说明 |
|--------|------|-----------|------|
| `GET` | 对象 | `200` + `application/octet-stream` | 读缓存；未命中 `404` |
| `HEAD` | 对象 | `200` / `404` | 存在性（真实 sccache 读路径**不发** HEAD，保留供排查） |
| `PUT` | 对象 | `204 No Content` | 写缓存（唯一必需的写成功码） |
| `MKCOL` | 集合 | `201 Created` | 建目录；Hotpot 中集合是**虚拟**的，直接成功不落盘 |
| `PROPFIND` | 集合/对象 | `207 Multi-Status` | 目录探测，返回最小合法 multistatus XML |
| `OPTIONS` | 任意 | `200` | 能力探测 |
| 其它 | — | `405` | 不支持的方法 |

响应体一律 `Content-Length` 与实际字节数严格一致（sccache 会把
`Content-Length` 参与签名计算，长度不符会导致校验失败）。

## 6. 真实交互时序

### 6.1 冷启动能力探测（**最关键，也最容易被忽略**）

sccache 启动时对每个远端后端做一次探测：

| 步 | 请求 | 期望 | 不满足的后果 |
|----|------|------|--------------|
| 1 | `GET /sccache/.sccache_check` | `404`（NotFound 被容忍） | 其它错误码 → sccache 启动失败 |
| 2 | `PUT /sccache/.sccache_check`（`Hello, World!`） | `2xx` | **静默降级为只读**，远端写入全部丢弃 |

第 2 步是**整条链路上最隐蔽的失败模式**：sccache 不会报错，只是把远端
缓存标记为不可写，此后所有 `PUT` 结果都丢弃，而 `GET` 仍会尝试（永远 miss）。
用户看到的现象是「命中率上不去，日志里只有一行 sccache 的 warning」。

因此 Hotpot 对哨兵 key 做了**显式短路**（`SCCACHE_CHECK_KEY`）：
稳定返回 `404`/`204`，且**不落 CAS、不入索引**。这样即使将来有人给 key
加「必须是 64 位十六进制」之类的校验，也不会把这条契约打破。
该行为由 `crates/hotpot-cacheproto/tests/routes.rs::sccache_check_probe_contract` 钉住。

### 6.2 冷写（opendal `create_dir` 默认开启）

```
PROPFIND /sccache/ab/cd/     → 207   (Depth: 1，探测父集合)
MKCOL    /sccache/ab/cd/     → 201
PROPFIND /sccache/ab/        → 207
MKCOL    /sccache/ab/        → 201
PROPFIND /sccache/           → 207   ← 集合根，Hotpot 专门注册了这条路由
MKCOL    /sccache/           → 201
PUT      /sccache/ab/cd/<hash> → 204
```

设 `SCCACHE_WEBDAV_DISABLE_CREATE_DIR=true` 可跳过前 6 步，只剩 `PUT`。
Hotpot 两种模式都工作。

> **PROPFIND 响应体的硬性要求**：opendal 用 `getlastmodified` 元素的
> **存在性**判断 multistatus 是否解析成功。空 body 或缺该元素会触发
> 反序列化失败 → 写前探测失败 → 静默只读。Hotpot 返回带
> `<getlastmodified>` 的最小合法 XML，并对 `href` 做 XML 转义。

### 6.3 冷读

```
GET /sccache/ab/cd/<hash>   → 200 + body，或 404
```

读路径**不发** `HEAD`，也不发 `PROPFIND`。服务端日志里没有 HEAD 是正常的。

## 7. 缓存 key 是怎么算出来的

由 sccache 客户端决定，服务端不参与。输入包含：

- `rustc -vV` 全量输出（含 commit hash、LLVM 版本、host triple）
- crate 名与版本
- 全部源文件内容哈希
- 全部编译参数（`-C opt-level`、`--cfg`、codegen-units、`-l`、crate-type…）
- 白名单内的环境变量（`CARGO_*`、`RUSTC_*`、target 相关）
- `--target` 三元组
- 直接依赖的 metadata hash

两个 Rust 特有的坑：

1. **绝对路径敏感**：不同机器上的构建目录不同会击穿命中率。
   跨机共享 Hotpot 时请设置 `SCCACHE_BASEDIRS` 剥离基目录前缀，
   或统一所有机器的构建根路径。
2. **链接阶段不进缓存**：bin/dylib/cdylib/proc-macro 的 link 步骤不走 sccache。
   所以「全命中」仍会有链接耗时，这是预期行为。

## 8. 缓存对象格式（服务端不需要知道，但了解一下）

sccache 存的**不是**裸 zstd，而是 **ZIP 归档**（外层整体 zstd 压缩）：

- 每个 entry 以 ZIP `Stored` 方式写入，压缩完全由外层 zstd 承担；
- entry 名 = 编译产物在输出目录中的相对路径（`libfoo.rlib`、`foo-abc.d`…）；
- 额外包含 `stdout` / `stderr` 两个 entry，用于命中时回放编译器的输出。

因此：

- 服务端**无法也不应该**解析它，只能当不透明字节（Hotpot 正是如此）；
- 协议层**没有** checksum 头要校验（上传不带 `Content-MD5`，下载不校验 ETag）；
- 完整性由 sccache 侧的 **ZIP CRC32** 在解压时保证——数据损坏表现为
  **静默 miss**，不会 crash。

## 9. curl 实操

```bash
B=http://127.0.0.1:7878
A="Authorization: Bearer $HOTPOT_CACHE_TOKEN"   # 未设置 token 时可省略

# 能力探测契约（务必先跑）
curl -i $B/sccache/.sccache_check                      # 期望 404
curl -i -X PUT --data-binary 'Hello, World!' $B/sccache/.sccache_check   # 期望 204

# 真实三层分片 key 往返
K=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
curl -i -X PUT --data-binary 'dummy-cache-entry' "$B/sccache/ab/cd/$K"   # 204
curl -i "$B/sccache/ab/cd/$K"                                             # 200
curl -i -I "$B/sccache/ab/cd/$K"                                          # 200

# 写前目录创建序列
curl -i -X PROPFIND -H 'Depth: 1' "$B/sccache/ab/cd/"                    # 207
curl -i -X MKCOL "$B/sccache/ab/cd/"                                     # 201
curl -i -X PROPFIND -H 'Depth: 1' "$B/sccache/"                          # 207
curl -i -X MKCOL "$B/sccache/"                                           # 201

# 不支持的方法
curl -i -X DELETE "$B/sccache/ab/cd/$K"                                  # 405
```

验证「纯远端命中」（本地盘清空，只靠 Hotpot）：

```bash
rm -rf "$SCCACHE_DIR" && cargo clean
sccache --zero-stats && cargo build     # 期望几乎全 hits
```

> 只删 `SCCACHE_DIR` 不删 target 是不够的：本地 target 命中会掩盖远端问题。

## 10. Hotpot 侧实现要点

| 关注点 | 做法 |
|--------|------|
| 载荷 | 完全 opaque，按 key 存字节；内容寻址入 blake3 CAS |
| 索引 | SQLite `kv_entries(namespace='sccache', tenant='', cache_key)` |
| 集合 | 虚拟化：`MKCOL`/`PROPFIND` 直接成功，不落盘 |
| 幂等 | 同 key 二次写入**不覆盖**（sccache 的 key 即内容哈希，安全）；非通用 WebDAV 覆盖语义 |
| 体积上限 | `DefaultBodyLimit` 4 GiB，超限 `413`（防止单请求打满内存） |
| 悬挂索引 | CAS 对象丢失时自动删索引并按 miss 处理 |
| 鉴权 | 可选 Bearer（`HOTPOT_CACHE_TOKEN`），常量时间比较 |
| 测试 | `tests/routes.rs` 覆盖状态码、Content-Length、分片 key、探测契约、href 转义 |

## 11. 排障速查

| 现象 | 优先排查 |
|------|---------|
| 命中率一直是 0 | ① `sccache --show-stats` 有没有远端计数；② 环境变量名（§3）；③ `HOTPOT_CACHE_TOKEN` 是否让 sccache 拿到 token |
| 能读不能写，PUT 后仍 miss | 几乎一定是 `.sccache_check` 的 PUT 不是 2xx（§6.1） |
| 换了机器就全 miss | 绝对路径进了 key → 设 `SCCACHE_BASEDIRS`（§7） |
| 服务端日志没有 HEAD | 正常，读路径只发 GET（§6.3） |
| 全命中但仍然慢 | 链接阶段不进缓存（§7） |
