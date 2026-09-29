# 分布式构建与远程缓存系统技术调研

> 调研日期：2026-09
> 目的：为 Rust 远程打包服务（remote build / packaging service）提供架构借鉴
> 调研方法：官方文档、proto 原文（remote-apis v2.12.0）、GitHub API 实测、厂商工程博客、学术论文
> 配套文档：`docs/research/`（01 为本项目问题域分析，本篇为外部系统横向调研）

---

内容顺序：问题框架 → Bazel/REAPI → REAPI 服务端 → Buck2/Pants → Goma/reclient/siso → distcc/ccache/icecream → sccache → Nix → Turborepo/Nx → BuildKit/Dagger/Earthly → Rust 生态 → 增量正确性 → 总对比表 → 架构启示 → 参考资料。

---

## 1. 问题框架：分布式构建的两个根本性质

论文 *Build Systems à la Carte*（Mokhov / Mitchell / Peyton Jones，ICFP 2018，JFP 2020 扩展版）把所有构建系统拆成四个正交组件：**依赖描述、调度器、重建判定器（rebuilder）、存储**，并给出两个贯穿本报告的判据：

- **正确性（correctness）**：输入发生变化时，变化必须传播到所有受影响的输出。漏一个文件、漏一个环境变量，就是错误产物。
- **最小性（minimality）**：只重建输出内容真正发生变化的任务。依赖被重新构建但内容不变时，不应引发下游连锁重建。

| 机制组合 | 正确性 | 最小性 | 代表系统 |
|---|---|---|---|
| mtime 时间戳 | 不保证（时钟偏差、checkout 旧时间戳） | 差（内容不变也连锁重编） | Make |
| mtime + restat 验证 | 较好 | 中 | Ninja |
| 内容哈希 + 动态依赖记录 | 好 | 好 | Shake、Buck2 |
| 内容哈希 + hermetic action + CAS/AC | 最好（跨机器可信） | 好 | Bazel、REAPI 生态 |

**分布式让正确性问题被放大**：单机构建里一个"脏读"只坑一个开发者；远程缓存里一个错误条目（cache poisoning）会被全公司命中。因此所有成功的远程构建系统都遵循同一个隐含契约：

> **远程缓存命中的正确性 = action 自描述（declared inputs 完备）+ 执行确定（hermetic / deterministic）+ key 计算把所有影响输出的输入编码进去。**

**缓存粒度谱系**（从粗到细，越细去重越好但 key 计算越贵）：整机构建（早期 Forge、CI 快照）> 任务级（Turborepo、Nx、Earthly）> action 级（Bazel、Buck2、Pants、REAPI）> 编译单元级（distcc、ccache、sccache、Goma）> 内容块级（CAS blob、Nix nar、BuildKit layer、CDC chunk）。

关键工程结论：**粒度和正确性解耦可以分层共存**。Bazel 内部同时存在 action 级 AC（语义层）和 blob 级 CAS（字节层）；sccache 是编译单元级 key 加对象存储后端；我们的 Rust 服务也应当设计多层缓存而不是只选一个粒度。

---

## 2. Bazel：现代远程构建的事实标准

Bazel（Google 内部系统 Blaze 的开源版，2015 年开源）是远程缓存/远程执行设计的集大成者。截至 2026-09，Bazel 仓库约 25.9k stars，Apache-2.0，Java 编写，仍非常活跃。

### 2.1 Hermetic builds（密封构建）

官方定义：build/test actions 不受任何外部影响，相同源码加相同配置必然得到确定结果。两个支柱：

1. **隔离（Isolation）**：工具本身当作源码管理——pinned 工具链下载到受管目录，不依赖宿主机安装的软件（rules_go 自带 Go SDK；hermetic CC 工具链；rules_rust 下载 rustc）。
2. **输入身份化（Source identity）**：所有输入用唯一哈希标识（git commit、依赖 tarball 的 SHA-256）。

典型约束：action 无网络访问、只能读 declared inputs、固定 timestamp/timezone、环境变量白名单、固定 RNG seed。
Non-hermetic 的常见来源：宿主机 `/usr/bin` 二进制、绝对路径、`__TIME__/__DATE__`/build-id、向 source tree 写文件、隐式读取 `LANG`/`HOME` 等环境。

**hermeticity 是 action key 跨机器可信的前提**，不是洁癖。

### 2.2 三阶段与 action graph

Bazel 构建分阶段：

1. **Loading**：解析 WORKSPACE/MODULE.bzl 与 BUILD 文件；
2. **Analysis**：求值规则，在内存中生成 **action graph**——DAG 节点是 actions（一次命令调用：命令行、action key、环境变量、declared inputs/outputs）与 artifacts（source 或 generated）；
3. **Execution**：按 action graph 调度执行。

Bazel 的正确性模型是 **artifact-based 而非 task-based**。官方文档明确指出：task-based 系统几乎不可能提供可靠 RBE 所需的三个保证——环境完全自描述、每个 action 自包含可在任意机器执行、输出完全 deterministic 使 worker 之间可以互信结果。可用 `aquery` 检查 action graph、`cquery` 检查 configured target。

### 2.3 Remote caching 与 remote execution

- **Remote cache**：AC + CAS 两个存储，只读/只写/读写均可独立配置。只接缓存时，Bazel 本地执行、把结果上传并在后续构建命中。
- **Remote execution**：把 action 整个发给远端 worker 执行。
- 两者是同一套 API（REAPI v2），cache 是 execution 的一个短路：远端先查 AC，命中直接返回 ActionResult，未命中才调度执行。

### 2.4 Platforms 与 toolchains

- `constraint_setting`（一个维度，如 CPU）→ `constraint_value`（x86_64/arm64）→ `platform`（一组约束值，描述一台完整机器）。
- 三种平台角色：**host**（Bazel 运行处）、**execution**（action 运行处，含远程 worker）、**target**（产物运行处），交叉编译时三者分离。
- **Toolchain resolution**：规则通过 `register_toolchains()` 注册工具链，Bazel 按 execution/target platform 的约束自动解析。
- 与 RBE 的衔接：platform 上的 `exec_properties`（字符串 map，如 `OSFamily=Linux`、`container-image=docker://...`、`Pool=large`、`priority=N`）写进 REAPI `Action.platform`，服务端据此匹配 worker。

### 2.5 调度：strategies、dynamic racing、retry、fallback

- **策略链**：按 action mnemonic 选策略，典型顺序 `remote, worker, sandboxed, local`，不兼容自动降级。
- **Dynamic execution（本地+远程竞速，0.21 起）**：同一 action 同时启动本地与远程执行，先完成者胜出，另一个被 cancel。语义刻意严格：**任一分支先返回失败则整个 action 失败**，防止本地/远程行为差异被掩盖。参数 `--dynamic_local_execution_delay=1000`（远程侧显示 cache 命中后本地分支延迟启动，避免白跑）。适合干净构建吃远程算力、增量构建吃本地低延迟。
- **重试与熔断**：`--remote_retries=5` + 指数退避/jitter（cap 5s）；检测到缓存条目被淘汰（输入 blob 丢失）时换新 invocation ID 重跑整个构建；错误率超阈值的熔断器；`--remote_local_fallback` 远程失败优雅降级本地。
- **优先级**：REAPI `ExecutionPolicy.priority`（0 默认，负数更优先，best-effort 非抢占）。

### 2.6 沙箱隔离

每个 action 有独立 `execroot/<sandbox>` 工作目录，只含 declared inputs、只回收 declared outputs。

| 策略 | 机制 | 隔离强度 |
|---|---|---|
| standalone/local | 直接在 workspace 执行 | 无 |
| processwrapper-sandbox | 全 symlink 的输入目录，可移植 POSIX 兜底 | 防 undeclared input，无 FS/网络隔离 |
| linux-sandbox | Linux namespaces：User/Mount/PID/Network/IPC，全 FS 只读、唯一可写 sandbox、可断网、PID 收割杀净子进程 | 强（类容器） |
| darwin-sandbox | macOS sandbox-exec（Seatbelt profile） | 强 |

OS sandbox 不能嵌套（容器内需回退 processwrapper 或加 `--privileged`）。远程侧隔离强度排序：**Firecracker microVM > 容器/namespace > chroot**。

### 2.7 规模数据（Google 官方口径）

Google 官方 Distributed Builds 页面（2026 年仍可访问）：

> "Google runs **millions of builds** executing **millions of test cases** and producing **petabytes** of build outputs from **billions of lines of source code every day**."

系统自 **2008 年**运行，分两部分：

- **ObjFS（远程缓存）**：构建产物存 Bigtable；每台开发机跑 FUSE daemon（objfsd）像浏览本地文件一样浏览产物，内容按需下载，比全量落盘快约一倍。
- **Forge（远程执行）**：Blaze 的 Distributor 把每个 action 发到数据中心；Scheduler 维护 action 结果缓存，Executor 池持续取任务执行、结果直写 ObjFS。

学术补充（Potvin & Levenberg, CACM 2016）：当时 Piper 约 20 亿行代码、build farm 数万台机器。

### 2.8 Bazel 的优缺点

- 优点：正确性模型最完整；REAPI 开放协议带来多后端/多客户端生态；hermetic 工具链成熟；大规模验证二十年。
- 缺点：Analysis 阶段 JVM 开销与内存占用；规则学习曲线陡；Bzlmod/WORKSPACE 迁移成本；对"非规则化"的遗留 C/C++/Rust 项目侵入性大；链式 RPC（上传输入→Execute→下载输出）在小 action 上延迟敏感。

---

## 3. REAPI 核心数据结构精读

> 仓库：[bazelbuild/remote-apis](https://github.com/bazelbuild/remote-apis)，Apache-2.0。
> 截至 2026-09 最新稳定 tag 为 **v2.12.0**（v2.11 未出正式 tag）。以下字段以 v2.12.0 `remote_execution.proto` 为准。
> 这一节是自研协议设计的直接参考，REAPI 把"一条命令的远程执行"抽象成了一个 **Merkle-DAG**，值得逐字理解。

### 3.1 Digest：一切身份的基础

```proto
message Digest {
  string hash = 1;        // 小写 hex，前导零补齐，如 SHA-256
  int64  size_bytes = 2;  // blob 字节数，是 digest 不可分割的一部分
}
```

设计要点：

- size 内嵌 digest：服务端 flatten Merkle tree、流式传输前常需先知道大小，省一次 metadata 查询。
- Digest 指向 proto message 时，指其 **canonical binary encoded form**：字段按 tag order、无 unknown fields、无重复字段。等价消息必须序列化出相同字节，否则跨实现 cache 不互认。
- v2.12 支持的 digest function：`SHA256`（事实默认）、SHA1、MD5、VSO（微软 paged SHA256）、SHA384/512、MURMUR3、`SHA256TREE`（Merkle 化分块 SHA256）、**BLAKE3**。不同 hash 函数的 blob 处于不同命名空间，切换 hash 等于清缓存；同一 request 的输入不能混用两种 hash。

### 3.2 Command：做什么

```proto
message Command {
  message EnvironmentVariable { string name = 1; string value = 2; }
  repeated string arguments = 1;                 // argv
  repeated EnvironmentVariable environment_variables = 2; // 必须按 name 字典序
  string working_directory = 6;                  // 相对 input root
  repeated string output_paths = 7;              // v2.1+，不区分文件/目录
  repeated string output_node_properties = 8;    // 按 name 排序
  OutputDirectoryFormat output_directory_format = 9;
  // platform 已在 v2.2 移至 Action
}
```

要点：**所有列表都有规范化（canonicalization）要求**——env 按 name 排序、output paths 去重并按 UTF-8 code point 排序。这些排序规则是协议的一部分，不是建议。

### 3.3 Directory / FileNode / DirectoryNode / SymlinkNode：Merkle 输入树

```proto
message Directory {
  repeated FileNode files = 1;          // 应按 name 排序
  repeated DirectoryNode directories = 2;
  repeated SymlinkNode symlinks = 3;
  NodeProperties node_properties = 5;   // mtime / unix_mode 等
}
message FileNode {
  string name = 1;
  Digest digest = 2;                    // 文件内容（存 CAS）
  bool is_executable = 4;
  NodeProperties node_properties = 6;
}
message DirectoryNode { string name = 1; Digest digest = 2; }
message SymlinkNode   { string name = 1; string target = 2; }
```

输入根是一棵 Merkle tree：每个 Directory 独立序列化存 CAS，父节点只持有子 digest。去重粒度细到"相同目录结构 / 相同文件内容"；增量改动只重传受影响的子树（这是 action 级缓存下大仓库仍可扩展的关键——input root digest 计算与文件内容上传都随目录增量复用）。

### 3.4 Action：密封的工作单元（缓存 key 本身）

```proto
message Action {
  Digest command_digest = 1;                    // CAS 中的 Command
  Digest input_root_digest = 2;                 // 输入根 Directory
  google.protobuf.Duration timeout = 6;         // 超时（不含排队）
  bool do_not_cache = 7;                        // 不缓存、不参与 in-flight 合并
  bytes salt = 9;                               // 缓存命名空间隔离/毒化后整体弃用
  Platform platform = 10;                       // 平台属性（worker 匹配）
}
```

要点：

- **Action 的 wire-format digest 就是 ActionCache 的 key**。
- `timeout` 入 key 是刻意设计：短 timeout 不能命中长 timeout 条目，避免错误被缓存掩盖。
- `salt` 是运维级的"缓存版本号"：发现编译器 bug 或缓存毒化后，换 salt 即可让全量条目逻辑失效，无需物理清空。
- Action 引用的所有 Directory 与内容 blob 必须先在 CAS 中。

### 3.5 ActionResult / OutputFile / OutputDirectory / Tree：执行结局

```proto
message ActionResult {
  repeated OutputFile output_files = 2;
  repeated OutputDirectory output_directories = 3;
  repeated OutputSymlink output_symlinks = 12;
  int32 exit_code = 4;
  bytes  stdout_raw = 5;  Digest stdout_digest = 6;
  bytes  stderr_raw = 7;  Digest stderr_digest = 8;
  ExecutedActionMetadata execution_metadata = 9;
}
message OutputFile {
  string path = 1; Digest digest = 2;
  bool is_executable = 4;
  bytes contents = 5;   // 仅在请求 inline 时
}
message Tree { Directory root = 1; repeated Directory children = 2; }
```

要点：

- **exit_code 非 0 的结果也缓存**（失败的测试可复用）；只有 transport/执行层 status 非 OK 才不写缓存。
- 大输出走 digest 引用 CAS，不塞进 ActionResult；stdout/stderr 默认也是 blob 引用。
- `ExecutedActionMetadata` 记录全时间线：queued / input_fetch / execution / output_upload 各阶段时间戳、worker 名，是调度可观测性的数据基础。

### 3.6 五大 gRPC 服务

| 服务 | 核心 RPC | 作用 |
|---|---|---|
| **ContentAddressableStorage** | `FindMissingBlobs`、`BatchUpdateBlobs`、`BatchReadBlobs`、`GetTree` | blob/Command/Directory/Tree 存取；上传前批量探活实现增量上传 |
| **ActionCache** | `GetActionResult`、`UpdateActionResult` | action digest → ActionResult |
| **Execution** | `Execute`（返回 longrunning Operation 流）、`WaitExecution`（断线重连） | 异步执行；stage 流：CACHE_CHECK → QUEUED → EXECUTING → COMPLETED |
| **Capabilities** | `GetCapabilities` | 协商支持的 hash 函数、压缩器、批量上限、优先级范围、符号链接策略 |
| **ByteStream** | `Read`（server streaming）、`Write`（client streaming，断点续传） | 大 blob 传输；resource name 编码 instance/hash/size |

相关协议：

- **Remote Asset API**：`FetchBlob`/`FetchDirectory`，让服务端代取外部 URL（解决 action 不能联网下载依赖的问题）。
- **BEP / Build Event Service**：invocation 级事件 DAG，双向流式上传、按序号 ack；Bazel 结果 UI 的事实标准。

### 3.7 典型端到端流程

1. 客户端本地构建输入 Merkle tree（Directory 递归 digest）；`FindMissingBlobs` 批量探活后，经 `BatchUpdateBlobs` / ByteStream 只上传缺失 blob（Command、各 Directory、文件内容）。
2. 发送 `Execute(Action)` → 服务端先查 AC：hit 直接返回 ActionResult；miss 则入队、按 platform 调度给 worker。
3. Worker 从 CAS 拉输入树 → 沙箱执行 → 输出上传 CAS、写 AC。
4. 客户端经 Operation 流收到 stage 更新与结果，再用 ByteStream 按需下载输出。

### 3.8 v2.12 新趋势：大 blob 的内容定义分块进入协议

- 新增 `SplitBlob` / `SpliceBlob`：大 blob 按 **content-defined chunking（CDC）** 切分（非定长），client 只传缺失 chunk，服务端按 chunk 拼回。背景是 BuildBuddy 主导的 CDC 实践被上游协议化。
- 工程价值：未大改的大产物（链接后的二进制、打包镜像）也能复用大部分字节。BuildBuddy 数据：上传量减少 40%、磁盘占用减少 40%。

---

## 4. REAPI 服务端生态

### 4.1 BuildBuddy

- 仓库 [buildbuddy-io/buildbuddy](https://github.com/buildbuddy-io/buildbuddy)，**Go，核心 MIT Expat**（`enterprise/` 目录商业许可）。产品形态：开源核心 + 云（个人/开源免费）+ Enterprise on-prem。功能覆盖 cache、RBE、结果分析 UI、Workflows CI、Remote Bazel。
- 架构：无状态 server（gRPC/HTTP 接 REAPI）+ 分布式 cache（磁盘/S3/GCS，可分层）+ executor 集群 + MySQL 元数据。
- **Firecracker microVM runner（2026 年重点博客 "Snapshot, Chunk, Clone"）**：用 Firecracker 快照（内存+磁盘+内核）维持预热 worker 池，CoW 网络克隆；快照按内容分块存 CAS，同宿主机 VM 复用 chunk，可保留 Bazel analysis graph/JVM 堆。实测：自仓全缓存构建约 6 分钟中 CI 机器初始化占 3.5 分钟；快照 runner 后**中位 CI 运行时间 30 秒**，小仓 6 秒。
- 支持优先级（-1000..1000）、Docker 与 microVM 两种隔离、macOS runner。
- 优点：开源功能完整、UI/文档最好、microVM 隔离强；缺点：stars 量级不大（约 791）、企业功能闭源、自托管组件较多。

### 4.2 EngFlow

- 2020 年成立，创始团队是 Google Bazel 核心：CEO Helen Altshuler、CTO Ulf Adams（在 Google 领导 Blaze/Bazel 11 年）。2022 年 Tiger Global 领投 1800 万美元 A 轮。
- 产品：RBE + cache + Build/Test UI，支持客户自有云部署；兼容 Bazel、Buck2、CMake（recc）、AOSP、Chromium。官方口径：**1–100,000+ cores、构建快 5–10×、云成本降 20–50%**。客户含 Databricks、Snap、ARM、BMW、Canva、Lyft。
- BazelCon 2025 数据点：EngFlow + Databricks 演讲 "Use Build Data to Manage **1 Billion Actions a Day**"。
- 核心产品**闭源**；公司雇佣多名 Bazel/rules 维护者、积极贡献 REAPI。

### 4.3 bazel-remote

- [buchgr/bazel-remote](https://github.com/buchgr/bazel-remote)，**Go，Apache-2.0**，约 775 stars，2026 年仍有更新。**仅 cache、无 execution**。同时提供 HTTP/1.1 REST 与 gRPC REAPI（AC/CAS/Capabilities/ByteStream）。
- 磁盘 LRU 淘汰；后端可 proxy 到 S3/GCS/Azure/另一个 HTTP cache；HTTP Basic、mTLS。官方称自 2018 年起生产承载 TB/天流量、合适机型出口 >15 Gbit/s。
- 定位：单二进制部署极简单，是"先接远程缓存"的最低成本起点；但无执行、无 UI、无分层。

### 4.4 Buildbarn

- [buildbarn](https://github.com/buildbarn) 组织，**Go，Apache-2.0**，云原生组件化最彻底的开源栈：
  - **bb-storage**：CAS+AC 核心。本地后端为 **circular storage**（环形文件块 + hash 索引，代际切换、自清理、无 GC）；云后端 S3/Redis/Etcd；无状态 frontend 做 fan-out/sharding；store pipeline 可组合（配额、复制、镜像）。
  - **bb-remote-execution**：`bb_scheduler`（排队 + platform 匹配）、`bb_worker`（拉输入/编排/上传，跨 action hardlink 复用输入）、`bb_runner`（真正执行命令，特权分离，可换 QEMU user-mode 或注入任意容器镜像）。
  - **bb-clientd**：FUSE（Linux）/NFSv4（macOS）挂载 CAS 的 lazy 客户端；**Bazel Output Service** 的先发实现（2024 年上游 Bazel 7.2），用虚拟目录替代实体 bazel-out，兼作本地 REAPI 缓存代理。
- 优点：存储设计成熟、K8s 原生、全部 Apache-2.0 复用干净；缺点：组件多、运维曲线陡、默认 chroot 级隔离需自建容器层、商业支持弱。实验性下一代系统 [bonanza](https://github.com/buildbarn/bonanza) 重设了编码与协议。

### 4.5 NativeLink（与我们技术栈最相关的项目）

- [TraceMachina/nativelink](https://github.com/TraceMachina/nativelink)，创建于 2020 年底，**Rust 编写**，约 1.6k stars，2026 年高度活跃。单二进制，角色由配置决定（CAS/AC/Scheduler/Worker 可合一可拆分）。
- **许可证需特别注意**：大部分代码为 **FSL-1.1-Apache-2.0**（Functional Source License，source-visible、有用途限制，约 2 年延迟期后自动转 Apache-2.0）；商业模块（metrics、persistent workers）为 BSL。早期版本是纯 Apache-2.0。**直接 fork 其代码需法务评估**；作为服务运行/个人缓存使用无问题。
- 架构：
  - CAS：SHA-256 或 BLAKE3（per-request 协商）；composable store 栈，支持 S3/GCS/Redis/Local/Memory 的 **tiered 分层**（对比 Go 对手的卖点）。
  - Scheduler：排队、platform 匹配、**in-flight action 去重合并**；默认状态在内存（两副本无法互相 dedup），HA/跨副本去重需 Redis state manager。
  - Worker：物化输入 → 超时执行 → 上传 CAS → **worker 自己写 AC** → 回报；支持 x86/ARM/GPU；可选 `single_use` 生命周期（一 action 一换可写卷，实现彻底隔离，仅重启进程不算）。
- 客户端兼容性：Bazel、Buck2、Pants、Siso、reclient、recc——任何 REAPI 客户端无需修改。
- 官方口径（注意官网 10B/1B requests 每月两个数字不一致）：构建加速 4–15×、cache hit 毫秒级、**0 GC pauses**（主打对比 Go 实现的 p99 抖动）；LLVM 案例（CMake+recc，无需改构建系统）全量 **17 分钟 → 4 分钟**。
- 优点：Rust 性能与内存安全、单二进制部署、分层存储、多构建系统；缺点：非纯 OSI 许可、公司年轻、无内建 microVM、UI 不及 BuildBuddy。

### 4.6 其他实现

| 项目 | 语言/许可 | 状态 | 说明 |
|---|---|---|---|
| **Buildfarm** | Java，Apache-2.0，约 775 stars | 活跃 | Worker/Shard 架构、磁盘/S3，Uber 等曾大规模使用；JVM/GC 调优是槽点 |
| **BuildGrid** | Python，Apache-2.0 | 活跃 | 配 BuildBox/recc 工具链适配 CMake/Make 最好；advertise 的 REAPI 版本偏旧（2.2） |
| **Scoot**（Twitter） | Go，Apache-2.0，约 362 stars | **事实停更**（约 2022 年冻结） | Scheduler + Worker + Snapshot 抽象，Thrift + REAPI 双协议；仅作 prior art |
| **Flare.Build** | 商业 | 已被收购 | Google 之外第一家 Bazel 后端服务商，擅长 Apple/macOS；2022-10 被 Bitrise 收购整合 |

**生态结论**：REAPI 是目前唯一一个让"构建系统"与"构建基础设施"可以独立演进、多方混搭的开放协议。客户端（Bazel/Buck2/Pants/reclient/siso）与服务端（9+ 种实现）构成两维生态。

---

## 5. Buck2：Meta 的 Rust 重写版

[facebook/buck2](https://github.com/facebook/buck2)，2023 年 4 月开源，核心 **Rust**，规则全部用 Starlark 外置，MIT OR Apache-2.0（约 4.4k stars，无 stable release，Meta 内部重度生产）。灵感来自 Bazel、Shake、Adapton 及 *Build Systems à la Carte*。

### 5.1 三张图与 DICE 引擎

- **Unconfigured graph（Evaluation）**：发现 package → 求值 BUCK/展开 macro → 未配置节点（`buck2 uquery`）。
- **Configured graph（Configuration）**：应用约束、解析 `select()`、platform resolution、transitions；同一 target 多配置可并存（`buck2 cquery`）。
- **Action graph（Analysis）**：规则运行并声明 actions/artifacts（`buck2 aquery`）。
- 但 Buck2 的核心论点是 **"Buck2 is not phased"**：整个构建跑在 **DICE** 增量计算引擎上——注册 leaf data 与 key→value 计算函数，自动建依赖图、跨"阶段"并行、同 key 共享；leaf 失效后沿反向边精确失效，节点带版本号做 **early cutoff**（重算值与旧值相等则不传播）。对比 Bazel 的 target graph → action graph 两阶段不可交错，Buck2 的增量更精确。Modern DICE 用单线程 core state thread 管理主状态，取代细粒度锁。

### 5.2 高级图特性

- **Dynamic dependencies（monadic）**：先生成文件、读取内容再声明后续依赖——用于 Haskell/OCaml import 跟踪、分布式 ThinLTO。
- **Anonymous targets**：超出用户目标图的共享（无关 binary 只编译一次共享代码）。
- **Transitive-sets**：类 Bazel depset 但直接嵌入图，减小图体积。

### 5.3 远程执行与 Materialization

- **原生 REAPI 客户端（Rust 自写）**，实测 EngFlow、BuildBarn、BuildBuddy、NativeLink。digest 默认 SHA256，可选 BLAKE3/SHA1。配置在 `.buckconfig [buck2_re_client]`。
- **hybrid execution**：同一 action 本地+远程竞速。
- **Materialization（关键差异化设计）**：action 完成或 AC 命中只拿到 output digest，产物下载时机分 eager 与 **deferred（默认）**——推迟到另一个本地 action 需要它、或最终交付时才下载。纯远程构建可以完全不下载中间产物。**Meta 内网实测约 2.5× 提速**。
  - 陷阱与配套：依赖 AC 条目 TTL 长于其引用构件（过期需 kill daemon，Restarter 缓解）；SQLite 物化状态、低磁盘水位激进清理（`clean_stale_artifact_ttl_hours=168`）；BXL/CLI 可指定 `materializations=all|none`。
- 与 **Eden/Sapling 虚拟文件系统**深度集成：文件不在本地也能算 hash（Bazel 无一等公民对应）。

### 5.4 规模与优缺点

- Meta：数千开发者、**每天数百万 builds**；比 Buck1 快约 2×，多数 CI 项目快 2–4×；无源码改动时 rebuild 几乎瞬时；Neil Mitchell 演讲称"can spawn 1000's of compiles"。
- 沙箱：RE 模式 hermetic（规则必须声明全输入，所有开发者可写共享缓存）；**local-only 构建尚无强沙箱**。
- 优点：单图跨阶段并行、Rust 无 GC、RE-first + deferred materialization、动态依赖表达力强。
- 缺点：无 stable release、外部生态/文档弱于 Bazel、Eden/TTL 刷新等 Meta 优化外部不可复现、冷缓存下第三方实测偶有不如 Bazel 成熟。

---

## 6. Pants v2：Rust 引擎 + Python 规则

[Pants](https://www.pantsbuild.org/) v2 引擎用 **Rust + Tokio 重写**，规则用 typed Python 3 编写（插件与内建同权），Apache-2.0，2026 年保持活跃版本节奏。常驻 daemon 保持 warm，把工作拆成大量小单元吃满所有 core。

### 6.1 Process 执行与缓存模型

规则作者通过返回 `Process` 对象让引擎执行外部命令，不接触调度/缓存：

- `argv`、`input_digest`（内容寻址 Merkle tree；用 `MergeDigests` 合并）、`env`（**禁止直接读 `os.environ`**，需经白名单）、`output_files/output_directories`、timeout、`cache_scope`。
- hermetic 执行：临时目录、不能读任意项目文件、剥离环境变量——key 因此总是精确。
- **本地缓存默认用 LMDB**（`~/.cache/pants/lmdb_store`）；`cache_scope` 可限 PER_SESSION。
- `ProcessResult` 含 stdout/stderr/output_digest；失败用 Fallible 变体，exit_code 非 0 也可缓存。

### 6.2 远程与 REAPI

远程缓存（CAS+AC）与远程执行（再加 Execution）全部走 **REAPI gRPC**，兼容 BuildBarn/Buildfarm/BuildGrid/NativeLink 等任何实现。

### 6.3 与 Bazel 的差异

- **依赖推断（dependency inference）**：静态分析 import 自动得依赖，BUILD 文件可极简；Bazel 传统显式 deps。
- 无 WORKSPACE：`pants.toml` + source roots + `pants tailor`；第三方依赖是普通 target。
- Python-first 体验顶级（Pex lockfile、内建 lint/type-check 并行）；规则作者写 Python 而非 Starlark、无需 JVM。
- 劣势：多语言广度与超大规模验证案例不如 Bazel/Buck2。

---

## 7. Goma → reclient → siso：Chromium 的二十年演进

### 7.1 Goma（已死）

Google 自研分布式编译器 + 远程缓存，服务 Chromium（GN+Ninja）、ChromeOS、Android 多年。架构：`gomacc` 前缀 + 本地 `compiler_proxy` 守护进程 + Google 运营的闭源服务端。**早于 REAPI，私有 RPC 跑在 HTTP/1 上**；客户端 2018 年开源（BSD-3），服务端从未开源。过渡期的 Goma/REAPI 翻译 proxy 被业内形容为 "heavy, expensive, and error-prone"。**2023-09 宣布弃用，2024 年 1 月底停服并从 Chromium 移除**；AOSP 更早转 RBE。

教训：**私有协议即使技术优秀，也会随着组织/团队变化而失去维护；开放协议是长期生存策略。**

### 7.2 reclient：REAPI 化的命令包装器

[bazelbuild/reclient](https://github.com/bazelbuild/reclient)，**Go，Apache-2.0**，原生 **REAPI 客户端（gRPC/HTTP2，不是 goma 协议）**：

- **rewrapper**：命令包装（对应 gomacc），通过 Unix domain socket/named pipe 连本地 reproxy。
- **reproxy**：每次构建启停的本地守护进程（对应 compiler_proxy），聚合全部请求，与 RBE 之间多路复用、批量、流式通信。
- bootstrap / scandeps_server（C/C++ include 扫描；自研 goma input processor 比 clang-scan-deps 在典型 Chrome 编译上快约 3×）。
- 执行策略：`local` / `remote` / `remote_local_fallback`（推荐）/ **`racing`**（本地远程竞速）。
- 集成 GN/Ninja 与 CMake（`CMAKE_*_COMPILER_LAUNCHER`）；可处理动作类型比 Goma 广。

### 7.3 siso：2025 年后的新主力

[siso](https://pkg.go.dev/go.chromium.org/build/siso)，**Go，BSD-3**，Ninja 的 drop-in 替代，为 RBE 从零设计：单进程内存共享避免重复 stat/disk IO、内置 REAPI 客户端（CAS bytestream、Merkle tree）、Starlark 配置。时间线：2023 dogfood → 2024 全部 CQ/发版 builder → **2025-04 内置 RE 客户端在 Chrome builder 取代 reclient** → 2025-08 迁至 go.chromium.org/build/siso → 2026-02 Android 开始迁移。

### 7.4 Chrome 规模数据

- 本地干净构建参考：高端单机（Ryzen 3990X 64 核 / 256 GB / Optane）约 **23 分钟（1429s）**；2017 年普通机器 non-jumbo 本地构建达 150–177 分钟（约每周增长 1%）。
- Goma 分布式时代 Google 内部 "a few minutes"，历史目标 "well under 10 minutes"。
- CI 常见机型 n1-standard-64 + SSD；第三方 RBE 报干净 AOSP 约 20 分钟。
- 注意：**link 阶段始终本地**；Google 没有公开统一的命中率/机器数口径，收益随 cache 温度与改动量变化。

---

## 8. distcc / ccache / icecream：C/C++ 时代的经典方案

### 8.1 distcc（2002 起，GPL-2.0，C）

- **模型**：client 本地预处理（cpp）得到自包含 `.i` → 发给 distccd → 远端只做 compile（-c）→ 回收 `.o`（+ `.d`）→ **链接始终本地**。
- **为什么链接不能分发**：ld 需要 target 的全部 `.o`、全部静态库 `.a` 与相关共享库；数据量大、IO/内存密集、产物单条不可切分，远端也没有完整库与 sysroot 布局，分发收益为负。distcc 官方明确只分发 compilation。
- **pump mode**：把预处理也移到远端——client 端 include server 做增量 include 分析、递归收集 header 打包；要求双方版本一致、必须开 LZO，与 ccache 不兼容，且不校验系统头一致性。
- 协议/调度：自研 TCP 文本协议；**无中央调度器**，纯客户端按 `DISTCC_HOSTS` 列表取有空 slot 的 host；`--randomize` 洗牌；失败 host 退避 60s；全失败回退本地。
- 安全：plain TCP 无加密无签名，历史 **CVE-2004-2687**（未认证 RCE）；不信任网络唯一安全选项是 SSH。
- 维护：最后正式版 3.4（2021-04），低速维护但未死。

### 8.2 ccache（2002 起，GPL-3.0/LGPL，C/C++）

- **Preprocessor mode**：key = 预处理输出 + 预处理 stderr + 命令行选项（剔除 `-I/-D`，因其已体现在预处理输出中）。
- **Direct mode（默认、更快）**：不做预处理，hash 源文件 + 编译选项 + common information → 定位 **manifest**（记录上次读到的 header 路径与各自 hash）→ 现算 header hash 与 manifest 比对，命中即取，miss 回退预处理并记录新 header 集合。**manifest 是避免 false cache share 的核心机制**（§14 详述）。
- **Depend mode**：direct hash + 编译器生成的 `.d` 依赖文件，miss 代价低。
- common information：编译器身份（默认 size+mtime，可选 content）、CWD（默认入 hash）、扩展名等。Hash 用 **BLAKE3（160-bit）**，校验 XXH3，默认 Zstandard 压缩。
- 典型 miss：`-g` 内嵌绝对路径（需 `-fdebug-prefix-map` + `base_dir`）、`__TIME__/__DATE__`、Clang modules（不理解二进制 module cache，需 sloppiness=modules）、C++20 named modules（基本不支持）。
- 维护：活跃，2026 年最新约 4.14.x。

### 8.3 icecream / icecc（2003 起，GPL-2.0，C++）

- 三组件：`icecc-scheduler`（元数据中心，自动选 master + failover）、`iceccd` worker（在 **chroot** 中执行）、compiler wrapper（数据直连 worker 不过 scheduler）。
- **智能调度**：默认 `fastest`（按机器速度 factor 与实时负载分给最快空闲者），优于 distcc 的静态列表；可选 random/round_robin/least_busy。
- **环境一致性**：`icecc-create-env` 把提交机工具链打成 tarball 发往 worker 缓存并在 chroot 展开；失败静默回退本地。
- 安全：无认证无加密，官方警告 "Never use in untrusted environments"；需 root。
- 维护：最后版 1.4（2022-03），进入低活跃。

**C/C++ 三件套的共同边界**：只覆盖编译这一动作类；链接、代码生成、打包全部留在本地；无 hermetic 模型，正确性靠"预处理输出自包含"这一个假设撑着。

---

## 9. sccache 与 Mozilla 的大规模共享缓存经验

[mozilla/sccache](https://github.com/mozilla/sccache)，**Rust，Apache-2.0**，client-server 模型（编译器前缀 → 本地 server `127.0.0.1:4226`，支持 make jobserver）。

### 9.1 缓存模型与后端

- C/C++ key：预处理源码 + 编译器参数；新版本有 direct/preprocessor 类优化。`SCCACHE_BASEDIRS` 在 hash 前剥离基目录，实现 automation 与本地 worktree 共享条目。
- 支持编译器：gcc、clang、MSVC、**rustc**、NVCC（CUDA）、hipcc、diab 等。
- **存储后端是其最大特色**：本地磁盘、S3、GCS、Azure、R2、Redis、Memcached、GH Actions cache、WebDAV、阿里云 OSS、腾讯云 COS；支持 **multi-level 分层缓存 + 自动 backfill**。

### 9.2 sccache-dist：icecream 风格但补齐安全

自研 HTTP/gRPC 风格协议（**不是 REAPI**）：scheduler（协调/分配）+ build server（注册、在 **overlay FS + bubblewrap 沙箱**中编译）+ client（直连分配 server，上传自动打包的工具链与任务）。安全上具备 **token 认证或 mTLS + TLS 传输 + 沙箱**——正好补齐 icecream 的三个短板。

### 9.3 Mozilla 的工程经验（glandium 系列）

起因：AWS 短生命周期 slave 导致本地 ccache 命中率极低（约 25% 的 Linux build 命中率过 50%），连续 push 共享 99% 代码却很少落在同一台机器。2014 年 S3 共享缓存实验，回放 629 个 Try push：

| 指标 | S3 共享缓存 | 热 slave ccache | AWS 冷 slave ccache |
|---|---|---|---|
| 平均构建（unified） | **17:11** | 29:19？（对照口径见下） | 31:35 |
| 平均构建（non-unified） | **30:58 vs 57:08**（中位 22:27 vs 60:57） | — | — |
| 总体 | >90% 的构建变快，仅 3.25% 变慢 | — | — |

2016 年 sccache 从 Python 重写为 Rust（新增 MSVC 支持）。现状：Firefox/Thunderbird CI 用 **GCS**（项目 `sccache-3`）；近期痛点如 asan 构建缓存命中率低。sccache 与 distcc 一样**不缓存链接**：链接的外部状态无法可靠入 key 且不可切分。

---

## 10. Nix：纯函数式 store 与二进制缓存

Nix（Dolstra 博士论文，2004 年起）把整个系统建模为纯函数：**derivation（构建描述）→ store path（构建输出）**。2026 年当前版本约 **Nix 2.35**。

### 10.1 Store 与 hash 模型

- Store 路径 `/nix/store/<hash>-<name>`。传统模型是 **input-addressed（fixed-output）**：hash 基于 derivation 图（所有输入 path、builder、参数）的哈希加输出名——不是输出内容的哈希。
- **CA derivations（RFC 62，2020 起，实验特性 `ca-derivations`）**：输出改由**实际内容哈希**标识（`__contentAddressed` 属性开启）。核心收益是**切断不必要的重建级联**：派生变化但输出内容不变时，输出 path 不变，下游不重建。与 `impure-derivations`（非固定输出、可联网）互斥；截至 2026 年仍属实验特性（milestone 35）。
- 依赖是完整闭包：每个 path 自带依赖引用扫描，部署/缓存单位是闭包而非单个包。

### 10.2 远程构建：remote builders 与协议

- `builders` 配置（`/etc/nix/machines`），每条 8 个字段：store URI、system types、SSH key、最大并行数、**speed factor（调度按 factor×负载选机）**、支持/要求的 features（如 kvm）、主机公钥。
- Store URI scheme：`ssh://[user@]host`。**历史背景**：Nix 1.x 时 `ssh://` 是旧的 nix-copy-closure 协议，`ssh-ng://` 是 Nix 2.0 的 nix-daemon worker 协议（"next generation"，直接在 SSH 隧道上跑 daemon 二进制序列化协议，增量、高效）；当前 Nix 中 `ssh://` 默认已是 daemon 协议，`ssh-ng://` 保留为别名。
- `max-jobs = 0` 可只构建在远端；`builders-use-substitutes = true` 让远端 builder 用**它自己的** substituter 获取依赖，避免慢速链路下本机上传全部依赖。

### 10.3 Substituters 与 binary cache（值得重点借鉴）

- 从二进制缓存（cache.nixos.org、S3、HTTP）下载预构建 path，按优先级尝试（cache.nixos.org 优先级 40）。
- 传输格式：**`.narinfo`（小元数据：nar 哈希、大小、依赖 path 列表、签名）+ `.nar`（Nix ARchive，规范化的文件树序列化，确定性优于 tar：排序、统一元数据、符号链接语义明确）**，通常 `.nar.zst`/`.xz` 压缩。
- **安全模型**：`require-sigs`（默认 true）要求非内容寻址 path 必须由 `trusted-public-keys`（ed25519）签名才接受；**内容寻址 path 自身即可信，免签名**。这是一条重要设计原则：**密码学内容寻址天然提供完整性，签名只在"承诺身份"（input-addressed name）时才必要**。

### 10.4 沙箱与优缺点

- Linux 上用 namespaces（私有挂载/网络/pid）构建，macOS 用 sandbox-exec；纯函数模型强制声明输入输出。大规模实测（arXiv:2501.15919，70.9 万次构建）bitwise 复现率 69%(2017)→91%(2023)，非确定性首因是嵌入日期。
- 优点：正确性理论最干净；二进制缓存协议（narinfo+nar+签名）简单到可以用任意静态存储托管；多版本共存无依赖冲突。
- 缺点：input-addressed 模型下"等价输出"仍重复存储/重复构建（CA 是解药但未稳定）；图哈希粒度粗（一个参数变化全量重编该派生）；学习曲线陡。

---

## 11. Turborepo / Nx：任务级远程缓存

### 11.1 Turborepo（Vercel，Rust，MIT；remote-cache SDK 为 MPL-2.0）

注：Turborepo 经历 JS → Go → Rust 三次重写（2023-12 起为 Rust）。指纹算法为 **XXH64（Cap'n Proto canonical 序列化后哈希）**，非 SHA-256；SHA-256 仅用于 artifact 的 HMAC 签名。

任务指纹分两级哈希：

- **全局哈希（任何变化使全部任务 miss）**：任务定义（turbo.json 的 outputs 配置）、根 lockfile、根依赖的内部包源码、`globalDependencies` 文件、`globalEnv` 环境变量值、透传 CLI 参数。
- **包/任务哈希**：包级 turbo.json、影响该包的 lockfile 段、包 `package.json`、包目录下版本控制文件（可用 `inputs` 精调）。
- 缓存内容：声明的 `outputs` 文件 + 终端日志；本地缓存 `.turbo/cache`（git worktree 自动共享主 worktree 缓存）。
- **Vercel Remote Cache**：简单 HTTP artifact API（按 team/project/hash 存取，`turbo login/link` 零配置），可自托管（如 [ducktors/turborepo-remote-cache](https://github.com/ducktors/turborepo-remote-cache)）；任务级 `"cache": false` 可关缓存，`--force` 只禁读不禁写。

### 11.2 Nx（TypeScript，性能模块 Rust，MIT；hash 算法 XXH3_64）

- hash 输入：项目文件、依赖项目文件（`^production` named inputs）、workspace 配置、外部依赖版本、runtime 信息（OS/CPU，可配 runtime inputs）、环境变量、CLI 参数；`inputs/namedInputs` 精确控制。
- 条目含 hash、stdout/stderr、outputs 文件；查找顺序 local → remote（Nx Replay/Cloud，可接 S3 等自建）。
- **DTE（Distributed Task Execution）**：不只缓存，还把任务图分片分发到多 agent 执行（main 进程分派 + agent 执行），是任务级系统里少见的"执行"而非仅"缓存"能力。

### 11.3 任务级缓存的权衡

优点：模型极简、不依赖编译器内部知识、接入成本低，且天然能缓存链接/打包/任意脚本。缺点：粒度粗，且 poisoning 风险集中在环境变量与隐式文件（防线：严格 env-mode、显式 inputs、runtime inputs 兜底机器差异）。对我们的启示：**"打包"这种粗粒度、脚本化动作适合任务级缓存，可作为产物级缓存的实现方式。**

---

## 12. BuildKit / Dagger / Earthly：容器构建的 solver 路线

### 12.1 BuildKit（Moby，Go，Apache-2.0）

- 架构：`buildkitd`（Frontend → **Solver** → Cache/Content Store → Worker）+ `buildctl`，gRPC 通信（Unix socket 或 TCP + **mTLS**，官方强调因 RUN 容器也可能访问 API）。
- **LLB（Low-Level Build graph）**：protobuf 定义的二进制中间格式，"LLB is to Dockerfile what LLVM IR is to C"；显式依赖图、可并发执行、缓存键即图节点的内容哈希，厂商中立。
- **内容寻址**：所有层/元数据用 sha256 digest，遵循 OCI Image Spec 布局；与 Docker 老式"按父镜像+指令字符串"的 layer cache 相比，LLB 的 key 包含完整输入闭包，跨 Dockerfile/跨仓库可复用。
- Solver 支持嵌套构建（build 中调用 build）、去重（in-flight 相同顶点合并）。
- **GC**：自动垃圾回收 + 可配策略；`buildctl du/prune`。
- **cache export/import**：registry（min/max 模式）、inline、local、gha、s3、azblob；compression 含 estargz（可懒加载）、zstd。
- **remote builders**：TCP/mTLS 远程 buildkitd、多实例负载均衡（非共享缓存时客户端一致性哈希）、K8s 部署、rootless、daemonless 临时容器模式。
- 可观测：内建 OpenTelemetry。

### 12.2 Dagger

- Dagger Engine 构建于 BuildKit 之上，对外是 **GraphQL System API**（早期 CUE，2023 年起转向代码 SDK）。2026 年 SDK 覆盖 8 种语言：Go、Python、TypeScript、PHP、Java、.NET、Elixir、Rust。
- 编程模型：Functions / Modules，函数在容器化沙箱中编排工具；自定义对象类型内容寻址、可跨 SDK/模块边界传递。
- 缓存：每个操作按输入内容寻址，本地/CI 间自动生效；Dagger Cloud/Cloud Cache 提供远程共享缓存与 trace UI；OTel 可观测。
- 价值主张：把"CI YAML"升级为可本地运行、可测试、可复用的代码。2026 年 v0.21 启动 **Project Theseus**：移除 BuildKit solver，新缓存基于 **e-graphs（等价关系图）**——语义等价但精确 key 不同的操作也能共享，命中率高于精确 key，值得跟踪。

### 12.3 Earthly

- BuildKit 之上的 Earthfile（Makefile + Dockerfile 混合语法）；**Earthly Satellites** 是托管的远程 BuildKit 实例（持久 warm、跨 CI 平台共享缓存）；缓存走 registry 与 Earthly Cloud。
- 定位：比 Dagger 更"CI 原教旨"，比 Docker Build 更可表达。

**solver 路线的共同启发**：把构建定义编译成一张显式的、内容寻址的中间图（LLB），所有执行/缓存/远程分发都由通用 solver 处理——前端语言（Dockerfile/Earthfile/代码 SDK）与后端能力解耦。这和 REAPI "Action  Merkle-DAG" 是同一个思想在不同领域的落地。

---

## 13. Rust 生态现状：rules_rust、sccache 与 REAPI

### 13.1 rules_rust 的远程执行

[rules_rust](https://github.com/bazelbuild/rules_rust)（Apache-2.0）把 Rust 编译建模为标准 Bazel action，因此**天然可以走 REAPI 远程执行**：

- 规则：`rust_library`（输出 rlib + metadata）、`rust_binary`、`rust_proc_macro`、`rust_test`、`rust_shared_library`/`rust_static_library`。
- **Hermetic 工具链**：`rust_register_toolchains()` / crate_universe 按版本从 static.rust-lang.org 下载 pinned rustc/cargo/std sysroot 作为普通 Bazel 仓库，rustc 二进制、sysroot 内 std rlib 全部作为 action inputs 声明。
- Rustc action 的输入：源码（含 `include!()` 需要的文件，必须声明，否则 hermetic 执行直接失败）+ 依赖 rlib（depset 传递）+ rustc + proc macro（作为 host 配置的构建工具 action）；输出 rlib/metadata。
- **链接是 hermeticity 的难点**：rustc 调用 cc 链接器，远程执行需要 hermetic CC 工具链（典型搭配 [uber/hermetic_cc_toolchain](https://github.com/uber/hermetic_cc_toolchain) 的 zig cc），否则远程 worker 的系统 glibc/ld 版本进入隐式输入。
- **process wrapper**：rules_rust 自含小的 `process_wrapper` 二进制（`util/process_wrapper`），用于响应文件、Windows 下的命令包装与输出处理；是工具链 action 的一部分而非远程执行专属机制。
- 路径确定性：配合 `--remap-path-prefix`（rules_rust 默认注入 rustc remap 参数）消除绝对路径与 CWD 对产物的影响。

### 13.2 Cargo 生态：没有官方远程缓存

- Cargo 自身无远程缓存/远程执行能力；依赖解析后的 crate 编译是纯本地行为。
- 现实组合：`RUSTC_WRAPPER=sccache` + `CARGO_INCREMENTAL=0` 是 CI 事实标准（详见 sccache 的 rustc 限制）：
  - **rlib（lib）可缓存**；**bin/dylib/cdylib/proc-macro 因调用系统链接器不可缓存**——官方 workaround 是把大 bin 拆成 lib 加薄 main；
  - 增量编译 crate 不可缓存（incr. 产物无法用单输入 hash 表达）；
  - build script 的文件/环境读取是潜在不纯来源。
- NativeLink 是 Rust 写的 REAPI server，但 Cargo 不是 REAPI client，两者不能直接对接；需要 REAPI 化的构建前端（Bazel/Pants/Buck2/recc）才能利用。
- Pants 对 Rust target 有实验支持（走同一套 Process/REAPI）；Buck2 prelude 内建 Rust 规则（Meta 内部有使用）。
- 截至 2026 年，Rust 基金会/cargo 团队尚无内建远程缓存的官方路线公布，空白正是本项目的存在理由。

### 13.3 对 rustc 可分发性的技术判断

rustc 单 crate 编译非常契合 action 模型：输入可枚举、输出确定、编译器自包含；难点集中在链接（cc sysroot）、proc macro/build script（需沙箱与输出声明）、路径与环境确定性——与 C++ 在 REAPI 下的经验同构，没有新的理论障碍。

---

## 14. 增量构建正确性：动态依赖、false share、cache poisoning

### 14.1 学术与工业脉络

| 系统/论文 | 年代 | 核心贡献 |
|---|---|---|
| Make（Feldman） | 1979 | mtime 依赖模型，现代构建系统的起点 |
| **Vesta / Vesta-2**（DEC SRC） | 1998-2003 | 第一个严肃的函数式 SCM：不可变全局命名空间、**通过系统调用拦截自动发现依赖**、派生文件按摘要缓存——Bazel/Nix 的直接思想先驱 |
| Nix（Dolstra 博士论文） | 2006 | 纯函数式部署模型，store path 与闭包，二进制缓存 |
| **Shake**（Mitchell） | 2012 | monadic 动态依赖（action 内 `need`）；**持久化依赖 trace + 内容哈希**，"既正确又最小" |
| **Build Systems à la Carte** | 2018/2020 | 调度/重建判定/存储的组件化分类与重组理论 |
| **BuildXL**（Microsoft，开源） | 2016 起 | "pip" 图 + **Detours 系统调用拦截**在 Windows 上通用地动态发现输入（不依赖编译器配合），大规模微内核/Office 构建 |

### 14.2 动态依赖发现（dynamic dependency discovery）

问题：编译动作的完整输入在运行前不知道——C/C++ 的 `#include`（含宏分支）、Fortran include、rustc 的 `mod`/`include!()`、ThinLTO 的跨模块引用。业界四种解法：

1. **预处理自包含**：本地跑预处理，把展开结果作为远程输入（distcc、sccache C 模式、Goma）。简单但只适用于类 C 预处理模型，且失去直接复用头文件 blob 的机会。
2. **显式声明 + include 扫描**：规则层声明 deps，构建系统/专用扫描器（goma input processor、clang-scan-deps、reclient scandeps）预扫描（Bazel cc、reclient）。
3. **两阶段 action / monadic 动态依赖**：先执行一个发现动作，读其输出再声明后续依赖（Bazel input discovery、Buck2 dynamic deps、Shake `need`）。表达力最强。
4. **系统调用拦截**：执行时拦截文件 open（Vesta、BuildXL Detours、Goma 服务端），事后得到真实输入集。最通用，不需编译器配合，但首次必须本地/受控执行，且要处理"读了但不影响输出"的噪音。

### 14.3 False cache share（错误共享）

定义：逻辑上不同的输入集合映射到了同一个 cache key，导致命中错误产物。典型场景与防线：

- ccache preprocessor 模式：只 hash 预处理输出，**两个不同源文件可能展开出相同 `.i`**（如路径差异、未使用的条件分支）→ direct mode 的 **manifest 记录真实 header 路径+各自 hash**，用更丰富的输入指纹消除歧义。
- 编译器身份变化（升级 clang）未入 key → common information 强制包含编译器哈希。
- 绝对路径/CWD 差异 → `base_dir`、`-fdebug-prefix-map`、rustc `--remap-path-prefix`。
- REAPI 的解法：**Action 把 Command、input root Merkle digest、platform、timeout、salt 全部编码**，key 空间在协议层尽量完备；canonicalization 规则保证"同输入同字节"。
- 设计原则：**key 漏字段是安全事故，key 多字段只是性能损失**——设计 key 时应当"宁多勿漏"，再用 sloppiness/精确 inputs 优化命中率。

### 14.4 Cache poisoning（缓存投毒）

定义：一个错误或恶意的结果被写入共享缓存，随后被大量构建命中、级联扩散。各系统防线：

| 防线 | 系统 |
|---|---|
| hermetic 沙箱使错误结果难以产生（隐式输入读不到） | Bazel/Buck2/BuildXL |
| 写入权限分级：CI 可写，开发者只读；按 instance_name 隔离信任域 | RBE、BuildBuddy、EngFlow |
| Action `salt`：编译器/基础设施事故后整体逻辑作废缓存 | REAPI |
| **二进制缓存签名**：只有持有私钥者能产出被接受的条目；内容寻址条目自带完整性 | Nix（ed25519 narinfo） |
| 传输层 mTLS + 审计日志 | NativeLink、BuildKit remote |
| 任务级系统：严格 env-mode、显式 inputs，防止环境变量注入污染 | Turborepo/Nx |

**2025 年标志性事件 CREEP（[CVE-2025-36852](https://nx.dev/blog/cve-2025-36852-critical-cache-poisoning-vulnerability-creep)，CVSS 9.4）**：当不可信 PR 与 main 共享同一缓存命名空间时，PR 构建 "first-to-cache-wins" 抢跑写入恶意条目；投毒发生在 hash/加密之前，checksum 必然匹配，**签名/HMAC/immutable/加密全部失效**。正解：branch-scoped 层级缓存 + 写权限按分支分级（PR 只读 main 可写）+ 发版构建跳过缓存。另据 IEEE Software 2025 对 70 个 Bazel 项目、1.5 亿次 syscall 的实测，**不存在完全 hermetic 的项目**，仅 37% 配置 hermetic 工具链——可见 hermeticity 是持续工程而非一次性配置。

业界共识：**非 hermetic 的远程缓存等于定时炸弹**，沙箱和 key 完备性必须先于命中率优化。

---

## 15. 横向总对比表

### 15.1 系统 × 缓存 × 协议 × 许可 × 隔离

| 系统 | 缓存粒度 | cache key | 远程协议 | 开源协议（语言） | 服务端隔离 |
|---|---|---|---|---|---|
| Bazel | action | Action digest（Command+Merkle input root+platform+timeout+salt） | REAPI v2 gRPC | Apache-2.0（Java） | namespaces/sandbox-exec；服务端视后端 |
| BuildBuddy | action + blob（CDC） | REAPI | REAPI v2（2.0–2.11） | MIT 核心（Go） | Docker / **Firecracker microVM** |
| EngFlow | action | REAPI | REAPI v2 | 闭源（服务端） | 容器 |
| bazel-remote | blob（仅 cache） | SHA256 | REAPI 2.0–2.3 + HTTP REST | Apache-2.0（Go） | 不执行 |
| Buildbarn | action + blob | REAPI | REAPI 2.3–2.11 | Apache-2.0（Go） | chroot（可换容器/QEMU） |
| NativeLink | action + blob | REAPI（SHA256/BLAKE3） | REAPI v2 | FSL/BSL（**Rust**） | 容器 + single_use 卷 |
| Buildfarm | action | REAPI | REAPI 2.3–2.11 | Apache-2.0（Java） | 容器 |
| BuildGrid | action | REAPI | REAPI 2.x | Apache-2.0（Python） | BuildBox 沙箱 |
| Buck2 | action + deferred 物化 | hash(command+all inputs) | **REAPI**（原生） | MIT/Apache（Rust） | 依赖后端 |
| Pants v2 | Process（action） | input Merkle digest + argv + env | REAPI | Apache-2.0（Rust 引擎） | 依赖后端 |
| Goma | 编译单元 | 编译器私有指纹 | 私有 HTTP/1（已死） | BSD-3 客户端 | 闭源 |
| reclient | action | REAPI | REAPI | Apache-2.0（Go） | 依赖 RBE |
| siso | action | REAPI | REAPI | BSD-3（Go） | 依赖 RBE |
| distcc | 编译单元 | 无缓存（纯分发） | 私有 TCP | GPL-2.0（C） | 无 |
| ccache | 编译单元 | direct manifest / 预处理输出（BLAKE3） | 无（HTTP storage 插件） | GPL-3.0（C/C++） | 无 |
| icecream | 编译单元 | 无缓存 | 私有协议 | GPL-2.0（C++） | chroot |
| sccache | 编译单元 | 预处理+参数（+basesdirs） | 云对象存储 HTTP；sccache-dist 自研 | Apache-2.0（Rust） | bwrap+overlay（dist） |
| Nix | derivation（闭包） | 传统图哈希；CA=内容哈希 | ssh daemon 协议；HTTP binary cache（narinfo+nar） | LGPL-2.1（C++） | namespaces/sandbox-exec |
| Turborepo | 任务（XXH64） | 全局 hash + 包 hash | HTTP artifact REST | MIT（Rust） | 不执行（仅缓存） |
| Nx | 任务 | 多输入 hash | HTTP（Nx Replay）；DTE 分发 | MIT（TS） | agent 容器（DTE） |
| BuildKit | LLB 顶点 / layer | LLB 内容哈希（sha256） | gRPC（mTLS） | Apache-2.0（Go） | runc/containerd |
| Dagger | 操作/对象 | 内容寻址 | GraphQL（engine 本地/远程） | Apache-2.0（Go） | 容器（BuildKit） |
| Earthly | target/layer | LLB | gRPC + 云 | MPL-2.0（Go） | 容器 |

### 15.2 规模数据一览

| 组织/系统 | 公开规模口径 |
|---|---|
| Google（Blaze/Forge/ObjFS） | 每天数百万 builds、数百万测试、PB 级产物、数十亿行代码；farm 数万台机器；2008 年起 |
| Meta（Buck2） | 每天数百万 builds、数千开发者；较 Buck1 快 2×；deferred materialization 2.5× |
| Databricks（Bazel+EngFlow） | 每天 10 亿次 action（BazelCon 2025） |
| EngFlow | 单部署 1–100k+ cores，5–10× 加速，云成本 -20~50% |
| NativeLink | 单集群每月 10B+ 请求（官网另有 1B 口径）；LLVM 17min→4min；0 GC pause |
| BuildBuddy | 快照 runner：CI 中位 6min→30s；CDC：上传/存储 -40% |
| Mozilla（sccache） | 共享缓存使 90%+ Try 构建变快；unified 17:11 vs 29:19；non-unified 30:58 vs 57:08 |
| Chrome（Goma/siso） | 本地 64 核怪兽机 23min；分布式时代内部数分钟；link 恒本地 |

## 16. 对 Rust 远程打包服务架构的启示

### 16.1 总体判断

1. **不要发明新的执行语义，要发明贴合 Cargo 的前端**。REAPI 已经把"命令 + 输入闭包 → 结果 + 输出闭包"抽象到接近最优，且生态（客户端/服务端/工具链规则）证明了它。差异化价值在于：让 **Cargo 工作流**（而非 Bazel/Buck）无需重写 BUILD 文件就能获得远程缓存/执行，并把 **打包（link + package）** 这一历史盲区纳入服务范围。
2. **正确性先于命中率，先于延迟**。所有系统的事故史都指向同一顺序：先 hermetic 沙箱 + 完备 key，再优化命中。
3. **多层缓存优于单一粒度**。

### 16.2 推荐的缓存层次

针对 Rust 打包服务，建议四层缓存，自上而下：

| 层 | 粒度 | key 内容 | 命中收益 | 借鉴对象 |
|---|---|---|---|---|
| L0 依赖源缓存 | 依赖包（crate 版本） | crate 名+版本+校验和（Cargo.lock 行） | 免下载/免 git fetch；统一工具源 | Remote Asset API、Nix fixed-output |
| L1 crate 编译缓存（rlib/metadata） | 单 crate 的非链接产物 | rustc 版本 + 完整 argv + feature 集 + 全部源文件 Merkle hash + 依赖 rlib digest 集 + sysroot/平台 hash + 路径 remap 配置 | 省编译（最贵） | Bazel AC + rules_rust action；sccache；Nix derivation hash |
| L2 链接/最终 bin 缓存 | 单最终目标（bin/cdylib） | L1 依赖闭包 digest + linker 身份+参数 + cc sysroot digest + 资源文件/打包脚本 hash | 省链接（sccache 的历史盲区，我们用沙箱补齐其"链接输入不可枚举"的理由） | BuildKit LLB 顶点、任务级缓存 |
| L3 打包产物缓存（tar/dmg/deb/镜像层） | 交付包 | L2 产物 digest 闭包 + 打包配方版本 + 签名/版本元数据 | 省打包；跨 CI/发版复用 | Nix nar/narinfo、BuildKit 内容寻址层、Turborepo 任务缓存 |

底层统一用 **CAS（blob 内容寻址，SHA-256，BLAKE3 可选）**支撑全部四层：所有 key 只引用 blob digest，字节天然去重；大产物（链接 bin、镜像）采用 **CDC 分块**复用字节（BuildBuddy 数据 -40%）。

关键设计点：

- **L1 key 必须像 REAPI Action 一样完备**：rustc 版本、`RUSTFLAGS`、feature、cfg、CWD/remap、平台、cc sysroot 全部编码；build script 与 proc macro 必须在沙箱中执行（它们能读文件/联网），其输出声明纳入 action 结果。
- **L2/L3 用"输入闭包 digest + 配方 digest"做内容寻址前置 key**，在协议层补 sccache "不缓存链接"的缺口——sccache 的理由是链接外部状态不可枚举，我们用 hermetic worker（固定 sysroot + 沙箱 + 显式资源输入）把外部状态变成可枚举输入。
- 每层保留 **salt / 版本号**：编译器升级、发现 rustc bug、打包配方变更后逻辑作废对应层，不物理清缓存。
- 支持 **AC 写入信任分级**：CI 签名写入，PR/开发者默认可写但按命名空间隔离；只读消费者可拒绝不可信来源（Nix 签名模型值得照搬：内容寻址条目免签，input-identity 条目需 ed25519 签名）。

### 16.3 调度器设计

建议调度器拆成四个可独立扩展的角色（与 NativeLink 的"角色是配置而非程序"一致）：

1. **Frontend/API 层**：无状态，接 API、鉴权、CAS 探活、把 action 提交给 scheduler。
2. **Scheduler（核心状态）**：
   - AC 短路：先查 action cache，命中直接返回；
   - **in-flight 去重合并**：相同 action digest 的并发请求只执行一次、多 client 订阅同一 Operation（NativeLink/BuildKit 都有，省算力的第一道杠杆）；
   - **platform/约束匹配**：按 OS/arch、sysroot 版本、GPU、macOS、工具链池匹配 worker（REAPI platform properties 模型）；
   - 优先级队列：优先级 hint（交互式 < CI < 批量预热）+ 公平性/配额（按团队），非抢占 best-effort；
   - 数据本地性：把 crate 编译调度到已缓存其依赖 rlib 的 worker（worker 本地盘做 L1 热缓存 + 心跳上报已持有 digest 清单），CAS 拉取是主要开销，数据亲和收益大；
   - 调度状态外置（Redis/DB）支持 HA 与跨副本去重。
3. **Worker 池**：
   - 生命周期分级：**warm 常驻 worker**（预热 rustc/sccache/依赖，跑 L1 小编译，低延迟）与 **强隔离 ephemeral worker**（跑 build script/proc macro/链接/打包，一 action 一换可写卷或 microVM）；
   - 最低隔离：Linux namespaces 五件套 + 全 FS 只读 + 唯一可写 execroot + 默认断网（Remote Asset API 代取依赖）+ PID 收割；强隔离上容器，最高要求上 **Firecracker 快照克隆**（BuildBuddy：6min→30s 的冷启动解法）；
   - worker 跨 action 复用输入用 hardlink（不可写），输出走新卷。
4. **存储层**：CAS 分层（内存 → 本机/SSD → S3/GCS 对象存储）+ AC 元数据库 + 大 blob ByteStream 流式传输与 CDC；本地缓存 LRU + 对象存储生命周期，Buildbarn 的环形免 GC 存储也是可参考实现。

客户端侧直接照搬 Bazel 验证过的容错策略：有限次重试 + 指数退避/jitter、检测到缓存淘汰后整体重试、错误率熔断、远程失败本地 fallback、可选的本地/远程竞速（对 rustc 小编译尤其有价值）。

### 16.4 API 协议建议

**推荐：协议语义与 REAPI v2 对齐（数据结构模型照搬），传输层提供 gRPC；保持与 REAPI 的双向兼容作为远期选项，但不强制客户端说原生 REAPI。**

- 核心模型直接采用 REAPI 五件套：`Digest / Command / Action(Merkle input root) / ActionResult / Directory tree`，外加 CAS / AC / Execute / Capabilities 服务划分与 ByteStream 分块。proto 以 Apache-2.0 从 remote-apis 衍生，遵守其 canonicalization 规则。
- 自研薄前端 API（Cargo 友好）：
  - `PrepareManifest`：给定 Cargo.toml/Cargo.lock 与源树，返回依赖闭包与各 crate 的 action digest（把 cargo metadata 的结果协议化）；
  - `BuildPackage`：批量提交 crate action 图 + 最终 link/package action（一次 RPC 内部完成 AC 查询/调度，避免 Bazel 链式 RPC 的小 action 延迟问题——把 DAG 提交给服务端而非逐 action RPC）；
  - Operation 流复用 REAPI 的 stage 模型（CACHE_CHECK/QUEUED/EXECUTING/COMPLETED）与 WaitExecution 断线恢复；
  - 支持 **materialization 策略**（Buck2 经验）：批量构建默认不物化中间 rlib，只返回最终包；交互式/调试时按需下载；
  - 事件/日志：invocation 级事件流对齐 BEP 思想（事件 DAG + 序号 ack），结果 UI 不自造分析协议。
- 认证：mTLS / token；签名与信任模型参考 Nix（内容寻址免签，身份承诺条目签名）。
- **不建议**：完全私有二进制协议（Goma 的下场）；把 task-based 粗粒度 key（Turborepo 模型）作为 L1/L2 的 key（只适合 L3 打包层）；首版就做跨语言通用执行（先把 Rust 一种做深，协议预留 platform 扩展即可）。

### 16.5 分阶段落地建议

1. **阶段一**：CAS + L1 crate rlib 缓存（只读缓存服务 + Cargo wrapper 客户端），key 按 §16.2 完备计算，worker hermetic 先行。对标 sccache 但 key 更完备、后端更工程化。
2. **阶段二**：远程执行 scheduler + in-flight 去重 + warm worker 池，把编译从客户端搬走；加入竞速与 fallback。
3. **阶段三**：L2 链接缓存（hermetic cc sysroot）+ L3 打包产物缓存，覆盖 sccache 盲区，形成"打包即服务"。
4. **阶段四**：microVM 强隔离池、CDC 大产物分块、BEP 结果 UI、（可选）REAPI 原生兼容接入 Bazel/Buck2/Pants 生态。

## 17. 参考资料

### Bazel / REAPI

- Bazel Distributed Builds（Google 规模 / Forge / ObjFS）：https://bazel.build/basics/distributed-builds
- Bazel Hermeticity：https://bazel.build/basics/hermeticity
- Bazel Sandboxing：https://bazel.build/docs/sandboxing
- Bazel Dynamic Execution：https://bazel.build/remote/dynamic
- Bazel Remote Caching：https://bazel.build/remote/caching
- Bazel Platforms：https://bazel.build/concepts/platforms
- Bazel BEP：https://bazel.build/remote/bep
- remote-apis 仓库：https://github.com/bazelbuild/remote-apis
- v2.12.0 proto：https://raw.githubusercontent.com/bazelbuild/remote-apis/refs/tags/v2.12.0/build/bazel/remote/execution/v2/remote_execution.proto
- ByteStream proto：https://github.com/googleapis/googleapis/blob/master/google/bytestream/bytestream.proto

### REAPI 服务端

- BuildBuddy：https://github.com/buildbuddy-io/buildbuddy ；Fast Runners：https://www.buildbuddy.io/blog/fast-runners-at-scale/ ；CDC：https://www.buildbuddy.io/blog/content-defined-chunking/
- EngFlow：https://www.engflow.com/ ；团队：https://dev.engflow.com/company/team
- bazel-remote：https://github.com/buchgr/bazel-remote
- Buildbarn：https://github.com/buildbarn/bb-remote-execution ；https://github.com/buildbarn/bb-storage ；bonanza：http://bonanza.build/
- NativeLink：https://github.com/TraceMachina/nativelink ；架构：https://docs.nativelink.com/explanations/architecture/ ；产品：https://nativelink.com/product
- Buildfarm：https://github.com/buildfarm/buildfarm ；BuildGrid：https://buildgrid.build/
- Scoot：https://github.com/twitter/scoot
- Flare 收购公告：https://www.businesswire.com/news/home/20221013005279/en/Bitrise-Acquires-Flare.Build

### Buck2 / Pants / reclient / siso

- Buck2 架构：https://buck2.build/docs/concepts/architecture/ ；Modern DICE：https://buck2.build/docs/insights_and_knowledge/modern_dice/
- Buck2 远程执行：https://buck2.build/docs/users/remote_execution/ ；Deferred Materialization：https://buck2.build/docs/users/advanced/deferred_materialization/
- Buck2 开源公告：https://engineering.fb.com/2023/04/06/open-source/buck2-open-source-large-scale-build-system/
- Pants 工作原理：https://www.pantsbuild.org/docs
- Pants Process API：https://v2.pantsbuild.org/stable/docs/writing-plugins/the-rules-api/processes
- reclient：https://github.com/bazelbuild/reclient
- Goma 停服分析（EngFlow）：https://blog.engflow.com/2023/09/11/goma-is-gone-put-everything-into-reclient/
- siso：https://pkg.go.dev/go.chromium.org/build/siso

### C/C++ 经典 / sccache

- distcc 手册：https://www.distcc.cc/ （安全：https://www.distcc.cc/security.html）
- ccache 手册：https://ccache.dev/manual/latest.html
- icecream：https://github.com/icecc/icecream
- sccache：https://github.com/mozilla/sccache ；Distributed：https://github.com/mozilla/sccache/blob/main/docs/DistributedQuickstart.md
- Mozilla 共享缓存实验：https://glandium.org/blog/?p=3054
- sccache Rust 重写：https://blog.mozilla.org/ted/2016/11/21/sccache-mozillas-distributed-compiler-cache-now-written-in-rust/

### Nix / Monorepo / 容器构建

- Nix 手册：https://nix.dev/manual/nix/stable/
- Nix 实验特性（ca-derivations）：https://nix.dev/manual/nix/stable/contributing/experimental-features
- Turborepo Caching：https://turborepo.dev/repo/docs/core-concepts/caching
- Turborepo Remote Cache 自建：https://github.com/ducktors/turborepo-remote-cache
- Nx Caching：https://nx.dev/concepts/how-caching-works
- BuildKit：https://github.com/moby/buildkit ；Remote builders：https://github.com/moby/buildkit/blob/master/docs/remote.md
- Dagger：https://docs.dagger.io/
- Earthly：https://earthly.dev/

### Rust / 学术

- rules_rust：https://github.com/bazelbuild/rules_rust
- Hermetic CC（zig cc）：https://github.com/uber/hermetic_cc_toolchain
- BuildXL：https://github.com/microsoft/BuildXL
- Build Systems à la Carte：https://www.microsoft.com/en-us/research/publication/build-systems-la-carte/
- Shake：https://ndmitchell.com/#shake
- Nix thesis（Dolstra 2006）：https://edolstra.github.io/pubs/phd-thesis.pdf
- Potvin & Levenberg, Why Google Stores Billions of Lines of Code in a Monorepo, CACM 2016：https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-monorepo/
