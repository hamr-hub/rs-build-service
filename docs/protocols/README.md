# Hotpot 双协议缓存：sccache（WebDAV）与 Turborepo v8

> Hotpot 同时实现两套**远端缓存协议**。它们不是重复劳动，而是覆盖两类完全不同的生态。

## 为什么是两套协议

| | sccache（WebDAV 兼容） | Turborepo v8 |
|---|---|---|
| 面向生态 | Rust / C / C++（任何 `rustc`/`gcc` 编译） | JS / TS monorepo（turbo 生态） |
| 缓存粒度 | 单个 crate 的编译产物（`.rlib`/`.o`/metadata） | 单个 task 的输出目录（gzip-tar） |
| 协议形态 | WebDAV 子集（GET/HEAD/PUT/MKCOL/PROPFIND） | 固定 REST（`/v8/artifacts/{hash}`） |
| 租户模型 | 无（key 即内容哈希，自带分片路径） | `teamId` / `slug` 强制隔离 |
| 完整性 | ZIP CRC32，损坏表现为静默 miss | HMAC-SHA256 签名（`x-artifact-tag`） |
| miss 信号 | GET 404 / 读取失败 | **仅** GET 404 |

一句话：**sccache 让 Hotpot 服务好 Rust 自己，Turborepo v8 让 Hotpot 顺手服务好前端 monorepo。**
两者共用同一份内容寻址存储（blake3 CAS）与同一套索引，因此边际成本极低——
多一个协议只是多一组 HTTP 路由，存储层零改动。

## 文档

| 文档 | 内容 |
|------|------|
| [sccache WebDAV 兼容协议](sccache-webdav.md) | URL 分片布局、方法与状态码、写前探测序列、`.sccache_check` 契约、正确环境变量、curl 实操 |
| [Turborepo v8 兼容协议](turborepo-v8.md) | 端点表、`teamId` 前缀硬规则、签名与内容完整性、404 是唯一 miss、curl 与真实 `turbo` 实操 |

## 共享实现

```
HTTP 路由（sccache / v8）          ← 两套协议语义在此收敛
        ↓
hotpot-cacheproto::RemoteCache     ← 命名空间隔离（sccache | turbo）+ 租户 key
        ↓
SQLite 索引 kv_entries             ← (namespace, tenant, cache_key) → digest
        ↓
hotpot-store::LocalStore (CAS)     ← blake3 内容寻址，256 桶，zstd，LRU
```

关键设计：**载荷对服务端完全 opaque**。sccache 存的是 ZIP+zstd，turbo 存的是
gzip-tar，两者的内部结构、版本、校验方式都不同。Hotpot 只按 key 存字节、
按 key 取字节，永远不解析内容——这正是内容寻址存储能同时服务任意协议的原因。

## 安全提醒

缓存端点**默认不鉴权**（单机自托管的零配置体验）。但一旦监听非回环地址，
`PUT` 就是**构建供应链投毒面**：任何人都能写入一个伪造产物，之后所有机器都会命中它。

```bash
# 对外暴露时必须设置 token
export HOTPOT_CACHE_TOKEN=$(openssl rand -hex 32)
```

设置后：
- sccache 侧用 `SCCACHE_WEBDAV_TOKEN` 携带同一个 token；
- turbo 侧用 `TURBO_TOKEN` / `--token`。

注意：sccache 与 turbo 都会把 **401 当作硬错误**而不是 cache miss。
所以 token 配错的表现是「客户端构建直接失败」，而不是「命中率悄悄下降」——
这是有意为之的 fail-fast 设计。
