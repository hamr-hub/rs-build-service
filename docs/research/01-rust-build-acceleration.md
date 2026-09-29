# Rust 编译流水线与编译加速技术调研报告

> 调研时间：2026 年 9 月（Rust stable 1.98）。目标：支撑「Rust 远程打包服务」设计——最小化构建时间与延迟，支持增量打包。性能数字均来自公开来源，落地前需以自身工程基准复测。



## 1. Cargo 构建机制

### 1.1 整体流水线

Cargo 一次构建的内部流程（`cargo_compile` 模块）：

```
resolve 依赖解析
  → 生成 root Units
  → 遍历依赖图构造 UnitGraph（unit_dependencies）
  → 构建 BuildContext
  → BuildRunner / JobQueue：每个 Unit 入队前先查 fingerprint
  → 调度 rustc 进程，解析其 JSON 消息（artifact / warning）
```

来源：<https://doc.rust-lang.org/nightly/nightly-rustc/cargo/ops/cargo_compile/>

### 1.2 Unit graph：缓存复用的最小单位

- **Unit = 一次 rustc 调用**。同一个 crate 可以产生多个 Unit：不同 profile（dev/release/自定义）、不同 target（host vs 交叉目标）、不同 mode（build script 编译 / build script 运行 / 普通编译 / test / bench）、**不同 feature 集合**。
- Unit 身份哈希（`unit_id` / `c_metadata`）决定产物在磁盘上的路径；`cargo metadata` 无法表达跨依赖类型的 feature，因此 Cargo 提供 `--unit-graph`（不稳定，tracking issue #8002）直接导出 Unit 图：

```bash
cargo +nightly build --unit-graph -Z unstable-options
```

- 对远程服务而言，**Unit graph 是天然的远程缓存 / 远程执行的任务切分边界**：每个 Unit 的输入封闭、产出确定。

来源：<https://doc.rust-lang.org/cargo/reference/unstable.html>（unit-graph 节）

### 1.3 Fingerprint 机制：Cargo 如何判断 Fresh / Dirty

`cargo::compiler::fingerprint` 模块负责变更检测，存放在
`target/{profile}/.fingerprint/<unit>/`（一个 16 位十六进制哈希 + `.json` 明细）。

三层判定：

1. **Fingerprint 哈希**：缺失或任一字段变化即 Dirty；**内嵌所有依赖的 fingerprint，脏状态沿图向上传播**。
2. **mtime 跟踪**：
   - 本 Unit 输出 mtime 对比依赖输出 mtime（`check_filesystem`）；
   - 源文件 mtime 对比 dep-info 锚点（`find_stale_file`）。
3. **`-Z checksum-freshness`**（nightly，#14136）：用「文件大小 + 校验和」替代 mtime，适合 mtime 分辨率弱的文件系统 / CI 场景。

排查「为什么被重新编译」：

```bash
CARGO_LOG=cargo::compiler::fingerprint=trace cargo build
```

**Fingerprint vs unit_id 关键内容对比**（来自 fingerprint 源码注释表）：

| 输入 | Fingerprint | unit_id | c_metadata |
|---|---|---|---|
| rustc 版本 | 是 | 是 | 是 |
| Profile 设置 | 是 | 是 | 是 |
| CompileMode / target 名与 kind | 是 | 是 | 是 |
| **已启用 features** | 是 | 是 | 是 |
| **已声明 features** | 是 | 否 | 否 |
| 直接依赖的哈希 | 是 | 是 | 是 |
| CompileKind（host/target） | 是 | 是 | 是 |
| RUSTFLAGS / LTO 标志 | 是 | 部分 | 否 |
| config 设置、`[lints]`、edition、源码路径 | 是 | 否 | 否 |
| package_id、is_std | 否 | 是 | 是 |

设计要点：`unit_id` 保证不同 Unit 在磁盘上路径分离；fingerprint 承载「应触发重编译但文件名不变」的额外输入。

来源：
- <https://doc.rust-lang.org/nightly/nightly-rustc/cargo/compiler/fingerprint/index.html>
- <https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/compiler/fingerprint/mod.rs.html>

### 1.4 Feature unification 与 resolver

| Resolver | 行为 | 对缓存的影响 |
|---|---|---|
| v1 | 同一 package 的 feature 全局统一，不区分请求来源 | Unit 最少、缓存最易复用；但会破坏 no_std / host-target 隔离 |
| v2（edition 2021 默认） | **不跨**以下边界统一：未激活的 target-specific 依赖；build-deps/proc-macro 与普通依赖；dev-deps（除非构建需要它们的目标） | 同一依赖被编译多次，每个变体是独立 Unit、独立 fingerprint |
| v3（edition 2024，见 §12） | 仅把 `incompatible-rust-versions` 默认改为 `fallback`（MSRV-aware） | 选入 Cargo.lock 的版本可能更旧，间接改变 Unit 集合 |

实践：`cargo tree --duplicates`、`cargo tree -e features` 可找出重复编译；对齐各调用方的 feature 集合即可把多个 Unit 折叠回一个。cargo-hakari（§9.3）是 workspace 级的系统化解法。

来源：<https://doc.rust-lang.org/cargo/reference/features.html>、<https://doc.rust-lang.org/cargo/reference/resolver.html>

### 1.5 Pipelined compilation（流水线编译）

问题：深依赖链上，仅靠「crate 级并行」会让构建尾部串行化。

原理——rustc 编译一个库分两段：

- **前段 / metadata**：解析、展开、类型检查、borrow check、编码 crate metadata；
- **后段 / codegen**：MIR → LLVM IR → 目标码 → `.rlib`。

下游 crate 在最终链接前只需要依赖的「接口」，不需要机器码。因此 Cargo 让 rustc 同时产出两者：

```
--emit=metadata,link --error-format=json --json=artifacts
```

`.rmeta` 在 codegen 开始前落盘，rustc 通过 stderr 的 JSON `artifact` 消息通知 Cargo，Cargo 随即解锁等待中的下游 Unit：

```
crate A:  [ metadata ][==== codegen ====]
crate B:           [ metadata ][ codegen ]   ← A 的 codegen 与 B 的前段重叠
```

边界与限制：

- 最终二进制链接仍需所有依赖完整 `.rlib`，pipeline 只对库生效；
- **proc-macro 不能流水线**：消费方生成 metadata 时必须实际执行 proc-macro，故它必须已是完整原生码。`syn`/proc-macro 链造成 `cargo build --timings` 上典型的「CPU 空洞」；
- `.rlink` 是另一套实验机制（`-Zno-link` / `-Zlink-only` 的延迟链接状态文件），不是常规流水线所用，但被视为未来分布式构建 / 更强产物缓存的基础。

观测：`cargo build --timings` 生成 HTML，codegen 段淡紫色显示，可直接看到重叠程度与等待原因。

来源：<https://www.rust-lang.org/blog/2023/11/09/parallel-rustc/>、<https://doc.rust-lang.org/cargo/reference/timings.html>、<https://doc.rust-lang.org/rustc/json.html>、<https://matklad.github.io/2021/09/04/fast-rust-builds.html>

### 1.6 Build script（build.rs）缓存

- **默认（保守策略）**：build script 不输出任何 `rerun-if` 指令时，**package 目录下任何文件变化**（受 `include`/`exclude` 控制）都会重跑脚本；检测只用 mtime。
- 输出至少一条 `cargo:rerun-if-changed=PATH` 后只跟踪指定路径（PATH 为目录则递归扫描）；`cargo:rerun-if-env-changed=NAME` 跟踪环境变量（不能用于 `TARGET` 等 Cargo 自带变量）。
- 官方建议每个脚本至少输出一条 rerun-if；无需重跑的脚本写 `cargo:rerun-if-changed=build.rs` 即可关闭默认全目录扫描。

对远程服务的含义：build script 是缓存正确性的高风险点（见 §8、§13），其输出（OUT_DIR 生成物、`cargo:rustc-env`、链接配置）必须整体捕获；默认全目录扫描会放大「文件一动、下游全挂」的失效面。

来源：<https://doc.rust-lang.org/cargo/reference/build-scripts.html>（Change Detection 节）

### 1.7 Cargo 缓存何时失效（汇总）

| 触发因素 | 是否失效 | 备注 |
|---|---|---|
| 修改源文件（mtime 变化且晚于 dep-info 锚点） | 是 | 仅受影响 Unit 及其反向依赖链 |
| 修改 `Cargo.toml` / `Cargo.lock`（依赖、版本、feature） | 是 | 近期修复过「workspace 根 Cargo.toml 修改未失效」的 bug |
| 切换 feature 集合 | 是 | 通常连 unit_id（磁盘路径）都变 |
| 升级 rustc | 是 | rustc 版本同时进 unit_id 与 fingerprint |
| 改变 RUSTFLAGS / profile 设置 | 是 | 历史上有过 RUSTFLAGS 改动误伤缓存的修复 |
| 修改 `.cargo/config.toml`（部分键） | 是 | config 进 fingerprint |
| build script 重跑且输出变化 | 是 | 输出不变时下游可保持 Fresh |
| 仅 `touch` 源文件（mtime 变、内容不变） | **可能重编译** | mtime 模型的已知粗糙点；CI 解包保持 mtime 很关键 |
| 切换 profile / target triple | 目录隔离 | 互不共享，也不算「失效」 |
| 绝对路径变化（搬家工作区） | registry 依赖通常可复用 | path crate 与部分编码进产物的路径会触发问题（见 §8） |

来源：<https://doc.rust-lang.org/cargo/reference/changelog.html>

## 2. rustc 增量编译

### 2.1 原理：query system + 依赖图 + on-disk cache

- rustc 内部一切计算皆 **query**（`rustc_query_system`）。源码被切分，**dependency graph** 记录输入如何影响每个 query 结果与最终产物。
- query 结果用 **128-bit fingerprint** 校验。重算 query 时有「fingerprint 稳定性检查」，与上一次会话的磁盘缓存不符即触发著名的 `found unstable fingerprints` ICE。
- `rustc_incremental` crate 负责依赖图的序列化 / 重载：`load_query_result_cache`、work-product 索引（复用 codegen-unit 产物）、HIR 节点哈希。

来源：<https://doc.rust-lang.org/nightly/nightly-rustc/rustc_incremental/>

### 2.2 On-disk cache 的存储模型（copy-on-write）

- 缓存版本一旦 finalize **永不修改**；新会话创建私有 `s-{ts}-{rand}-working` 目录，硬链接 / 拷贝最新缓存，只改副本；
- 编译成功后 rename 为 `s-{ts}-{SVH}`「发布」；失败则删除——支持多 rustc 进程并发编译同一 crate；
- GC 清理崩溃残留及除最新外的旧会话，用 lock 文件保护在用缓存。

来源：<https://doc.rust-lang.org/nightly/nightly-rustc/rustc_incremental/persist/fs/>

### 2.3 用法与默认值

```bash
# dev/test profile 默认开启（Cargo）
CARGO_INCREMENTAL=1
cargo build                                    # dev profile：开
cargo build --release                          # release：默认关
```

### 2.4 为什么 release 默认关闭

- **抑制优化**：增量构建默认 codegen-units = **256**（非增量为 16），为重编译速度牺牲跨单元优化，产物可能明显更慢；
- 历史可靠性事件：2021 年 on-disk cache 缺陷（误编译风险）导致 **Rust 1.52.1 在 stable 上强制关闭增量**，仅能用 `RUSTC_FORCE_INCREMENTAL=1` 覆盖；
- on-disk cache 的 query 复用是「同 crate 小改动」场景优化，远程打包的 release 产物走 sccache（§3）收益更大且不牺牲优化。

### 2.5 局限

粒度为 crate 内复用，跨 crate 无帮助，依赖大变时 dep graph 大面积失效；与 sccache 互斥；incremental 目录膨胀快；历史上 unstable fingerprints / 误编译 bug 多次出现（1.52.1 事件根源）；缓存绑定工作区路径与 crate SVH，不为跨机共享设计。

来源：<https://blog.rust-lang.org/2021/05/10/Rust-1.52.1.html>、<https://doc.rust-lang.org/rustc/codegen-options/index.html>


## 3. sccache：共享编译缓存与分布式编译

Mozilla 开发的编译器包装器（类 ccache），支持 C/C++、**Rust**、CUDA、HIP 等；客户端-服务器模型（本地 server 默认 `127.0.0.1:4226`）。

### 3.1 对 Rust 的支持原理与接入

```bash
export RUSTC_WRAPPER=sccache        # 或 .cargo/config.toml: build.rustc-wrapper = "sccache"
export CARGO_INCREMENTAL=0          # 必须
cargo build --release
sccache --show-stats                # 命中率统计
```

sccache 作为 rustc 前置包装：解析参数 → 计算缓存键 → 命中则直接反序列化产物（不启动 rustc）→ 未命中则运行 rustc 并把结果存入缓存。

对 rustc 命令行的硬性要求：必须有 `--emit`（仅支持含 `link` 的 `link`/`metadata`/`dep-info` 组合）、`--crate-name`、`--out-dir`、显式源文件路径（不支持 stdin）。

### 3.2 缓存键包含什么

- rustc 可执行文件版本 / 其输入；
- 完整 rustc 参数（crate 名、emit、profile、codegen 选项、extern 路径等）；
- 源文件内容；
- 通过 `env!` 读取的环境变量（**Rust ≥ 1.46** 才能被追踪，旧版本会漏检）；
- 覆盖文件的内容（`-Z unstable --extern` 等引用到的文件）。

### 3.3 能缓存什么 / 不能缓存什么

| 类别 | 能否缓存 |
|---|---|
| `rlib`（普通库，release/dev 均可，需关闭增量） | 可以，按 codegen units 级缓存 |
| `bin` / `dylib` / `cdylib` / `proc-macro` | **不能**（需调用系统链接器） |
| 开启 incremental 的编译 | 不能 |
| 展开时读取文件系统的 proc-macro | 可能缓存不正确（副作用无法追踪） |
| build script 的执行 | 不在缓存覆盖范围（build script 编译本身视 crate 类型而定） |

工程对策：把大 bin crate 拆成「lib + 薄 bin wrapper」，让主体编译进入缓存；bin 链接仍要本地做。

来源：<https://github.com/mozilla/sccache/blob/main/docs/Rust.md>

### 3.4 Storage 后端

| 后端 | 适用场景 |
|---|---|
| local disk | 单机 / 单作业，默认 |
| S3（及兼容 API，如 MinIO）、Cloudflare R2 | 团队 / 全局共享，事实标准 |
| GCS、Azure Blob | 对应云生态 |
| Redis、Memcached | 低延迟团队共享层 |
| WebDAV | 与 ccache/Bazel/Gradle 生态互通 |
| GitHub Actions Cache | CI 原生 |
| Alibaba OSS、Tencent COS | 国内云 |

通过编译 feature 选择：

```bash
cargo build --release --no-default-features \
  --features=s3,redis,gcs,memcached,azure,gha,webdav,oss,cos
```

**Multi-Level Cache**（较新特性）：链式分层 + 慢层命中后异步回填快层 + 写穿透：

```bash
export SCCACHE_MULTILEVEL_CHAIN="disk,redis,s3"
# L0 本地盘（~5–10ms，建议 5–10GB）→ L1 Redis（团队共享）→ L2 S3（全局冷数据，~50–200ms）
```

来源：<https://github.com/mozilla/sccache/blob/main/docs/MultiLevel.md>、<https://crates.io/crates/sccache>

### 3.5 dist 模式（分布式编译）

icecream 风格：scheduler + build server（需 `dist-server` feature），自动打包本地工具链，支持所有受支持语言**包括 Rust**；相比 icecream 增加身份认证、TLS、编译服务器沙箱。适合「缓存未命中也要把计算摊到多机」的场景。

快速开始：<https://github.com/mozilla/sccache/blob/master/docs/DistributedQuickstart.md>
Firefox 实测文档：全量构建期望 **2–3× 提升**（<https://firefox-source-docs.mozilla.org/build/buildsystem/sccache-dist.html>）。

### 3.6 实际加速效果（带数据）

| 来源 / 场景 | 数字 |
|---|---|
| Mozilla 官方：热缓存（preprocessor mode，0.7.4+ 默认） | 构建时间 **缩短 2–3×**（<https://firefox-source-docs.mozilla.org/setup/configuring_build_options.html>） |
| CI 典型命中率 | 依赖树少变时 **80%+ 命中常见**；50–60% 命中即可把构建时间砍半（<https://rustutils.com/tools/sccache/>） |
| Qiita 实测某 Rust 项目 | debug 2m01s → 36s（~3.3×）；release 4m59s → 1m10s（~4.3×）；冷构建仅慢 5–15% |
| Xuanwo 在 maturin/PyO3 CI 对比 rust-cache | sccache 在部分 Windows/macOS job 快 **45–50%**，Linux 上互有胜负（<https://xuanwo.io/en-us/reports/2023-04/>） |

### 3.7 局限与坑

不覆盖最终链接（bin/dylib/cdylib/proc-macro），「全命中」仍有链接耗时，需配快链接器（§5）；与增量编译互斥；路径敏感——跨机不同绝对路径击穿命中率，需 `SCCACHE_BASEDIRS` 路径归一（Firefox CI 已默认）；读文件系统 / 依赖时间戳的 proc-macro、build script 有正确性风险；远程服务里应每构建会话起独立 sccache 实例并显式配置远端，避免互相污染。


## 4. cargo-chef：Docker 层缓存

### 4.1 原理

cargo-chef 自身不是缓存服务，而是重组 Docker 构建，让 **Docker 原生 layer cache** 把「依赖编译」与「业务源码」分离：

| 命令 | 作用 |
|---|---|
| `cargo chef prepare --recipe-path recipe.json` | 分析项目，生成最小骨架描述：所有 Cargo.toml（含相对路径）、Cargo.lock、所有 lib/bin target 声明（含默认路径的显式声明） |
| `cargo chef cook --release --recipe-path recipe.json` | 从 recipe.json 还原骨架并**只编译依赖**，不需要真实源码 |

### 4.2 标准三阶段 Dockerfile

```dockerfile
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json   # 依赖层：仅 recipe 变才失效
COPY . .
RUN cargo build --release --bin app

FROM debian:trixie-slim AS runtime
COPY --from=builder /app/target/release/app /usr/local/bin
ENTRYPOINT ["/usr/local/bin/app"]
```

缓存失效边界：

| 层 | 输入 | 失效条件 |
|---|---|---|
| prepare | 全部清单与 target 结构 | 清单 / workspace 成员 / target 变化 |
| cook | recipe.json + 工具链 + 环境 | 依赖、工具链、RUSTFLAGS、target triple 变化 |
| build | 真实源码 | 任意源码变化（依赖产物仍在） |

### 4.3 效果与局限

效果：依赖不变时源码改动跳过整个依赖编译；与 BuildKit `cache-from/cache-to`、sccache 叠加最佳（Depot 范例：<https://depot.dev/docs/container-builds/optimal-dockerfiles/rust-dockerfile>）。局限：为容器构建设计，不建议本地直接使用；planner 与 builder 必须同基础镜像 / 同工具链；cook 层内仍是普通 cargo，bin/proc-macro 链接照样发生，可用 sccache + 快链接器压缩；workspace 新增成员、改 target 声明都会让 recipe 变化。

来源：<https://lib.rs/crates/cargo-chef>


## 5. Linker 加速

### 5.1 为什么链接器重要

链接耗时在 Rust 构建中占比可观：中小项目 release 构建上有实测称 **30–50% 时间花在链接**；增量构建中因为重编译代码少，链接占比更高，成为主要延迟。

### 5.2 各链接器对比

| 链接器 | 平台 | 授权 | 现状 / 加速数据 |
|---|---|---|---|
| GNU ld (bfd) | Linux | 免费 | 默认基线，最慢 |
| **mold** | Linux（主流 x86_64/aarch64） | 免费开源（MIT） | 大规模并行，作者 Rui Ueyama；某 50k LOC 项目 release：ld 4m32s → mold 3m02s（总快 ~33%，比 lld 再快 ~8%）（<https://docs.bswen.com/blog/2026-02-20-rust-compilation-slow-fix-it/>） |
| **sold** | macOS / iOS | **商业付费** | mold 作者的 Apple 平台版本，配置方式相同，指向 sold 二进制 |
| lld（LLVM） | Linux / FreeBSD / macOS（Mach-O port）/ Windows（coff） | 免费 | 上述实测 release 3m18s（比 ld 快 ~27%）；Mach-O 端口实测比 ld64 快 **3–4×**，Chrome Mac 自 Chrome 95 起、Meta iOS 开发构建在用（<https://lld.llvm.org/MachO/index.html>） |
| rust-lld | 随 rustc 分发 | 免费 | lld 的副本，wasm target 默认使用；裸机 / 交叉场景免额外安装 |
| Apple ld64 | macOS | Xcode 自带 | 旧版；**Xcode 15 起新链接器 ld-prime（`-ld_new`）大幅提速**，是 zld 退场主因；Xcode 27 beta 已移除 ld64 实现 |
| zld | macOS | 免费 | michaeleisel/zld，ld64 加速 fork，**仓库已 archived（2024 年确认）**，不要再推荐（<https://github.com/michaeleisel/zld>） |

### 5.3 用法

```toml
# .cargo/config.toml —— Linux + mold
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=/usr/local/bin/mold"]

# macOS + lld
[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

```bash
mold -run cargo build          # 临时用 mold 而不改配置
```

### 5.4 实测综合

- mold + cranelift 组合的某项目：干净构建快 ~25%，**增量构建快 ~75.6%**（4.65s vs 19s 量级）——增量场景链接主导，收益最大（<https://benw.is/posts/how-to-improved-my-rust-compile-times-by-seventy-five-percent>）。

### 5.5 局限

mold 不支持 Windows；macOS 上 mold 本体不可用，对等能力是付费 sold；lld Mach-O 端口个别原生特性兼容性需验证；链接器只压缩链接阶段，对纯计算型全量构建收益有上限；fat LTO 会把大量优化挪到「链接阶段」，此时换链接器的收益模型完全不同（§6）。


## 6. Codegen 选项

来源基线：<https://doc.rust-lang.org/cargo/reference/profiles.html>、<https://doc.rust-lang.org/rustc/codegen-options/index.html>、Nethercote《Rust Performance Book — Build Configuration》<https://nnethercote.github.io/perf-book/build-configuration.html>。

### 6.1 选项逐一说明

| 选项 | 取值 | 对编译时间 | 对产物（速度/体积） |
|---|---|---|---|
| `codegen-units` | N（非增量默认 16；增量默认 256；=1 最慢编译） | N 越大并行越高、编译越快 | N 越大跨单元优化越少，运行可能更慢、体积更大 |
| `lto` | `false`（默认，仅本 crate 内 thin local LTO）/ `"thin"` / `"fat"`(=true) / `"off"` | off 最快链接；fat 链接最慢、内存最高 | fat 全程序优化，运行最快、体积最小；thin 接近 fat 的收益、时间省很多（rustc 自身级别项目 thin 甚至可超 fat）；注意 `lto="off"` ≠ `lto=false` |
| `opt-level` | 0/1/2/3、`s`、`z` | 0 最快，3 最慢 | 3 运行最快；s/z 优化体积，z 有时反而比 s 大（内联/向量化少） |
| `debuginfo` | 0/1/2 | 越高编译越慢、产物体积越大 | 远程发布包通常 0 或仅在需要回溯时保留 |
| `panic` | `"unwind"`（默认）/ `"abort"` | abort **编译还略快一点** | abort 体积略小、运行略快；代价是无 catch_unwind；test/bench/build.rs/proc-macro 强制不受其影响 |
| `incremental` | true/false | 小改时显著加速 | release 下牺牲优化（§2） |
| `debug` 压缩 | `-C compress-debug-sections=zlib/zstd`（nightly 另有 `-Z debuginfo-compression`） | 压缩增加少量 codegen CPU | 省磁盘 / 链接 I/O；消费工具需支持，zstd 工具链支持较新 |

### 6.2 split-debuginfo：容易被忽视的构建时间大项

| 取值 | 行为 |
|---|---|
| `off` | 调试信息留在最终产物（ELF/windows-gnu 默认） |
| `packed` | 打包为单一文件：MSVC `.pdb`、macOS `.dSYM`、ELF `.dwp` |
| `unpacked` | 每个 codegen unit 一个 `.dwo`/`.o`，**跳过昂贵的调试信息链接 / 打包步骤** |

macOS 实测（Cargo 团队，internals #14016）：增量构建中约 **70% 时间耗在 dsymutil**；`unpacked` 让全量构建 164s → 87s（~2×），增量 69s → 11s（>6×），且 target 目录更小。

坑：`packed`（.dwp）下依赖地址的调试信息仍留在 ELF，最终二进制可能意外变大（users.rust-lang.org #131473）。

### 6.3 面向「最快打包」的 profile 取舍建议

```toml
# 服务端「快速发布包」候选基线（产物仍可接受为优化构建）
[profile.release]
opt-level = 2
codegen-units = 16        # 保持默认即可；追求更快编译可调高，=1 最追求运行性能
lto = "off"               # 或 "thin" 换取运行性能；fat 仅在对运行性能敏感时
panic = "abort"           # 确认业务不依赖 unwind
split-debuginfo = "unpacked"
incremental = false
```

原则：远程打包服务应把「构建时间」与「产物运行性能」作为**显式可选档位**暴露，而不是写死——不同业务愿意付的权衡不同。


## 7. Cranelift codegen backend

`rustc_codegen_cranelift`：用 Cranelift（专为 JIT/快速 codegen 设计）替代 LLVM 后端。

### 7.1 现状（截至 2026 年初）

- 以 nightly rustup 组件分发（Linux x86_64/aarch64、macOS、Windows x86_64）：

```bash
rustup component add rustc-codegen-cranelift-preview --toolchain nightly
CARGO_PROFILE_DEV_CODEGEN_BACKEND=cranelift cargo +nightly build -Zcodegen-backend
```

- 加速数据：用户实测中型后端项目冷 debug 构建 ~60s → ~19s（约 3×，配合 `-Zthreads=0`、`-Zshare-generics=y`、快链接器）；Cranelift 自举旧测：29.6s vs LLVM 37.5s（CPU 时间省 ~40%）；
- 运行时产物比 LLVM 慢（部分基准约 2×），**只适合 dev/debug，不适合发布**；
- 功能缺口：`std::arch` SIMD intrinsics 仅部分支持（主要缺口）；panic-unwind 2025 年基于 Cranelift 新异常处理实现已可用，但仍实验性、Windows/macOS 不可用，默认 `panic=abort`。

### 7.2 关键转折：生产化目标已终止（2025-12）

Rust 项目目标「Production-ready cranelift backend」（#397）2025 年 12 月**因未获资助 + 实测发现真实增量编辑场景里 codegen 占比比预期小、端到端收益有限而关闭**， stabilization 无时间表。

对远程服务的含义：debug/检查类（IDE、CI check）构建可作为可选加速档，但不应押注其成为 stable 发布路径；发布构建继续依赖 LLVM。

来源：
- <https://github.com/rust-lang/rustc_codegen_cranelift>
- <https://blog.rust-lang.org/2026/01/05/project-goals-2025-december-update/>


## 8. target 目录复用与陷阱

### 8.1 共享 target 目录的两类问题

**（1）正确性问题——worktree 间静默交叉污染（最严重）**

2026 年 6 月披露的案例：多个 git worktree 共享 `CARGO_TARGET_DIR`，在 worktree B 构建可能**静默编译 / 运行 worktree A 的代码，无任何告警**。根因：

- Cargo 对 workspace/path crate 的元数据哈希**不含 worktree 路径**；同名同版本的两个 worktree 落入同一 fingerprint 槽位；
- Fresh 判定只比源码 mtime 与上次输出，构建较早的 worktree 被误判 Fresh，复用另一个 worktree 的产物；
- registry 依赖不受影响（内容相同复用是正确的）；只有 path/workspace 本地 crate（含测试二进制）碰撞。

**（2）操作性问题**

- 多构建进程写同一 target 目录的锁竞争；
- `cargo clean` 误伤全部项目；
- profile / RUSTFLAGS / features / toolchain 任一不同导致指纹碰撞区域内反复重建；
- 产物无限堆积（deps 目录只增不减，官方 Build Dir Layout v2 正是为解决此问题，见 §12.1）。

### 8.2 推荐架构：两层分离

1. **每个项目 / 每个 worktree 独立 target 目录**（构建工作树，不是缓存）；
2. **sccache 作为跨项目内容寻址缓存**（`cargo clean` 不清它，可设容量 / TTL），但需 `CARGO_INCREMENTAL=0`，且清楚它不缓存 bin/proc-macro 链接。

### 8.3 fingerprint 误判的常见来源

| 现象 | 原因 | 对策 |
|---|---|---|
| CI 里全部重编译 | 解压源码丢失 / 压平 mtime（晚于缓存锚点） | 用保留 mtime 的解包；或切到 `-Z checksum-freshness` |
| 同一项目偶尔莫名全量 | 时钟回拨 / NTP 跳变、文件系统 mtime 分辨率粗（HFS+ 为 1s） | 构建机时钟同步；现代 APFS/ext4；必要时 checksum 模式 |
| 切换容器用户 / 路径后全重编 | 路径相关指纹、权限差异 | 固定构建路径（如 `/workspace`）、固定 UID |
| sccache 命中但 cargo 仍重跑 rustc | RUSTC_WRAPPER 只作用于 rustc 调用，cargo 自己的调度判断在 fingerprint | 缓存策略必须同时照顾 cargo fingerprint 与 sccache 两层 |

来源：
- <https://gitlab.com/lx-industries/openblob/-/work_items/522>
- <https://users.rust-lang.org/t/rust-crates-mut-be-pre-compiled/140741>
- <https://corrode.dev/blog/tips-for-faster-rust-compile-times/>


## 9. 其他加速工具

### 9.1 cargo-nextest（测试执行）

- 自研测试运行器：每个测试独立进程、分批调度、强并行与重试、结构化输出；**不加速编译本身**，但显著压缩测试阶段墙钟时间与 flaky 成本；
- 与本仓库 §8 模型兼容：编译产物仍由 cargo 产出，nextest 只负责执行；CI 中与 Swatinem/rust-cache 是官方推荐组合。
- 来源：<https://nexte.st/>

### 9.2 Swatinem/rust-cache（GitHub Actions）

缓存 `~/.cargo`（registry、git 依赖、bin）与 `target/`。缓存键自动包含：job_id、rustc 版本/host/hash、所有 Cargo.toml/Cargo.lock hash、rust-toolchain 文件、`.cargo/config.toml` hash、编译器相关环境变量（前缀匹配 `CARGO CC CFLAGS CXX CMAKE RUST`）。

要点：

- 默认**只缓存依赖 crate，不缓存 workspace 自身 crate**（性价比考虑），自动设 `CARGO_INCREMENTAL=0`；
- 关键选项：`workspaces`（monorepo 多 target 目录映射）、`cache-all-crates`、`save-if`、`shared-key`。
- 对自研远程服务的价值：其 cache key 设计是直接可抄的清单（见 §13.2）。
- 来源：<https://github.com/Swatinem/rust-cache>

### 9.3 cargo-hakari（workspace-hack，统一 feature）

生成一个 `workspace-hack` 包，把所有 crate 实际用到的依赖 feature 聚合声明，**消除 resolver v2 下同一依赖的多 feature 变体重复编译**。官方数据：大 workspace 构建 / check 提速 **15–95%**，累计 20–25%+。

硬性要求：**必须提交 Cargo.lock**；四步走：`hakari init` → `manage-deps` → 配置 `.config/hakari.toml`（建议 `resolver = "2"`、配置常用 platforms）→ 日常依赖变更后重新 generate。

来源：<https://crates.io/crates/cargo-hakari>

### 9.4 依赖卫生类

| 工具 | 作用 | 备注 |
|---|---|---|
| cargo-machete | 文本扫描找**未使用依赖**，快、stable 可用、CI 友好（退出码 0/1/2），`--fix` 自动删 | 不精确：proc-macro 间接使用会误报，可 metadata ignore；要编译器级精度用 cargo-udeps（需 nightly） |
| cargo-hack | feature 组合测试：`--each-feature`、`--feature-powerset`（全幂集）、`--depth`、跨版本 `--version-range` | 间接提速：保证「任意 feature 组合可编译」，使缓存分片 / 按需 feature 策略不会踩编译失败 |
| cargo-unleash | 大型 monorepo 批量打包 / 发布（`em-dragons`、`--changed-since`） | alpha、约 4 年未更新，历史参考价值为主 |

来源：<https://crates.io/crates/cargo-machete>、<https://crates.io/crates/cargo-hack>、<https://crates.io/crates/cargo-unleash>


## 10. 依赖层加速

### 10.1 Sparse registry index

- 旧 git 协议需克隆整个索引仓库（crates.io-index 数十万文件，首次极慢）；**sparse 协议**按 crate 路径发独立 HTTP 请求取元数据（`sparse+https://index.crates.io/`），支持 HTTP/2 与多路复用；
- **Rust 1.68 stable，Rust 1.70 起 crates.io 默认 sparse**；
- 缓存：Cargo 记录 `ETag`/`Last-Modified`，刷新时条件请求，服务器返回 **304**；两者并存只用 ETag。
- 配置：

```toml
[registries.crates-io]
protocol = "sparse"
```

来源：<https://doc.rust-lang.org/cargo/reference/registry-index.html>、<https://blog.rust-lang.org/inside-rust/2023/01/30/cargo-sparse-protocol/>

### 10.2 镜像与 source replacement

- 通过 `.cargo/config.toml` 的 `[source.crates-io] replace-with` 指向镜像（国内 USTC、字节内 / 腾讯内镜像等），镜像可同时提供 sparse 入口；
- 自建 sparse 代理 / 缓存（如 crates-io-proxy）可把元数据与 crate 包体全部缓存到内网，消除外部延迟与抖动；
- 注意：使用 CDN 的 registry 必须处理缓存失效，否则新版本不可见。

来源：<https://doc.rust-lang.org/cargo/reference/source-replacement.html>、<https://doc.rust-lang.org/cargo/reference/registries.html>

### 10.3 cargo vendor

```bash
cargo vendor ./vendor
# 输出 [source.crates-io] replace-with = "vendored-sources" 配置片段
```

- 把所有依赖包体（含 git 依赖）下载到本地目录并改写 source 指向；构建零网络、零 registry 请求，最适合**空气隔离 / 可复现打包**；
- 代价：vendor 目录需随依赖更新维护；跨多项目重复占用；可作为远程服务的「预热物料」而非每个项目各自长期维护。

来源：<https://doc.rust-lang.org/cargo/commands/cargo-vendor.html>

### 10.4 Git 依赖缓存

- Cargo 把 git 依赖 bare clone 缓存在 `~/.cargo/git/db/`，按 rev 增量 fetch；同 URL 不同 rev 共享一个 db；
- 加速做法：构建机持久化 `CARGO_HOME`；对 GitHub 等用内网代理 / 镜像（避免 CI 高峰期 git fetch 限流）；大 monorepo 型 git 依赖可用 `?rev=` 浅化策略有限，根治方案是推动依赖发布到 registry。

### 10.5 依赖层延迟预算（定性）

git index 克隆冷首次分钟级、热增量秒级；sparse index 冷秒级、热 304 亚秒；crate 包体冷受带宽限、热近零；vendor 为一次性成本、构建零网络。

## 11. 硬件 / 环境层面

### 11.1 tmpfs / ramdisk

- tmpfs：内存文件系统，随机小文件 I/O 比 SSD 高 10–50×；有报告构建从 10min 降到 4min，但也有全缓存放 tmpfs 导致 16GB 机器 OOM 的事故。
- **现代 NVMe + 大内存时 Linux page cache 已吸收大量收益**，Rust 场景官方更推荐 tuned ext4 scratch 盘：
  `mount -o noauto_da_alloc,data=writeback,lazytime,journal_async_commit,nobarrier`（corrode，/u/The_8472）。
- 经验上限：tmpfs ≤ 25% 内存且重启即失，只放会话内数据；Windows 对应 Dev Drive（ReFS + 乐观并发），实测约 20–30% 提速。

### 11.2 并行度

- cargo 默认 `-j` = 逻辑核数；`cargo build --timings` 区分「等 CPU（红）」与「等依赖」；
- LLVM codegen units 提供进程内并行；nightly 前端并行 `-Zthreads=N` 最佳情况约 50% 编译提速，代价是内存；
- 坑：sccache 下过度并行会让「缓存查询」本身成为瓶颈；内存不足时高 `-j` 触发 OOM / swap，反而更慢。应按「核数 × 单 rustc 峰值内存」反推。

### 11.3 CPU 架构

- `-C target-cpu=native` 改善**运行时**（AVX 等指令、更好向量化），不显著改变编译时间；
- 远程打包服务如果给同构机群产包可固定具体 target-cpu（比 native 更可缓存）；若产物要分发到异构用户，保持默认 baseline 以保兼容，缓存键中必须记录该选项。

`SCCACHE_DIR` 放本地 NVMe 或 tmpfs；对象存储后端靠网络 RTT，多级缓存把 P99 延迟压住。构建机选型经验序：多核 + 足内存 > 快本地盘 > 高单核主频（链接 / LTO 吃内存与核数）。

来源：<https://corrode.dev/blog/tips-for-faster-rust-compile-times/>、<https://nnethercote.github.io/perf-book/build-configuration.html>、<https://www.rust-lang.org/blog/2023/11/09/parallel-rustc/>、<https://blog.rust-lang.org/2025/09/10/rust-compiler-performance-survey-2025-results/>

## 12. 2024–2026 新特性与新兴项目

### 12.1 Build Dir Layout v2（2026-03 Call for Testing，需重点跟踪）

把 target 内部布局从「按内容类型组织」改为「按包名 + Unit 及输入的哈希组织」：

```
build-dir/debug/build/<包名>/<单元哈希>/{fingerprint,out,run}
```

- 取消顶层 `.fingerprint/` 与 `deps/`；每个 Unit 成为**自包含目录**（fingerprint + 产物 + build script 运行缓存并列）；
- 铺路目标：**跨工作区缓存（cargo #5931）**、旧单元自动 GC（#5026，解决磁盘无限增长）、细粒度锁（#4282，cargo test 与 rust-analyzer 不再互斥）、中间产物文件名冲突（#16673）；
- 启用：nightly 2026-03-10+，`-Zbuild-dir-new-layout`；跟踪 issue #15010；
- 相关：Cargo 1.91（2025-10）起已支持 **build-dir 与 target-dir 分离**（`CARGO_BUILD_BUILD_DIR`）；
- 对本服务：这是官方方向上与「远程 / 跨工作区缓存」最相关的演进，设计缓存模型时应预留与自包含 Unit 目录对齐的能力。

来源：<https://blog.rust-lang.org/2026/03/13/call-for-testing-build-dir-layout-v2/>

### 12.2 Resolver v3 / MSRV-aware resolver（已 stable）

- Rust 1.84（2025-01）stable：`resolver = "3"`，edition 2024（1.85）隐含启用；
- 行为：优先选择与 `rust-version`（MSRV）兼容的依赖版本（`incompatible-rust-versions = "fallback"`），而不是一味取最新；
- 构建影响：升级后 Cargo.lock 可能出现更旧版本 → Unit 集合 / 产物随之改变；virtual workspace 必须在 `[workspace]` 显式设；CI 可用 `CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=allow` 临时取最新。

来源：<https://blog.rust-lang.org/2025/01/09/Rust-1.84.0.html>、<https://doc.rust-lang.org/edition-guide/rust-2024/cargo-resolver.html>

### 12.3 Public/private dependencies（仍 nightly）

- RFC 1977 / RFC 3516：依赖可标 `public = true`；Cargo 对私有依赖传 `--extern priv:`，rustc 跑 `exported_private_dependencies` lint（**warn 级**，2024 edition 提 deny 的提案未采纳）；
- 仍 nightly-only（`-Zpublic-dependency`，至 Cargo 1.89 仍在迭代）；`workspace.dependencies` 不支持 public；
- 远期价值：私有契约成立后，Cargo 才可能对私有依赖**独立解析、不强制版本统一**——意味着未来 Unit / 缓存模型还会变化，值得跟踪但现在不能依赖。

来源：<https://doc.rust-lang.org/cargo/reference/unstable.html>、<https://internals.rust-lang.org/t/pre-rfc-superseding-public-private-dependencies/19708>

### 12.4 Feature unification 配置化（RFC 3529）

`-Z feature-unification` 增加 `resolver.feature-unification` 配置，显式控制 workspace 内跨包 feature 统一策略——把「重复 Unit（缓存碎片） vs 不想要的 feature 耦合」变成可调参数。与 cargo-hakari 的目标互补。

### 12.5 新兴远程缓存 / 远程执行项目

| 项目 | 形态 | 状态 / 要点 |
|---|---|---|
| **sccache dist** | 官方系最成熟：共享缓存 + icecream 式分布式 | 生产可用多年；含 Rust；认证 / TLS / 沙箱 |
| **rch**（Remote Compilation Helper） | CLI + SSH 卸载到远程 worker；池化 cargo、版本化 target 缓存、稳定源码路径、产物回取；fail-open | 2025 新项目（v2.1.x），原为多 AI agent 并发场景设计（<https://docs.rs/crate/rch/latest>） |
| **cargo-burst** | 按需 Hetzner 云机 + 持久卷保 target/ 与 sccache 温热，空闲销毁 | pre-alpha（<https://crates.io/crates/cargo-burst>） |
| **Bazel + rules_rust + RBE**（BuildBuddy / bazel-remote / buildfarm） | 以 Bazel gRPC 远程缓存 / 远程执行协议承载 hermetic Rust 构建 | OpenAI codex-rs 已在生产用 BuildBuddy（Linux 构建+测试远程；Mac/Win 构建远程、测试本地）；代价是双构建系统（<https://github.com/openai/codex/blob/main/codex-rs/docs/bazel.md>） |
| **zccache** | 自称比 sccache 更低命中延迟（~1ms vs ~170ms）、更快 CI | 2026-04 新工具，厂商自测数据，需观望（<https://pypi.org/project/zccache/>） |

两条路线判断：

1. **Cargo-native**：sccache（缓存）+ sccache-dist（算力）+ 周边（chef/hakari/快链接器），侵入小，是当前自研远程打包服务的现实基座；
2. **Bazel/RBE-native**：hermetic、远程执行能力完整、生态成熟，但要求 rules_rust 迁移与双系统成本；超大 monorepo / 多语言场景可作为二期演进参照（codex 是真实架构参考）。

## 13. 对构建服务设计的启示

### 13.1 可直接采用的技术

| 技术 | 角色 | 优先级 |
|---|---|---|
| sccache + 远端对象存储（S3 兼容内网）+ 本地盘多级缓存 | 跨项目 / 跨会话主缓存，覆盖 rlib codegen | P0 |
| 固定独立 target 目录（每项目 / 每构建会话） | 构建工作树，杜绝共享 target 污染与锁竞争 | P0 |
| mold（Linux 构建机） | 压缩链接阶段；Mac 构建机用 lld 或评估 sold | P0 |
| sparse index + 内网 registry 代理 / 镜像 | 依赖元数据与包体低延迟、可预热 | P0 |
| 持久化 CARGO_HOME（git db + crate 缓存） | 依赖层热复用 | P0 |
| cargo-chef 模式（recipe 思想） | 若服务内部以容器 / snapshot 组织物料，照搬「依赖骨架先行」分层 | P1 |
| cargo-hakari | 面向大 workspace 用户提供「统一 feature」预处理（可作为服务端可选优化） | P1 |
| split-debuginfo=unpacked、panic=abort 等 profile 档位 | 作为「构建档位」暴露给用户选择 | P1 |
| cargo-nextest | 「打包 + 跑测试」时压缩测试阶段 | P2 |
| sccache-dist / Bazel RBE / cranelift | 未命中计算多机摊薄（二期）；cranelift 仅 debug/check 可选加速、不进发布 | P2 |

### 13.2 缓存 Key 应该包含什么

远程缓存（无论是直接用 sccache 还是自研层）的 key 至少包含：

1. 工具链：rustc 完整版本（含 commit hash）、codegen backend（LLVM/Cranelift）、其版本；
2. target：完整 target triple、`target-cpu`、target-feature、链接器类型与版本；
3. Profile：opt-level、codegen-units、lto、debuginfo、split-debuginfo、panic 策略、debug 压缩方式；
4. Unit 身份：crate 名与版本、**已启用与已声明 features 全集**、依赖 Unit 的缓存键（沿图传递）；
5. 输入内容：全部源文件内容哈希（不能只信 mtime）、build.rs 与其声明的 rerun-if 输入文件、`env!` 读取的环境变量白名单；
6. 构建环境：RUSTFLAGS / RUSTDOCFLAGS、影响构建的 config 键、构建容器镜像哈希（系统库版本）、Cargo.lock；
7. 路径归一：使用固定构建根路径或 SCCACHE_BASEDIRS 等价归一化，避免路径碎片击穿命中率。

原则：**内容寻址（content hash）优先，mtime 只作快速路径**；fingerprint 判 Fresh 的本地语义保留在每会话 target 目录，跨机复用全部走内容键。

### 13.3 已知的坑（设计红线）

1. **不要跨项目 / 跨 worktree 共享 target 目录**——会静默产出错误代码（§8.1），这是正确性事故而非性能问题；
2. **不要在 sccache 场景开启增量编译**——互斥，且 release 增量牺牲优化；
3. **不要假设 bin/proc-macro/dylib 能被 sccache 缓存**——链接环节永远在本地发生，必须配快链接器并把链接时间计入预算；
4. **CI / 容器解包必须保留 mtime**，否则 cargo 全量重判；时钟要同步，避免 NTP 跳变；
5. **build script / proc-macro 的隐式输入**（读文件系统、读环境变量、依赖宿主机工具）无法被自动追踪：需要用户声明（类似 rerun-if/白名单）或对这类 crate 保守不缓存；
6. **feature unification 是缓存命中率的最大变量之一**：同一仓库不同请求的 feature 组合不同会导致 Unit 集合不同。设计上应把「feature 组合」作为显式缓存维度，并对高频组合（默认 features、all-features）做预热；可建议大 workspace 用户接入 hakari；
7. **不要把 target-cpu=native 用于分发给异构用户的产物**；同机群内部可用固定具体 cpu 型号，且必须进 key；
8. **tmpfs 不是银弹**：NVMe + page cache 下收益有限且有 OOM 风险，若用需限额 ≤25% RAM 且只放会话内数据；
9. **LTO 档位会重塑时间结构**：fat/thin LTO 下「链接阶段」变重，快链接器收益模型改变，且 LTO 产物缓存更碎，需要单独基准；
10. **演进预留**：Build Dir Layout v2 把 Unit 自包含目录化、官方跨工作区缓存（#5931）在途——自研缓存模型应按「Unit 自包含 + 内容寻址」设计，以便未来与官方方案对齐或迁移。

### 13.4 参考目标架构（示意）

```
请求（repo + rev + features + profile 档位）
  │
  ├─ 物料层：内网 sparse 代理 + crate 包体缓存 + 持久 CARGO_HOME（git db）
  │
  ├─ 调度层：固定路径工作区；每会话独立 target 目录
  │
  ├─ 缓存层：sccache 多级（本地 NVMe → Redis → S3 兼容存储），CARGO_INCREMENTAL=0
  │
  ├─ 执行层：cargo build，mold/lld；高核机；按内存预算定 -j
  │
  ├─ 后处理：split-debuginfo 产物分离、strip、按档位打包
  │
  └─ 增量打包：以 Unit graph 为粒度记录已缓存 Unit；后续版本仅传输/重算缺失 Unit
```

