# Turborepo 远端缓存协议 v8（兼容实现）

> 面向使用者与维护者。目标是让你**照着就能把 `turbo` 指向 Hotpot**，
> 并且知道哪些行为是协议强制的、哪些是 Hotpot 的选择。

---

## 1. 这是什么

[Turborepo](https://turbo.build) 的 Remote Cache 把 monorepo 里每个 task 的输出
（gzip-tar）存到远端。CI 或同事的机器上只要输入指纹一致，就能直接还原产物，
跳过执行——日志里会打印 `>>> FULL TURBO`。

Hotpot 实现了它的 **v8 HTTP 端点**，因此可以把前端构建缓存也放进同一套
自托管基础设施，与 Rust 构建共享同一份存储。

## 2. 快速开始

```bash
export TURBO_API=http://127.0.0.1:7878      # 注意：不要带结尾斜杠
export TURBO_TOKEN=<与 HOTPOT_CACHE_TOKEN 相同>
export TURBO_TEAM=my-team                   # → ?slug=
export TURBO_TEAMID=team_myteam             # → ?teamId=（必须 team_ 前缀）

# 冷：miss 并执行
turbo run build --cache=local:rw,remote:rw

# 清掉本地缓存，验证「纯远端命中」→ 期望 >>> FULL TURBO
rm -rf node_modules/.cache/turbo .turbo/cache
turbo run build --cache=local:,remote:rw
```

等价的命令行形式：

```bash
turbo run build --api http://127.0.0.1:7878 --token "$TOKEN" --team my-team \
  --cache=local:,remote:rw
```

## 3. 版本说明

- 客户端把 `/v8` **硬编码**在源码里，**没有版本协商**。
- 官方明确声明「所有版本的 `turbo` 都兼容 v8 端点」。
- 官方 OpenAPI spec 里的路径**不含** `/v8`（写的是 `/artifacts/{hash}`），
  `/v8` 前缀是客户端实现细节。写文档/自研客户端时别被这点绕进去。

## 4. 端点表

| Method | 路径 | Query | 关键请求头 | 成功响应 |
|--------|------|-------|-----------|---------|
| `GET` | `/v8/artifacts/status` | `teamId?` `slug?` | `Authorization` | `200` + `{"status":"enabled"}` |
| `HEAD` | `/v8/artifacts/{hash}` | `teamId?` `slug?` | `Authorization` | `200`（存在）/ `404` |
| `GET` | `/v8/artifacts/{hash}` | `teamId?` `slug?` | `Authorization` | `200` + `application/octet-stream` + `x-artifact-tag?` |
| `PUT` | `/v8/artifacts/{hash}` | `teamId?` `slug?` | `Content-Type`、`Content-Length`、**`x-artifact-tag?`**、`x-artifact-duration?` | `201`（Hotpot；客户端接受任意 2xx） |
| `OPTIONS` | `/v8/artifacts/{hash}` | — | `Access-Control-Request-*` | `204` + `Access-Control-Allow-Headers` |

鉴权**只有** `Authorization: Bearer <token>`（`TURBO_TOKEN` / `--token`）。
**不存在 `x-api-key`**。Hotpot 未配置 `HOTPOT_CACHE_TOKEN` 时不校验。

`{hash}` 是 task 指纹的内容寻址哈希（通常 32 位十六进制）。

## 5. 三个必须知道的硬规则

### 5.1 `teamId` 必须以 `team_` 开头，否则被**静默丢弃**

客户端代码只接受 `team_` 前缀的 teamId，不满足就**不发送该参数**。

```bash
# ✅ 正确
curl "$B/v8/artifacts/$H?teamId=team_demo"
# ❌ 无效：客户端会直接丢弃 teamId，退化成无租户
curl "$B/v8/artifacts/$H?teamId=demo"
```

实践中 `--team` / `TURBO_TEAM` 走的是 `slug`，与 `teamId` 是**两个独立命名空间**
（Hotpot 不做 `slug ↔ teamId` 映射）。

### 5.2 404 是**唯一**的 cache miss 信号

```
404                    → cache miss，正常降级
403 + remote_caching_* → 缓存被禁用（解析状态并提示）
其它非 2xx（含 500）    → 硬错误，中断构建
```

这意味着：**你的服务端返回 500 不是「缓存不可用」，而是「构建会挂」**。
Hotpot 对所有内部错误返回 500，所以索引/CAS 出问题时需要立刻关注。
反过来说，如果想让 turbo 安静地只用本地缓存，正确做法是返回 **404**，不是 401。

### 5.3 内容完整性：`x-artifact-tag` 必须原样回显

当 `turbo.json` 里设置 `remoteCache.signature = true`（并提供
`TURBO_REMOTE_CACHE_SIGNATURE_KEY`）时：

- 上传时客户端计算 HMAC-SHA256 签名，放在 `x-artifact-tag` 头；
- 下载时**必须**拿到该头，否则报 `ArtifactTagMissing`——这是**硬错误**，不是 miss；
- 签名消息为 `len("artifact-signature:v2") ‖ 前缀 ‖ len(hash) ‖ hash ‖
  len(team_id) ‖ team_id ‖ body_len ‖ body`，标准 base64。

**服务端不计算、也无法计算签名**（消息含客户端私有的 key 与 team_id）。
唯一正确的做法是：**存下来、原样回显、绝不自行修改**。

> 协议里**没有** `If-None-Match` / `ETag` / `Content-Range` / 409 / 412。
> 所谓「不可变」由三件事共同保证：
> ① hash 本身内容寻址；② HMAC 把 (hash, team_id, body_len, body) 绑死；
> ③ 服务端 first-write-wins。
>
> **必须守住的不变量**：body 与 tag 必须配对。覆盖 body 却保留旧 tag
> （或反之）必然验签失败。Hotpot 的做法是「body 首次写入即固定，
> tag 只在缺失时补齐」，由 `turbo_backfills_tag_on_existing_entry` 测试钉住。

## 6. 请求时序

### 6.1 上传（PUT）

```
（可选）OPTIONS 预检          → 需要回 Access-Control-Allow-Headers
PUT /v8/artifacts/{hash}?teamId=…&slug=…
    Content-Type: application/octet-stream
    Content-Length: <必填，spec 标 required>
    x-artifact-duration: 1234
    x-artifact-tag: <签名，可选>
    x-artifact-sha: <commit，可选>
    x-artifact-dirty-hash: <可选>
    x-artifact-client-ci: <仅 CI 且 vendor 常量存在时发送>
    <gzip-tar 字节流>
→ 任意 2xx 即成功（Hotpot 返回 201）
```

客户端会用 `error_for_status()`，任意 2xx 都通过。连接错误会**重试**，
因此 **PUT 必须幂等**（Hotpot 按 key 幂等：同 key 不覆盖 body）。

### 6.2 下载（GET）与存在性（HEAD）

```
GET /v8/artifacts/{hash}?teamId=…&slug=…   (或 HEAD)
→ 200 + application/octet-stream
      Content-Length: <必须等于真实 body 长度>
      x-artifact-tag: <若上传时带过，必须回显>
      x-artifact-duration / x-artifact-sha / x-artifact-dirty-hash: <回显>
→ 404 表示 miss
```

`Content-Length` 会被喂进签名的 HMAC 消息，长度不符会导致验签失败。
Hotpot 用实际字节数填 `Content-Length`，并在 HEAD 上同样返回真实体积。

## 7. curl 实操

```bash
B=http://127.0.0.1:7878
A="Authorization: Bearer $HOTPOT_CACHE_TOKEN"   # 未启用鉴权时可省略
TEAM=team_demo

# 状态
curl -s "$B/v8/artifacts/status?teamId=$TEAM"; echo

# 上传
printf 'hello hotpot\n' > /tmp/hotpot-demo.txt
tar -czf /tmp/hotpot-demo.tar.gz -C /tmp hotpot-demo.txt
curl -i -X PUT "$B/v8/artifacts/$HASH?teamId=$TEAM" -H "$A" \
  -H 'Content-Type: application/octet-stream' \
  -H "Content-Length: $(stat -f%z /tmp/hotpot-demo.tar.gz)" \
  -H 'x-artifact-duration: 1234' \
  -H 'x-artifact-tag: dGVzdC1zaWduYXR1cmUtdmFsdWU=' \
  --data-binary @/tmp/hotpot-demo.tar.gz

# 下载并检查头
curl -i "$B/v8/artifacts/$HASH?teamId=$TEAM" -H "$A" -o /tmp/dl.tar.gz
grep -iE 'content-length|x-artifact' /tmp/dl.tar.gz.hdr 2>/dev/null

# HEAD 存在性
curl -i -I "$B/v8/artifacts/$HASH?teamId=$TEAM" -H "$A"

# miss 语义：必须 404
curl -o /dev/null -s -w '%{http_code}\n' "$B/v8/artifacts/deadbeef?teamId=$TEAM" -H "$A"

# 租户隔离：同 hash 不同 team → 404
curl -o /dev/null -s -w '%{http_code}\n' "$B/v8/artifacts/$HASH?teamId=team_other" -H "$A"
curl -o /dev/null -s -w '%{http_code}\n' "$B/v8/artifacts/$HASH?slug=other" -H "$A"

# 预检
curl -i -X OPTIONS "$B/v8/artifacts/$HASH?teamId=$TEAM" -H "$A"
```

## 8. Hotpot 侧实现要点

| 关注点 | 做法 |
|--------|------|
| 租户 | `teamId` 优先，回落 `slug`，再回落空串；不匹配一律 404 |
| 载荷 | gzip-tar 原样入 CAS，opaque |
| 签名 | 只存与回显 `x-artifact-tag`，**不计算**；已有条目缺失时补齐 |
| 元数据 | 存并回显 `x-artifact-duration` / `-sha` / `-dirty-hash`（缺失时 turbo 记 time saved=0，不影响正确性） |
| 幂等 | PUT 按 key first-write-wins，连接重试安全 |
| 状态码 | PUT `201`（与官方 mock server 一致；spec 文档化为 200/202，客户端接受任意 2xx） |
| 体积上限 | `DefaultBodyLimit` 4 GiB，超限 `413` |
| 鉴权 | 可选 Bearer，常量时间比较 |
| 预检 | `OPTIONS` 返回 `Access-Control-Allow-Headers`（含 `Authorization`） |
| 测试 | `tests/routes.rs` 覆盖往返、租户隔离、404 语义、tag 补齐、预检 |

## 9. 已知边界（诚实清单）

| 项 | 状态 | 影响 |
|----|------|------|
| `POST /v8/artifacts`（批量状态查询） | 未实现 | 仅影响 `turbo` 的 dry run；上游有测试证明会**自动回退**到 HEAD |
| `POST /v8/artifacts/events`（分析事件） | 未实现 | 可选端点；404 容忍度未实测 |
| 302 重定向到对象存储 | 未实现 | 无法把大 artifact offload 到 S3 |
| 流式上传/下载 | 未实现 | 目前整块进内存，受 4 GiB 上限保护；大 artifact 场景下这是下一个优化点 |
| `x-api-key` 鉴权 | 不适用 | 上游只有 Bearer |

## 10. 排障速查

| 现象 | 优先排查 |
|------|---------|
| turbo 一直 miss | ① `TURBO_API` 是否带尾斜杠（会变成 `//v8/…`）；② `teamId` 是否漏了 `team_` 前缀 |
| turbo 直接报错（不是 miss） | 服务端返回了非 404 的错误；看服务端日志与 token 配置 |
| 开了签名后报 `ArtifactTagMissing` | 服务端没回显 `x-artifact-tag`；确认 PUT 时带了该头 |
| 租户之间互相看不到 | 预期行为：`slug` 与 `teamId` 是独立命名空间 |
| 启用了 `--preflight` 就失败 | 服务端缺 `OPTIONS`/CORS 头（Hotpot 已实现） |
