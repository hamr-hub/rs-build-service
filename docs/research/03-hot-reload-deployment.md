# 各语言热更新 / 热部署 / 热重载技术调研

> 调研时间：2026-09
> 目标：为 Rust 构建打包服务设计「构建完成后如何快速、不停机地把新版本跑起来」的能力。
> 方法：对照各语言运行时的工业实践，按「原理 / 状态如何保持 / 依赖变更如何处理 / 限制 / 成熟度」逐项剖析，最后给出本服务的分层设计。

---

## 目录

1. [先对齐语义：四种「热更新」](#1-先对齐语义四种热更新)
2. [Java 系：JVM 上的类重定义生态](#2-java-系jvm-上的类重定义生态)
3. [Erlang/OTP：工业界最完整的热升级模型](#3-erlangotp工业界最完整的热升级模型)
4. [JavaScript/TypeScript：模块图上的 HMR](#4-javascripttypescript模块图上的-hmr)
5. [Go：快速重编译 + 重启的胜利](#5-go快速重编译--重启的胜利)
6. [Python：reload 局限与 worker 滚动回收](#6-pythonreload-局限与-worker-滚动回收)
7. [C/C++：dlopen 动态库热替换](#7-ccdlopen-动态库热替换)
8. [Rust 现有方案盘点](#8-rust-现有方案盘点)
9. [Rust 做热重载的根本困难与可行路径](#9-rust-做热重载的根本困难与可行路径)
10. [进程级零停机部署](#10-进程级零停机部署)
11. [服务网格层的流量切换与连接排空](#11-服务网格层的流量切换与连接排空)
12. [eBPF 与 live patching：观测、拦截与内核级参考](#12-ebpf-与-live-patching观测拦截与内核级参考)
13. [场景选择矩阵](#13-场景选择矩阵)
14. [Rust 打包服务的热部署能力设计](#14-rust-打包服务的热部署能力设计)
15. [参考资料](#15-参考资料)

---

## 1. 先对齐语义：四种「热更新」

「热更新」在不同语境下是四种完全不同的东西，后文所有技术都可以对号入座：

| 层级 | 替换粒度 | 内存状态 | 连接 / 外部状态 | 典型技术 | 典型延迟 |
|---|---|---|---|---|---|
| L1 函数级 | 单个函数 / 方法体 | 完全保留 | 完全保留 | JVM HotSwap、eBPF uprobe、livepatch | 毫秒 |
| L2 模块级 | 模块 / 类 / 动态库 | 保留（需显式迁移） | 完全保留 | Erlang code upgrade、DCEVM+HotswapAgent、hot-lib-reloader、HMR | 毫秒~秒 |
| L3 进程级 | 整个进程（新二进制） | 不保留（或外部化恢复） | 保留（fd 传递 / 排水） | systemd socket activation、SO_REUSEPORT、gunicorn HUP | 亚秒~数秒 |
| L4 环境级 | 整套环境 / 集群 | 不保留 | 由 LB / 网格切流 | 蓝绿、K8s rolling、Istio 流量切换 | 数十秒~分钟 |

核心规律：**替换粒度越大，对运行时的要求越低、可变更范围越广、但状态保持成本越高**。Rust 作为 AOT 静态编译语言，L1/L2 的施展空间远小于 Java/Erlang，真正的主战场在 L3，并以 L4 兜底。

---

## 2. Java 系：JVM 上的类重定义生态

### 2.1 标准 HotSwap（JPDA / JVMTI）

- **原理**：调试器通过 JDWP/JDI（JPDA 体系）下发指令，JVM 调用 JVMTI 的 `RedefineClasses`（`java.lang.instrument` 的 Instrumentation API 是同一底层能力的封装）。新类的常量池、方法字节码被替换，JIT 失效后重新编译。
- **状态如何保持**：堆上已有对象的实例字段原样保留——**因为类的字段布局不允许变**，旧对象天然兼容新方法体。
- **依赖变更如何处理**：不处理。不能新增/删除方法、字段、接口，不能改父类，只能改方法体内部。
- **限制**：schema 变更直接返回 unsupported-operation；JEP 451 之后动态挂载 agent 受到限制（JDK 21 警告、后续版本默认拒绝），生产环境附加 agent 越来越难。
- **成熟度**：JDK 1.4 起内置，IDE 调试时「Reload Changed Classes」即此，极其稳定，但能力天花板低。

### 2.2 DCEVM 与 JetBrains Runtime

- **原理**：DCEVM（Dynamic Code Evolution VM，源自 Thomas Würthinger 的研究）是 HotSpot 的补丁，通过增强类重定义算法支持增删字段、方法、接口、枚举值、匿名类等结构性变更。旧对象通过类版本间的字段映射「搬迁」到新 schema。
- **状态如何保持**：能迁移的字段自动保留；被删除字段丢弃，新增字段取默认值；静态成员同样迁移。
- **依赖变更如何处理**：配合框架 agent（HotswapAgent）刷新容器元数据，否则只是类换了、Spring/Hibernate 的缓存与代理仍旧。
- **限制**：官方 dcevm.github.io 的补丁只维护到 OpenJDK 11；上游化尝试 **JEP 159（Enhanced Class Redefinition）已关闭/撤回**，JDK 21/24 主线至今无增强热替换；唯一不支持的是改父类 / 移除接口这类继承层级变更。
- **2026 年的获取方式**：**JetBrains Runtime（JBR）** 是事实上的现代 DCEVM 发行版，jbr17/jbr21/JBR25 分支持续维护（2025 年内多次发版）。另一选择是 Oracle GraalVM 的 Java on Truffle（Espresso），热替换能力「接近完整」，但解释执行、非默认热点路径。
- **成熟度**：研究级补丁 → JetBrains 产品每日百万级使用，工程成熟，但属第三方 JDK，非主流生产部署选择。

### 2.3 JRebel（商业）

- **原理**：JVMTI agent + 自有类重定义框架（不依赖 DCEVM），在类加载期插入中介层，并维护极其庞大的**框架插件矩阵**（Spring、CDI、EJB、Hibernate、MyBatis、OSGi、数十种应用服务器）。
- **状态如何保持**：类结构变更 + 框架内部状态（Bean 定义、代理、映射元数据、连接池）联动刷新，业务运行时状态保留。
- **依赖变更如何处理**：新增 jar / 修改配置大部分能映射到其框架插件；超出插件覆盖范围（如 JNI、驱动初始化）需重启。
- **限制**：商业付费、闭源、与 JDK 版本存在追赶期；生产长期挂载 agent 有性能与合规考量（JEP 451 大背景下更敏感）。
- **成熟度**：2007 年至今，商业热部署的标杆，生态覆盖最广。

### 2.4 HotSwapAgent（开源）

- **原理**：开源 javaagent，定位为「JRebel 的免费替代」，**必须配合 DCEVM 能力的 JVM**（现在即 JBR）。负责类重定义之后的框架状态同步，插件化覆盖 Spring、Hibernate、Logback、Jetty/Tomcat 等。
- **现代用法**：JBR + `lib/hotswap/hotswap-agent.jar`，启动参数 `-XX:+AllowEnhancedClassRedefinition -XX:HotswapAgent=fatjar`，模式分 `fatjar / core / external`。
- **状态如何保持**：随 DCEVM 的字段迁移 + 框架插件重建 Bean/代理/映射。
- **限制**：框架覆盖面不如 JRebel；继承层级变更仍不支持；插件质量参差。
- **成熟度**：社区活跃（文档 2025 年仍持续更新），Java 系免费方案首选组合 **JBR + HotswapAgent**。

### 2.5 Spring DevTools（双 ClassLoader restart）

- **原理**：并非真热替换，而是**让重启更便宜**。两个 ClassLoader 分工：
  - base classloader：第三方 jar，基本不变；
  - restart classloader：项目代码；变更时整个丢弃重建，base 保留。
- **状态如何保持**：不保留应用内存状态——Spring 应用上下文重新初始化（Bean 重建、类路径扫描重跑）。
- **依赖变更如何处理**：通过 `META-INF/spring-devtools.properties` 的 `restart.include/exclude` 把 jar 在两个 loader 间重新划分；触发器文件（trigger-file）模式可把重启收敛到显式动作。
- **限制**：本质是重启，上下文初始化开销仍在；2026 年基准约 8 秒/次增量。
- **成熟度**：Spring Boot 官方内置，事实标准，但只解决开发期问题。

### 2.6 Spring Loaded

- 早于 DevTools 的开源 javaagent（Grails 团队），运行时拦截类加载做方法/字段级增量，无需重启。项目基本停止维护，被 DevTools 与 JBR+HotswapAgent 路线取代，新项目不应选用。

### 2.7 Quarkus dev mode：构建期增强 + 可选字节码原地替换

- **原理（与 Spring 路线的根本差异）**：Quarkus 把框架引导工作搬到**构建期**：
  - Jandex 索引注解与字节码（不加载应用类）；
  - Gizmo 生成「录制好的」直接实例化运行时服务的字节码；
  - 引导逻辑进入静态初始化段（native image 中直接折叠进镜像）。
  `quarkus:dev` 下保存文件触发后台重编译 + 重新部署，通常在下次请求/刷新时完成；开启 `quarkus.live-reload.instrumentation=true` 后，兼容变更可走内置 `quarkus-class-change-agent` 做**原地类重定义**，连重新部署都跳过。
- **状态如何保持**：重新部署时上下文重建（但因运行时引导极少而很快，2026 基准约 3 秒/次）；instrumentation 路径按 HotSwap 语义保留实例。
- **Dev Services**：自动起 PostgreSQL/Redis/Kafka 等容器，与代码热替换正交；2025-2026 年（3.22→3.28）专门重构了 Dev Services 与增强阶段的时序，消除重复容器问题。
- **成熟度**：Red Hat 支持，社区增长快；体现了「把运行时工作前移到构建期」的先进思路——对 Rust 打包服务有直接借鉴意义。

### 2.8 ClassLoader 层级热替换原理（为什么 Java 能按模块热换）

- 类的身份 = `ClassLoader 实例 + 全限定名`。同一个 `.class` 由不同 ClassLoader 加载就是两个互不兼容的类（instanceof 失败、ClassCastException）。
- 层级委托模型（bootstrap → platform → application → 自定义）让每个 webapp / 插件拥有独立子 loader；卸载时解除对 loader 的全部引用（实例、类对象、线程上下文、ThreadLocal）即可 GC。
- 热替换套路：新建一个同层 loader 加载新版 → 把路由/注册表的引用原子切换到新类 → 旧 loader 排空后卸载。难点从来不是「加载」，而是**引用清理**：泄漏一条引用就造成元空间泄漏。
- 对 Rust 的启示：dlopen 路线的「身份与卸载」问题与此同构（见 §9）。

### 2.9 Java 方案横向对比

| 方案 | 粒度 | 结构变更 | 状态保持 | 依赖/框架刷新 | 生产可用 | 成本 |
|---|---|---|---|---|---|---|
| HotSwap | 方法体 | 否 | 全保留 | 无 | 是（调试通道） | 免费 |
| JBR(DCEVM)+HotswapAgent | 类 | 除继承层级外均可 | 字段迁移+框架插件 | 插件覆盖 | 不推荐常态挂载 | 免费 |
| JRebel | 类 | 广泛支持 | 全保留+框架联动 | 覆盖最广 | 商业支持 | 付费 |
| Spring DevTools | 上下文重启 | 任意 | 不保留 | 重新引导 | 否 | 免费 |
| Quarkus dev | 重部署/可选原地 | 重部署任意 | 部分 | 构建期模型 | dev only | 免费 |

---

## 3. Erlang/OTP：工业界最完整的热升级模型

Erlang 是唯一一个把「运行中升级」写进语言与运行时设计 DNA 的体系，电信系统要求数年不停机。值得逐组件讲透。

### 3.1 code server：模块的唯一权威

- 每个节点有一个 code server（`code` 模块），负责在 BEAM 代码路径（code path）中查找并加载 beam 文件。
- 模块版本在 VM 内以 **「当前版 current」+「旧版 old」两段式**保存：`code:load_file(Mod)` 时，原 current 降级为 old（外部函数仍可进入 old 的进程继续执行完），新加载的成为 current。
- 关键机制：**函数是完全限定调用（external call，`Mod:Fun(Args)`）时在调用点查 current 表**，因此新请求天然进新代码；而模块内部的本地调用（`Fun(Args)`）是静态直跳，正在旧代码里执行的进程会把旧版执行到底——这就是「两版共存、自然过渡」。
- `code:purge/1`（ brutal_purge 会杀掉还滞留旧版的进程）与 `code:delete/1` 控制旧版何时彻底消失。每个模块最多两版共存，第三次加载前必须 purge。

### 3.2 进程状态迁移：code_change/3 与 gen_server

- gen_server 等行为在收到升级指令时经历 `suspend`：系统把进程挂起，调用回调：

  ```erlang
  code_change(OldVsn, OldState, Extra) -> {ok, NewState}.
  ```

  开发者在此把旧版 State 数据结构显式转换成新版——这是 Erlang 模型中**唯一需要手写、也因此最可控的状态迁移点**。
- 对纯函数式的 BEAM 而言，进程状态就是归约机（reduction machine）里的一个绑定值，替换它没有 C/Rust 那种「堆上旧对象布局不兼容」问题。
- supervisor 升级用 `{advanced, ...}` 指令触发其 code_change（可改变子规格、增删子进程）。

### 3.3 appup：每个应用的升级脚本

- `.appup` 文件描述应用从各旧版本升级 / 降级的指令序列：

  ```erlang
  {"2",
   [{"1", [{load_module, ch_rel},
           {suspend,[ch_sup]},
           {code_change,up,[{ch_sup,[]}]},
           {resume,[ch_sup]}]}],
   [{"1", [{suspend,[ch_sup]},
           {code_change,down,[{ch_sup,[]}]},
           {resume,[ch_sup]},
           {load_module, ch_rel}]}]}.
  ```

- 常用高级指令：`add_module/delete_module/load_module`、`update`（按模块依赖自动排序的批量更新）、`{apply,M,F,A}`（执行任意数据迁移函数）、`add_child/delete_child/restart_children`、`purge`、`point_of_no_return`（过此点降级不再保证安全）。

### 3.4 relup 与 release_handler：全系统编排

- 发布单元是 release（一组应用 + ERTS + 配置）。`systools:make_relup/3,4` 汇总各应用 appup，生成整个 release 的升级脚本 **relup**。
- **release_handler（SASL 应用）** 是运行时的升级执行者：解包新 release tar → 安装为 `releases/<Vsn>` → 按 relup 顺序执行指令 → 原子提交或回滚，并维护 permanent / transient / temporary 状态。升级失败可回滚到上一 permanent 版本。
- 打包：`systools:make_tar/2`，可含 priv 资源、可含新 ERTS。

### 3.5 ERTS 自身升级（虚拟机换底）

- 当升级包含 ERTS/kernel/stdlib 时，指令中自动插入 `restart_new_emulator`：新 beam 虚拟机先启动、通过节点间握手接管，旧 VM 排空后退出——本质是**进程级 L3 替换由 L2 框架自动编排**。`restart_emulator` 用于其他必须重启 VM 的场景。
- 这给出一个重要范式：**模型再完整，也要为「运行时/底层依赖变更」保留一条进程级切换通道**。

### 3.6 成熟度与代价

- 爱立信等电信系统数十年验证，可靠性模型工业界第一。
- 代价：每个可能跨版本存活的进程都要写并持续维护 `code_change`，appup 要覆盖所有升级路径（N 版本 → N² 测试组合），工程纪律成本极高；现实中很多 Elixir/Erlang 团队日常发布也只用集群滚动重启（节点摘流 → 重启 → 回流量），把完整 relup 留给关键电信场景。
- 对 Rust 的启示：**状态迁移必须显式、可测试、可回滚；框架负责编排顺序，不负责猜语义**。

---

## 4. JavaScript/TypeScript：模块图上的 HMR

### 4.1 webpack HMR：完整的图算法范本

- **编译器侧**：watch 模式下除普通产物外，额外产出「更新包」：
  - manifest（`[runtime].[hash].hot-update.json`）：变更 chunk 列表、删除的模块；
  - chunk 更新（`[id].[hash].hot-update.js`）：新模块代码。
  前提是模块 ID / chunk ID 在多次构建间稳定（dev-server 内存中天然成立，跨构建用 `recordsPath`）。
- **运行时侧**：注入的 HMR runtime 为模块图维护每个模块的 `parents / children`。
  - `check`：浏览器走 HTTP 拉 manifest（`target: 'node'` 时走 `fs.readFile`），下载变更 chunk；
  - `apply`：把变更模块标记失效，失效沿 **parent 链向上冒泡**，直到遇到注册了 `module.hot.accept` 的边界；一路冒泡到入口无人接管 → 放弃更新并整页 reload。
- **API**：`hot.accept(cb)`（自身成为边界）、`hot.accept(deps, cb)`（接管子模块）、`hot.dispose(cb)`（清理旧副本的定时器/订阅/socket）、`hot.data`（跨更新持久化的状态对象）。
- **状态保持**：完全由边界代码负责。React/Vue 等框架插件把组件注册成边界从而保住 hooks/组件态。
- **成熟度**：定义了 HMR 的标准语义，Rust 的 hot-lib-reloader、Dioxus Subsecond 都在复刻这套「边界 + 冒泡 + dispose/data」模型。

### 4.2 Vite：原生 ESM 时代的 HMR

- **依赖预构建**：node_modules 依赖用 esbuild（新版本正迁移到 Rust 写的 Rolldown）打包成少量 ESM，消灭 CJS/UMD 兼容与瀑布请求；裸导入被重写到 `/node_modules/.vite/deps/...?v=hash`，永久强缓存。
- **源码侧**：以原生 ESM 提供，浏览器按需请求、按需编译。文件变更时只让**编辑点到最近 HMR 边界之间的链路**失效，更新速度与项目总规模无关；变更经 WebSocket 推送，模块以 cache-busting 查询参数重新拉取。
- **HMR API**：`import.meta.hot.accept/dispose/prune/invalidate/data`，用法必须被 `if (import.meta.hot)` 包裹以便生产 tree-shake。注意 Vite 不替换「最初被导入的模块」、不生成代理模块，边界模块自己用 `let` 更新再导出。
- **成熟度**：2026 年前端 dev server 的事实标准；esbuild/ Rolldown 把「预构建」从分钟压到秒级，是「工具性能决定热更体验」的典型。

### 4.3 React Fast Refresh

- React 官方热替换方案（替代 React Hot Loader），由 `@vitejs/plugin-react[-swc]` 等注入注册代码：
  - 每个组件文件成为 HMR 边界，保留 hooks 状态与本地 state；
  - 规则：文件只导出组件时可保状态；混入非组件导出 → 退化为整页刷新（更新 Babel/SWC 插件有放宽）；
  - 对错误边界、class 组件、ref 的语义有专门处理（出错后保留可恢复状态而非白屏）。
- 成熟度：React 团队官方维护，所有现代 React 脚手架默认集成。

### 4.4 服务端 HMR（Node.js）

- **朴素路线**：CommonJS 按绝对路径永久缓存模块，`delete require.cache[path]` 后重新 `require` 即得新代码；必须递归清理 children（否则子模块仍旧），配合 chokidar/fs.watch 使用。
- **局限**：无 accept/dispose 生命周期、旧副作用（定时器、连接监听、事件监听）不会被拆除、无状态保持；只能用于开发，且要小心重复监听端口。
- **工程路线**：webpack `target: 'node'` 的 HMR（读磁盘更新包、图感知冒泡），或直接由 nodemon/tsx/ts-node-dev 重启进程。生产侧与其他语言一样走进程级滚动。

---

## 5. Go：快速重编译 + 重启的胜利

### 5.1 静态二进制之后的现实

Go 编译为（接近）静态的单一二进制，启动快、构建有激进的增量缓存。生态结论非常一致：**不在进程内热换代码，而是把「重编译 + 重启」做到亚秒级**。

### 5.2 watcher 工具

- **air**（air-verse，2026 年标准选择）：fsnotify 监听 → 防抖 → `go build`（吃增量缓存）→ 旧进程 SIGINT、超时 SIGKILL → 启动新二进制；**构建失败时保留旧进程继续跑**；附带浏览器刷新代理。
- realize（早期方案，基本停维护）、go-task（通用任务执行器）类似。
- 整个循环通常 <1 秒，且产出的就是要发布的那个二进制，零架构侵入。

### 5.3 plugin 包为什么被放弃

`go build -buildmode=plugin` 产出 `.so`，`plugin.Open` 加载。官方文档自带 Warnings：

| 问题 | 说明 |
|---|---|
| 平台 | 仅 Linux/FreeBSD/macOS，无 Windows |
| 不可卸载 | 插件只初始化一次、无法 Close，每次重载换文件名 → 内存只涨不降 |
| 工具链一致 | 宿主与插件必须**完全相同**的 Go 版本、编译标签、flags、环境变量，否则崩溃 |
| 依赖一致 | 共同依赖必须同一份源码；类型身份按包路径+版本严格判定 |
| 部署 | 需要 CGO/gcc；执行原生代码无沙箱 |

Go 团队自己的建议是：要么源码 blank-import 整体编译，要么走 IPC/RPC（HashiCorp go-plugin 即子进程 + RPC，可跨平台、可重启、可杀）。

### 5.4 启示

Go 与 Rust 处境最相似（AOT、无稳定跨版本 ABI、鼓励静态二进制），其生态选择是经过大量试错的答案：**开发体验靠「快」解决，生产可靠性靠进程/IPC 边界解决，而不是死磕进程内热替换**。

---

## 6. Python：reload 局限与 worker 滚动回收

### 6.1 importlib.reload 的局限

- 只重载模块自身，**不递归子模块**：`pkg.submod` 等缓存引用仍指向旧对象。
- **旧实例不迁移**：reload 前创建的对象仍绑定旧类；新旧类身份不同，isinstance 行为分裂。
- 模块级状态被重新执行而非合并；多线程下可能暴露半初始化状态。
- 多进程盲区：每个 worker 独立 `sys.modules`，单进程 reload 无法广播。

### 6.2 开发期：watchfiles + 整进程重启

- `uvicorn --reload`：基于 **watchfiles**（Rust 实现的通知库，Python 绑定），重启的是整个 server 子进程（绕过 reload 语义陷阱）；`--reload` 与 `--workers` 互斥；有 `reload_delay`（默认 0.25s）。
- `gunicorn --reload`：同为开发用途，inotify/poll 引擎；与 `preload_app` 不兼容；reload 期间出错会把完整 traceback 暴露给客户端，安全上必须限定开发环境。

### 6.3 生产期：多 worker 滚动回收

- **SIGHUP**：gunicorn master 持有监听 socket，启动新 worker、优雅排空旧 worker；在飞请求跑完，新 worker 从磁盘加载新代码。**前提是不开 `preload_app`**（开了应用在 master 导入，HUP 不会重新导入）。
- **USR2**：二进制级升级，新旧 master 并存，旧 master 保留用于回滚；preload 场景必须走这条。
- 配套：`--max-requests + jitter` 周期性回收 worker 抗内存泄漏；systemd `ExecReload=kill -HUP $MAINPID`。
- uvicorn 自带多 worker 管理器同样支持 SIGHUP 逐个优雅重启。

**小结**：Python 生态明确否定了「进程内 reload 用于生产」，统一到进程级滚动，与 Go 殊途同归。

---

## 7. C/C++：dlopen 动态库热替换

### 7.1 基本机制

- `dlopen(filename, RTLD_NOW/RTLD_LOCAL)` → `dlsym` 取函数/工厂符号 → `dlclose` 卸载（POSIX）；Windows 对应 `LoadLibrary/GetProcAddress/FreeLibrary`。
- 行业惯例（游戏开发数十年经验）：
  1. **冻结 C ABI 边界**：只导出 `extern "C"` 的 `create/destroy/update` 工厂，接口放一个带版本号的函数表结构体；
  2. **状态留在宿主**：世界/实体状态不放动态库，库只持有可丢弃的临时工作集；
  3. **静止时卸载**：帧边界 / 安全点确认没有线程在库代码内，再 close→open（或新旧双缓冲切换）；
  4. **原子安装**：先构建到临时路径、在「测试 loader」里校验 `api->version`，再原子 rename 到正式路径，杜绝加载写了一半的库。

### 7.2 状态如何保持

- 库内静态变量 / 单例随 dlclose 全部销毁，重载后重新初始化——这是状态丢失主源，所以架构上必须把持久状态外置给宿主，每帧/每次调用通过参数传入。
- 想跨卸载保留状态：宿主侧维护；或导出 `save_state/load_state` 走序列化。

### 7.3 两个著名坑

- **GCC 的 `STB_GNU_UNIQUE`**：模板静态、函数局部静态常被标成 unique 符号，`dlclose` 不会真正卸载，每次重载泄漏一份映射。必须用 **`-fno-gnu-unique`** 编译被重载库。
- **Windows 文件锁**：已加载的 DLL 不能被覆盖，套路是输出到临时文件再 rename。

### 7.4 gdb 与 SONAME

- gdb 通过 r_debug / link map 自动跟踪 dlopen 事件加载新符号；`set stop-on-solib-events 1`、`info sharedlibrary`、`sharedlibrary` 可核对；gdb 脚本也可反向用于向运行中进程注入 .so（社区 HMR 注入器原理）。
- **SONAME 版本化**：`libfoo.so.1.2.3` → SONAME `libfoo.so.1`（主版本即 ABI 版本）→ dev 符号链接 `libfoo.so`。不同主版本可在同一进程共存而不互相干扰，是依赖变更管理的底层纪律。

### 7.5 成熟度

- 游戏（客户端 dev）、数据库扩展、交易系统插件等几十年验证，机制可靠；但「可靠」的前提是 C ABI + 状态外置 + 安全点三条铁律，本质仍是 **L2 模块替换 + L3 的安全点纪律**。Rust 的 dylib 热重载完全建立在这套机制之上。

---

## 8. Rust 现有方案盘点

### 8.1 libloading（基础件）

- `dlopen/LoadLibrary` 的安全封装：`Library::new`、`unsafe { lib.get::<fn(...)>(symbol) }`。
- 只解决「加载与取符号」；ABI 安全、重载时序、卸载、状态全部使用者自负。整个生态的地基。

### 8.2 hot-lib-reloader（rksm，即题中 ksm2 所指）

- 作者 Robert Krahn。被重载代码放进独立 crate（`crate-type = ["rlib","dylib"]`），函数 `pub` + `#[unsafe(no_mangle)]`（edition 2024 语法），`#[hot_module]` 宏生成转发与重载逻辑。
- 2026 现状：**0.8.x（2025-08 发布 0.8.0）**，稳定版 Rust 可用（旧版需 nightly 的要求已取消），含 LibReloadObserver 事件、多库、500ms 防抖可配、影子文件名（pid/uuid 防锁）、macOS 自动 codesign；官方示例覆盖 Bevy / egui / Iced / nannou / tokio / 序列化状态。
- 硬性限制：
  1. 不能改函数签名（改了即崩溃）；
  2. 共享 struct/enum 布局不能改（UB）；
  3. 不能导出泛型函数（monomorphization + no_mangle）；
  4. 库内全局状态 / thread_local 重载后需重新初始化；**`TypeId` 每次重载会变**，破坏依赖稳定类型 ID 的代码（ECS、注册式设施）；
  5. 与 `tracing` 有已知冲突（DefaultCallsite 重复注册崩溃）；
  6. 明确仅限开发，禁止生产。
- 推荐架构与 C++ 铁律一致：functional core / imperative shell——状态与主循环在二进制，热换纯-ish 的 update/render。

### 8.3 Bevy 生态

- **资产热重载**：Bevy 原生支持（scene、纹理等改动即生效，`file_watcher` 配置 ChangeWatcher），与代码热替换是两回事。
- **bevy_dylib / dynamic_linking**：用动态链接加速增量编译，dev only，release 必须关。
- **bevy_dynamic_plugin 已在 0.15 删除**（0.14 以 unsound 弃用），勿再使用。
- **bevy_hotpatching_experiments（Simple Subsecond）**：2025 年最新潮方案，基于 Dioxus 的 **Subsecond** 引擎，`#[hot]` 标记系统、`dx serve --hot-patch`；配 nightly + mold + cranelift 可亚秒。默认只能改函数体；签名变更、setup 重跑需显式开关；热补丁系统是 exclusive system，不参与并行调度；组件顶层可迁移、字段内部类型变更基本 UB；只跟踪 tip crate。
- **dexterous_developer**：重型方案，特色是用 rmp_serde（MessagePack）序列化 Resource/Component，**允许 schema 随时间演进**，支持跨设备/容器；scope 内外混用类型有 UB 风险。

### 8.4 abi_stable / stabby：把 ABI 做稳

- **abi_stable**（0.11.x）：Rust-to-Rust 动态库的 ABI 安全层：
  - `#[sabi_trait]` 生成 FFI 安全 trait object（虚表布局稳定）；`DynTrait` 多 trait 对象可 downcast；
  - `StableAbi` + 加载时布局校验；prefix types（末尾加字段不破 ABI）、`NonExhaustive`（可加枚举变体）支持 semver 演进；
  - 三层架构 interface/implementation(cdylib)/user。**不支持卸载**。
- **stabby**（Zama，2025 活跃）：另一套 ABI 稳定方案，可在非动态库场景也导出稳定 ABI。
- 定位：解决「跨编译器版本的插件」问题，是对 hot-lib-reloader「必须同版本工具链」约束的升级，但布局检查≠状态迁移，重载后语义仍需自己处理。

### 8.5 脚本路线（绕开重编译）

- **Rhai**（嵌入式 Rust 脚本，官方有 hot-reload 章节）：整体替换 AST，或 `retain_functions` 做函数级补丁；`Arc<RwLock<AST>>` + `sync` feature 支持多线程。
- 同类：**Rune**、**Ad Astra**（2024 起，类 Rust 语法带 LSP）、Lua（mlua）。适合「业务规则频繁变、性能非热点」的场景。

### 8.6 watcher 与无缝重启配套

- **cargo-watch**：处于「生命维持」状态，作者推荐迁移到 **Bacon** 或 **watchexec**；默认 `cargo check`，可 `-x run/build`。
- **watchexec**：独立 CLI + 可作为库嵌入（需要管子进程用它，只要事件用更底层的 **notify**）。
- **systemfd / catflap**：开发期 socket 保活工具——先占住端口，`cargo watch -x run` 时把 LISTEN_FDS 交给新进程，避免重启窗口的 connection refused。这是开发版的 fd-passing supervisor。

### 8.7 Shuttle 的部署体验（平台视角参照）

- `cargo shuttle deploy` 一条命令；`#[shuttle_runtime::main]` 注解 + 签名注入资源（DB、secrets），无需 Dockerfile。
- 新平台（2024Q4 起）构建与部署分离：CodeBuild 产镜像 → ECS Fargate VM 级隔离运行，官方提供**零停机部署 + 优雅回滚**；2025-10 起 GitHub push 自动构建部署。
- 参照价值：**用户感知层只应暴露「deploy / rollback / status」，零停机是平台内部实现；构建缓存与产物管理是体验关键**。

### 8.8 Rust 生态成熟度速览

| 方案 | 用途 | 成熟度 | 生产可用 |
|---|---|---|---|
| libloading | FFI 加载地基 | 高 | 是（需自管安全） |
| hot-lib-reloader | dev 函数热换 | 中高（活跃） | 否 |
| Subsecond/bevy_hotpatch | dev 热补丁 | 中（实验性） | 否 |
| dexterous_developer | dev 带状态迁移 | 中 | 否 |
| abi_stable/stabby | 跨版本 ABI 插件 | 中高 | 是（无卸载） |
| Rhai/Rune | 运行时脚本热换 | 高 | 是 |
| cargo-watch/watchexec/bacon | watch 触发 | 高 | dev |
| systemfd/catflap | dev socket 保活 | 中 | dev |
| Shuttle | 托管部署平台 | 高（商业） | 服务本身 |

---

## 9. Rust 做热重载的根本困难与可行路径

这是本调研的核心理论章节。Rust 在 L1/L2 热重载上面临四重根本性困难，全部源于语言设计取舍。

### 9.1 没有稳定 ABI（第一困难）

- Rust 的 ABI **在不同编译器版本之间显式不保证稳定**：调用约定、符号 mangling（legacy vs v0）、repr(Rust) 的字段排列都可能变。
- 后果：dylib 与宿主必须用**同一 rustc 同一次产物体系**编译。跨版本时通常表现为「找不到符号」（fail loud，尚可接受），但同版本内 `repr(Rust)` 布局是 rustc 自由决定的——改了字段顺序/枚举变体，跨越边界传引用就是静默 UB（fail silent，最危险）。
- 缓解：C ABI 边界（`extern "C"` + `#[repr(C)]`）、abi_stable 的布局自检、锁 `rust-toolchain.toml`。没有一条让「随便改结构」成立。

### 9.2 单态化（monomorphization）

- 泛型在编译期为每个具体类型参数生成一份专有代码。`fn foo<T>(x: T)` 不存在「一个叫 foo 的符号」，而是 `foo::<u32>`、`foo::<String>` 等 mangled 实例。
- 后果：`#[no_mangle]` 无法作用于泛型，dlsym 找不到稳定入口；泛型函数基本无法热替换。trait 泛型、impl 块同理。
- 缓解：把热换函数写成非泛型；泛型外壳在宿主、单态化后的具体调用点留在稳定侧；或用 trait object 去单态化（但 Rust trait object 虚表 ABI 本身不稳，需 abi_stable）。

### 9.3 静态链接与「每个库一份世界」

- 默认 rlib 静态链接，一个 dylib 会把用到的标准库/依赖打进来。宿主 + 插件若各带一份 `std`，则：
  - 全局状态（allocator、`std::sync::Once`、日志 callsite、panic hook、线程局部存储）存在**多份独立副本**；
  - 跨边界传 `Box/Vec/String` 用 A 端的分配器分配、B 端的分配器释放 → 堆损坏。
- 缓解：`RUSTFLAGS="-C prefer-dynamic"` 强制共享 libstd（同 §8.3 bjorn3 的确认：同版本 + 同一份 libstd 时 Rust ABI 实践上可工作），但这与 release 静态分发目标冲突，且只是开发期约定。

### 9.4 所有权与状态迁移（语义层困难）

- 即使布局问题解决，热替换还要求回答：旧版正在借用中的值怎么移交？新代码需要的新字段从哪来？旧代码 spawn 的线程、持有的锁、未完成的 future 由谁取消？
- Erlang 靠 `code_change` 显式转换不可变 State；JVM 靠 GC + 字段默认值兜底。Rust 没有运行时帮忙：所有权要求**唯一主人**，新旧两版并存时一个值不能有两个 owner；跨版本迁移只能靠 `unsafe` 重解释（UB 风险）或序列化/反序列化（安全但要求类型可序列化、有默认/迁移语义）。
- `TypeId` 随每次 dylib 加载变化（同一编译器也会），导致依赖类型注册表的设施（ECS、扩展系统、tracing callsite）把「同一类型」认成两个——这是运行时缺少「类型身份锚点」的表现。

### 9.5 可行路径总结

| 路径 | 解决什么 | 残留代价 | 适用 |
|---|---|---|---|
| C ABI + no_mangle 函数热换 | 调用约定稳定 | 签名/布局冻结、无泛型 | dev L2 |
| abi_stable/stabby | 跨版本布局与虚表 | 不支持卸载、需按其模型定义类型 | 插件体系 |
| 序列化状态跨重载 | 安全的状态迁移 | 要求 Serialize + 默认值/版本迁移 | dev L2（dexterous 路线） |
| 同工具链 + prefer-dynamic | 共享 std/分配器 | 与静态发布冲突 | dev L2 |
| 脚本 AST 替换 | 无重编译热换 | 仅限脚本表达的业务逻辑 | 业务规则 |
| **进程级二进制切换** | 任意变更（含依赖、泛型、布局） | 不保留内存态（外部化恢复） | **生产 L3，主战场** |
| 蓝绿 / 滚动（L4） | 环境级兜底 | 需编排与流量设施 | 大规模/高危变更 |

**结论先行**：Rust 的生产热部署不应追求 Erlang 式 L2，而应把 L3（零停机进程切换）做到极致，L2 仅作为开发期体验工具。

---

## 10. 进程级零停机部署

L3 的本质：**新二进制是全新进程，不继承内存；但监听端点不中断、外部状态（DB/缓存）天然共享，内存状态通过外部化（DB/缓存/文件/memfd）在启动时重建**。

### 10.1 systemd socket activation + fdstore（现代首选单机方案）

- **socket activation**：systemd 先创建并监听 socket（`.socket` 单元），服务启动时把 fd 以 `LISTEN_FDS` 经 execve 传给服务。服务只 `accept` 不 `bind`，因此重启服务期间内核持续监听、连接在内核队列里等待，不产生 RST。
- **fdstore**（关键扩展）：服务可在运行时把 fd **上传给 systemd 托管**（systemd 持 dup 副本），下次激活原样发还。官方推荐：连接一进来就把连接 fd 存入 fdstore，升级重启后新进程取回这些连接继续服务——**已建立的 TCP 连接不断**。还可用 memfd 把序列化状态存进 fdstore，实现带状态重启。
- 官方明确**不推荐**让旧进程残留、用 SCM_RIGHTS 在新旧进程间直传（必须 KillMode=none，破坏生命周期、安全上下文与资源计数）；fdstore 以 PID 1 为中立托管方是正解。
- soft-reboot（只换用户空间）时 fdstore 也可跨周期保留。
- Rust 对接：`sd_listen_fds`（listenfd / socket2 生态），产物侧只要求服务支持「从 fd 0 起始的已激活 socket」。

### 10.2 SCM_RIGHTS 与 SO_REUSEPORT

- **SCM_RIGHTS**：经 AF_UNIX 辅助消息在进程间传 fd（Linux 另有 pidfd 传递，systemd v258 起 `AcceptFileDescriptors`/`PassPIDFD` 可管控）。适合无 systemd 场景下 supervisor 与服务间的定向交接；代价是必须自行处理新旧并存时序。
- **SO_REUSEPORT**：多进程/多实例 bind 同一端口，内核按连接（四元组哈希）在实例间分发（systemd `ReusePort=` 可设）。滚动时新老实例同时接流，旧实例停 accept、排空在飞连接后退出。
- 注意：REUSEPORT 是「负载共享」，不保证已建立连接接续；UDP/QUIC 场景的迁移另有难题（Cloudflare 的 udpgrm 即在解 QUIC 重启慢的问题）。

### 10.3 蓝绿 / LB 健康检查摘除

- 蓝绿：绿环境跑新版本并通过健康检查后，LB/nginx/HAProxy 把上游原子切到绿；蓝保留用于回滚。代价是双倍容量。
- nginx：上游服务器 `down` / 健康检查（主动 `health_check` 或被动失败计数）摘除，`nginx -s reload` 不断连接地换配置；配合 `keepalive_timeout` 与优雅 shutdown 排空。
- HAProxy：`set server ... state ready/drain` 通过 stats socket 动态摘流，drain 模式只维持存量连接、不接新流。
- 通用纪律：**先摘流、再等 LB 同步、再发 SIGTERM**——直接杀进程会让 LB 把新连接发给已死实例产生 5xx。

### 10.4 Kubernetes rollingUpdate

- Deployment 参数：
  - `maxSurge`：超出期望副本的额外 Pod 数（默认 25%）；
  - `maxUnavailable`：允许不可用数（默认 25%）。
  - 零停机配方：`maxSurge: 1`（或 25%）+ `maxUnavailable: 0`——新 Pod Ready 入 Endpoints 后才删旧 Pod。
- Pod 终止时序：preStop hook（常用 `sleep 5-10`，等待 kube-proxy/iptables 与上游摘除生效）→ SIGTERM → `terminationGracePeriodSeconds`（默认 30s）超时 SIGKILL。
- 应用必须实现 graceful shutdown：收到 SIGTERM 后停止接受新请求、排空在飞请求、关闭连接池，再退出；就绪探针（readiness）在 terminating 状态的处理是常见踩坑点。
- StatefulSet 有顺序保证（逆序滚动）；节点侧 drain 超时与 maxSurge（AKS 建议 33%）是节点升级参数。

### 10.5 各机制对比

| 机制 | 保连接 | 已建立连接 | 依赖 | 适用 |
|---|---|---|---|---|
| socket activation | 监听不断 | 队列等待 | systemd | 单机/VM |
| fdstore | 不断（取回继续） | 完整接续 | 新版 systemd | 单机高要求 |
| SCM_RIGHTS | 可接续 | 定向交接 | 自管 supervisor | 容器/无 systemd |
| SO_REUSEPORT | 监听不断 | 不接续旧连接 | 内核 ≥3.9 | 多实例共享 |
| 蓝绿 + LB | 不断 | 排空不接续 | 双倍容量 | 回滚要求高 |
| K8s rolling | 不断 | 排空不接续 | K8s | 容器编排 |

---

## 11. 服务网格层的流量切换与连接排空

### 11.1 Istio / Envoy

- 切流粒度：VirtualService 的权重路由（金丝雀 90/10）、DestinationRule subsets、mirror（流量镜像）。L4/L7 均可，无需应用感知。
- **连接排空是网格滚动的核心坑**：
  - Pod 终止时 istio-agent 让 Envoy 进入 drain（拒新连接、存量继续），但 `terminationDrainDuration` **默认仅 5 秒**，随后强杀 → 长请求被切、表现 503/RST；
  - Envoy 底层 `drainDuration` 与 `parentShutdownDuration`（后者必须更大）；
  - 正解：注解 `proxy.istio.io/config: terminationDrainDuration: 30s`，且 **drain < terminationGracePeriodSeconds**；或 preStop 轮询连接表直到归零。
- 完整时序：preStop sleep（摘 Endpoints + xDS 同步）→ 应用停接新请求并排空 → sidecar drain ≥ 最长请求 → Kubelet 最后才 SIGKILL。三层（K8s、应用、sidecar）窗口必须嵌套对齐。

### 11.2 Linkerd / Envoy Gateway

- Linkerd 偏「零配置 + 轻量」，split（ServiceMeshTrafficSplit）做金丝雀；数据面 Rust 写的 linkerd2-proxy 同样支持 drain。
- Envoy Gateway 文档给出 graceful shutdown / hitless upgrades 的标准操作：健康检查失败摘端点 + drain timeouts 配合，适用于非 K8s 或自建网关。

### 11.3 连接排空的通用语义

无论 nginx、HAProxy、Envoy 还是 gunicorn，drain 都是同一句话：**宣布「不接新的」+ 给「旧的」一个有上限的完成窗口 + 到点强杀**。差异只在默认窗口长短与配置位置。打包服务必须把「排空窗口」作为显式可配参数，而非依赖各组件默认值（5 秒默认值在生产上普遍不够）。

---

## 12. eBPF 与 live patching：观测、拦截与内核级参考

### 12.1 Linux 内核 livepatch（参考模型）

- 官方 livepatch = kGraft 的 per-task 一致性 + kpatch 的栈检查混合体；补丁是内核模块，基于 **dynamic ftrace 在函数入口重定向**执行流。
- 严格限制：只能 patch 可被 ftrace 的函数（notrace 函数除外）；桩必须在函数最前（x86 需 `-fentry`）；与 kretprobe 互斥；一次只允许一个 transition；任务卡在受影响函数中可令转换**无限挂起**（需 fake signal 推进，force 后补丁永远无法卸载）；**数据结构布局变更不可补丁**——只适合加 NULL/边界检查、补 barrier/锁这类语义等价修复。
- 对用户态热更新的教益：入口重定向 + 显式一致性模型 + 保守的可补丁范围，是「函数级热补丁」可信的必要条件。

### 12.2 eBPF / uprobe 用户态路线

- 原生 uprobe：在用户态指令插 int3 陷入内核跑 BPF，两次上下文切换，**10–20 倍减速**、单点约 1–2μs，高频下 CPU 开销 >10%，perf buffer 满会静默丢事件。
- **bpftime**（用户态 eBPF 运行时）：内联 hook + 二进制改写直接在进程内替换函数入口为跳转，开销降低约 10 倍，可 attach 运行中进程，兼容 CO-RE/BTF 与共享内存 maps。
- 共性限制：JVM/V8 等 JIT/自修改代码上 hook 可能失效；**多插桩工具二次 hook 互踩**；x86 syscall 指令太短需 zepoline 技巧；需 CAP_BPF/root；**没有内核 livepatch 那样的转换一致性、补丁栈管理与安全卸载语义**。
- 定位：eBPF/uprobe 是优秀的**观测 / 拦截 / 参数与返回值改写**手段，能做「轻量 live patch」（紧急开关、灰度逻辑注入、临时修复探针），但不是通用用户态热补丁方案。

### 12.3 用户态 live patching 工具

- libpatch / Live++（商业，游戏/桌面 C++ 开发，编译后自动把新函数 patch 进运行进程，处理重定位与增量链接）等走「目标文件增量链接 + 入口跳转」路线，与 bpftime 同构但更偏开发期。
- gdb 注入 .so、`call` 执行 dlopen 是最朴素的应急补丁通道。
- 对 Rust：这些工具依赖 C ABI 符号与可预测重定位，对泛型/单态化代码基本不可用，仅作应急与观测层考虑，不进核心设计。

---

## 13. 场景选择矩阵

### 13.1 按变更类型

| 变更内容 | L1 函数级 | L2 模块级 | L3 进程级 | L4 环境级 |
|---|---|---|---|---|
| 改函数体逻辑 | 最佳 | 可以 | 可以 | 可以 |
| 改函数签名 | 否 | 否（需重启） | 可以 | 可以 |
| 改 struct 布局 | 否 | 序列化迁移 | 可以（状态外部化） | 可以 |
| 新增/升级依赖 | 否 | 否 | 可以 | 可以 |
| 升级 Rust 工具链 | 否 | 否（abi_stable 部分） | 可以 | 可以 |
| 迁移数据库 schema | 否 | 否 | 应用启动迁移 + 回滚预案 | 蓝绿 + 双写 |
| 紧急改一个判断 | uprobe/配置中心 | 可以 | 可以 | 过重 |

### 13.2 按使用场景

| 场景 | 推荐组合 |
|---|---|
| 本地开发迭代 Rust 服务 | watchexec + systemfd（保 socket）；热敏感逻辑 hot-lib-reloader |
| 本地开发 Bevy/游戏 | Subsecond 或 hot-lib-reloader + dylib |
| 单机/VM 生产升级 | systemd socket activation + fdstore + graceful shutdown |
| K8s 生产常规发布 | rollingUpdate(surge 1/unavail 0) + preStop + readiness + 优雅停机 |
| K8s + 网格 | 上述 + terminationDrainDuration 与 grace period 嵌套 |
| 高危发布 / 快速回滚 | 蓝绿或 Istio 权重金丝雀 |
| 业务规则频繁变 | Rhai/Rune 脚本 + AST 热换 |
| 紧急线上拦截/观测 | eBPF uprobe/bpftime（只读优先） |
| 长连接 / WebSocket / QUIC | fdstore 或 REUSEPORT + 会话状态外部化（Redis） |

### 13.3 跨语言经验收敛成的五条公理

1. **进程内热替换的能力与运行时复杂度成正比**：JVM/BEAM 能做，是因为有 GC、字节码身份、code server；AOT 静态语言做不到同等水平是结构性的，不是工程不够。
2. **状态迁移必须显式**：`code_change`、HotswapAgent 插件、序列化迁移——凡是框架「猜」的状态迁移都会在生产出事。
3. **依赖/底层变更最终都要落到进程级**：连 Erlang 升 ERTS 也要 restart_new_emulator。L3 是不可绕过的底座。
4. **drain 窗口必须显式且有上限**：默认 5s 在长请求场景必丢请求；窗口要大于最长请求、小于强杀期限。
5. **快是最便宜的热更新**：Go 的亚秒重编译、Vite 的 esbuild/Rolldown、Quarkus 的构建期增强——把构建做到足够快，重启就不再是痛点。打包服务首先要把构建与切换的端到端延迟压下去。

---

## 14. Rust 打包服务的热部署能力设计

结合全部调研，给出本服务应提供的能力分层、组件与关键流程。

### 14.1 总体分层：dev 热重载库 + 生产零停机二进制切换

```
┌──────────────────────────────────────────────────────────────┐
│ L4 环境层：蓝绿 / K8s rolling / Istio 切流（对接，不重复造）      │
├──────────────────────────────────────────────────────────────┤
│ L3 生产层：deploy-agent —— 零停机二进制切换（核心自研）          │
│   产物落盘 → 健康预热 → fd 交接/REUSEPORT → drain → 提交/回滚    │
├──────────────────────────────────────────────────────────────┤
│ L2 开发层：dev-reloader —— watch + dylib 热换 + socket 保活     │
│   watchexec/notify + hot-lib-reloader + systemfd 等价物         │
├──────────────────────────────────────────────────────────────┤
│ 构建层：sccache/mold/cranelift 分级 + 产物仓库 + 版本元数据       │
└──────────────────────────────────────────────────────────────┘
```

原则：**生产路径只做 L3（任意变更可发布、语义最简单、可审计可回滚）；L2 只服务本地开发，不进生产；L4 对接用户既有编排**。

### 14.2 组件清单

| 组件 | 职责 | 技术参考 / 可复用 |
|---|---|---|
| watcher | 文件变更监听、防抖、事件聚合 | watchexec 库 / notify；防抖默认 200-500ms |
| libreloader | dev 期 dylib 构建与热换、reload 事件 | hot-lib-reloader 0.8.x 直接集成或对齐其宏模型 |
| build orchestrator | 增量/缓存分级构建（dev cranelift，release mold+lto），产 dylib 或静态二进制 | sccache、cargo、toolchain pin |
| artifact store | 版本化产物（内容寻址）、元数据（git rev、依赖锁、迁移声明）、原子 rename 安装 | 对齐 SONAME 版本化纪律 |
| fd-passing supervisor | 生产侧持有监听 socket，新二进制从 LISTEN_FDS 接管；支持连接 fd 托管 | systemd socket activation+fdstore；无 systemd 时自管 SCM_RIGHTS（AF_UNIX 交接） |
| deploy-agent（sidecar） | 发布编排：下载→预热→健康检查→切流→drain→commit/rollback；状态机持久化 | Erlang release_handler 的状态机模型（permanent/transient + point_of_no_return） |
| drain controller | 优雅停机信号、在飞请求计数、排空窗口（默认建议 30s，可配） | SIGTERM + readiness 失败 + LB/Envoy 摘除联动 |
| dev socket keeper | 开发期占住端口、重启不 connection refused | systemfd/catflap 等价物 |
| control API / CLI | 用户面只暴露 deploy / rollback / status / releases | Shuttle 体验参照 |

### 14.3 生产部署状态机（deploy-agent 核心）

参考 release_handler 与蓝绿实践，单次发布的状态序列：

1. **Fetch**：拉取/校验内容寻址产物，原子安装到 `releases/<git-rev>`（不覆盖当前版本）。
2. **Preflight**：新二进制以预热模式启动（不接流量）：迁移检查、依赖/配置校验、连接池建立、自身 `/healthz:warm` 通过。失败 → 丢弃，旧版本毫发无伤。
3. **Arm**：supervisor 把监听 fd 传给新进程（socket activation / SCM_RIGHTS），或多实例下 REUSEPORT 加入监听。
4. **Attach**：新实例 readiness 通过 → 注册到 LB / Endpoints。
5. **Drain old**：旧实例先从 LB 摘除（等待传播），再收 SIGTERM，停止接新、排空在飞请求；超过 drain 窗口强杀并记告警。
6. **Commit**：新版本写为 permanent；保留上一版本 + 元数据用于一键 rollback（rollback 走同一状态机）。
7. 全程状态机持久化（agent 崩溃可恢复）；point_of_no_return 之前任何失败自动回退。

对应用的契约要求（写进接入文档）：支持从 LISTEN_FDS 接管监听、响应 SIGTERM 优雅停机、暴露 readiness/liveness、长请求可中断或可等待、内存态可从外部状态重建。

### 14.4 依赖变更与状态迁移的处理策略

- **依赖升级 / 工具链升级 / 泛型与布局变更**：全部走 L3 新进程——这正是选 L3 作为生产主路径的原因，变更类型不受限。
- **持久状态（DB schema）**：发布单声明迁移；迁移在 Preflight 之后、Attach 之前执行，要求向后兼容（expand/contract 模式：先加列/双写 → 切流 → 后清理），蓝绿场景强制双写兼容窗口。
- **内存态**：文档明确「不跨进程保证」；需要连续性的会话状态外部化到 Redis/DB；单机极端场景可用 fdstore + memfd 序列化接续（作为高级可选能力，不默认承诺）。

### 14.5 开发期形态

- `rs-dev up`：watcher 监听 → dev 构建走 cranelift/快速 profile → dev socket keeper 保端口 → 整进程重启为主路径（对齐 Go，亚秒级、零侵入）。
- 对标记热换的库（`crate-type rlib+dylib` + 宏）走 libreloader 做函数级热换，限制（签名/布局/泛型）在文档与宏编译期提示。
- 热换与整重启自动二选一：兼容变更热换，不兼容变更回落到整进程重启并在控制台说明原因。

### 14.6 与环境层的边界

- K8s 用户：deploy-agent 可作为 sidecar/init 形态或直接输出适配 rollingUpdate 的 Deployment；不替代 K8s 编排，只保证单实例切换不断连、drain 窗口与 `terminationGracePeriodSeconds` 对齐。
- 网格用户：自动生成/校验 `terminationDrainDuration < grace period` 的嵌套配置。
- VM/裸机用户：supervisor + socket activation 自成一体，不依赖外部 LB。

### 14.7 分期落地建议

| 阶段 | 能力 | 价值 |
|---|---|---|
| P1 | 版本化产物 + supervisor + socket activation + 基础 drain + CLI deploy/rollback | 单机零停机闭环 |
| P2 | deploy-agent 完整状态机、预热健康检查、K8s rolling 适配 | 生产可靠、可恢复 |
| P3 | dev-reloader（watchexec + 热换库 + socket keeper） | 开发体验 |
| P4 | fdstore 连接接续、网格 drain 联动、金丝雀权重对接、eBPF 观测注入 | 高级场景 |

---

## 15. 参考资料

### Java
- JEP 159 Enhanced Class Redefinition：https://openjdk.org/jeps/159
- JEP 451 Dynamic Loading of Agents：https://openjdk.org/jeps/451
- DCEVM：https://dcevm.github.io/
- JetBrains Runtime：https://github.com/JetBrains/JetBrainsRuntime
- HotswapAgent：https://github.com/HotswapProjects/HotswapAgent
- GraalVM Espresso HotSwap：https://docs.oracle.com/en/graalvm/jdk/21/docs/guides/java-hotswap/
- Spring Boot DevTools：https://docs.spring.io/spring-boot/reference/using/devtools.html
- Quarkus dev mode / class loading：https://quarkus.io/guides/dev-mode-differences ，https://quarkus.io/guides/class-loading-reference

### Erlang/OTP
- Release Handling：https://www.erlang.org/docs/28/system/release_handling.html
- Appup Cookbook：https://www.erlang.org/docs/27/system/appup_cookbook.html
- release_handler：https://www.erlang.org/docs/29/apps/sasl/release_handler.html

### JavaScript / TypeScript
- webpack HMR：https://webpack.js.org/concepts/hot-module-replacement/
- Vite HMR API：https://vite.dev/guide/api-hmr
- Vite Features（预构建）：https://vite.dev/guide/features

### Go / Python / C++
- Go plugin：https://pkg.go.dev/plugin
- air：https://github.com/air-verse/air
- Gunicorn signals：https://gunicorn.org/signals/
- Uvicorn settings/deployment：https://www.uvicorn.org/
- C++ hot reload 实践：https://pkglog.com/blog/cpp-series-55-7-hot-reload/
- Shared Library Hot Reloading：https://cristiandonosoc.github.io/en/posts/cpp/dll_hot_reloading_part1/

### Rust
- hot-lib-reloader：https://github.com/rksm/hot-lib-reloader-rs ，https://docs.rs/hot-lib-reloader/
- abi_stable：https://docs.rs/abi_stable/
- stabby：https://docs.rs/stabby/
- Rhai hot reload：https://rhai.rs/book/patterns/hot-reload.html
- cargo-watch（迁移建议）：https://docs.rs/crate/cargo-watch/latest
- watchexec：https://watchexec.github.io/
- systemfd：https://github.com/mitsuhiko/systemfd
- Shuttle：https://www.shuttle.dev/ ，https://docs.shuttle.dev/
- bevy_hotpatching_experiments：https://crates.io/crates/bevy_hotpatching_experiments
- dexterous_developer：https://lee-orr.github.io/dexterous_developer/

### 进程级 / 网格 / eBPF
- systemd fdstore：https://systemd.io/FILE_DESCRIPTOR_STORE/
- systemd.socket：https://www.freedesktop.org/software/systemd/man/latest/systemd.socket.html
- Kubernetes rollout：https://kubernetes.io/docs/concepts/workloads/controllers/deployment/
- Envoy graceful shutdown：https://gateway.envoyproxy.io/docs/tasks/operations/graceful-shutdown/
- Linux livepatch：https://docs.kernel.org/livepatch/livepatch.html
- bpftime：https://github.com/eunomia-bpf/bpftime ，论文 https://arxiv.org/abs/2311.07923
