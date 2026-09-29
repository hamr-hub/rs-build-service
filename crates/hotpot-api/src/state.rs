//! HTTP 层共享状态。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::hardening::LoadShedder;
use hotpot_cacheproto::RemoteCache;
use hotpot_core::BuildId;
use hotpot_core::model::BuildEvent;
use hotpot_scheduler::Scheduler;
use hotpot_store::LocalStore;
use hotpot_worker::ExecutorKind;
use tokio::sync::{Mutex, broadcast, watch};

/// 应用共享状态。
#[derive(Clone)]
pub struct AppState {
    pub scheduler: Scheduler,
    pub store: Arc<LocalStore>,
    /// 远程缓存（`/metrics` 统计命中率用；未挂载协议层时为 None）。
    pub cache: Option<RemoteCache>,
    /// 执行后端（供 `/metrics` 与 `/v1/toolchains` 标注）。
    pub executor: Arc<ExecutorKind>,
    /// 是否接受 git 来源构建。默认 false：git 来源会执行不可信仓库里的
    /// `build.rs`/proc-macro，等价于允许在服务进程权限下执行任意代码。
    pub allow_git_source: bool,
    /// 内嵌 worker 数（`/metrics` 暴露）。
    pub workers: usize,
    /// 资源回收器的累计计数（`/metrics` 暴露）。
    pub gc_stats: Arc<crate::gc::GcStats>,
    /// 构建默认使用的工具链描述（`/metrics` 的 `hotpot_info` 标签）。
    pub default_toolchain: String,
    /// 取消信号状态（通道 + 已登记但未生效的取消意图）。
    cancels: Arc<Mutex<CancelTable>>,
    /// 构建事件广播（见 [`StreamSignal`]）。
    stream: broadcast::Sender<StreamSignal>,
    /// 并发限流器。
    ///
    /// 放在 `AppState` 上而不是只存在于中间件闭包里，是为了让 `/metrics`
    /// 能读出当前占用：限流在生产上表现为"莫名其妙的一批 503"，没有这个
    /// 指标就只能靠猜。`hotpot_concurrency_in_flight` 长期贴着
    /// `hotpot_concurrency_limit` 就说明容量该调了。
    pub shedder: LoadShedder,
}

/// 推送给 SSE 订阅者的信号。
///
/// 存在的理由是**并发**：SSE 曾经靠"每个连接每 400ms 轮询一次数据库"
/// 推进。连接数一多，查询量就随连接数线性增长（N 个看日志的人 = 每秒
/// 2.5N 次查询），而且日志还有最多 400ms 的延迟。改成广播后：
///
/// - 正常路径**零查询**——事件由 worker 写库后直接推给订阅者；
/// - 延迟从"最多 400ms"降到"亚毫秒"；
/// - 数据库压力与观看人数**解耦**。
///
/// 数据库仍然是唯一事实来源：订阅只是"有新东西了，去读"的信号，
/// 断线重连靠 `since` 游标从库里补齐，不依赖广播的完整性。
#[derive(Clone, Debug)]
pub enum StreamSignal {
    /// 有新日志事件。
    Log(BuildEvent),
    /// 构建状态发生变化（认领、开始、结束、取消）。
    Status(BuildId),
}

/// 广播通道容量。
///
/// 慢订阅者（网络卡住的浏览器）会被丢包（`RecvError::Lagged`），此时
/// 订阅者从库里按游标全量补齐即可——广播只负责"有变化"的提示，不负责
/// 可靠投递，所以容量不需要大到能缓存整个构建的日志。
const STREAM_CHANNEL_CAPACITY: usize = 1024;

/// 并发上限的默认值；实际值以 `--max-concurrent-requests` 为准。
pub const DEFAULT_CONCURRENCY_LIMIT: usize = 256;

/// 取消信号表。
///
/// 拆成「已注册通道」与「待生效意图」两张表，是因为这两件事之间存在一段
/// 真实窗口：worker 认领构建（状态置 `dispatched`）之后，要先拉源码、
/// 准备会话目录，**才**注册取消通道；这段时间可能是几十分钟的 git clone。
/// 窗口内到达的取消请求如果找不到通道就直接丢弃，接口仍返回 `200 OK`
/// 与原样的 `dispatched` 记录——用户以为取消成功，构建却跑完了。
/// 这是最难排查的一类失败：看起来成功了，实际没生效。
///
/// 所以「取消意图」先记下来，等通道注册时立即补发。执行器收到的
/// `watch::Receiver` 初始值就是 `true`，取消在它开始执行的第一瞬间生效。
#[derive(Default)]
struct CancelTable {
    /// 已注册通道的构建。
    live: HashMap<BuildId, watch::Sender<bool>>,
    /// 已请求取消、但通道尚未注册的构建。
    pending: HashSet<BuildId>,
}

impl AppState {
    pub fn new(scheduler: Scheduler, store: LocalStore) -> Self {
        Self {
            scheduler,
            store: Arc::new(store),
            cache: None,
            executor: Arc::new(ExecutorKind::default()),
            allow_git_source: false,
            workers: 0,
            gc_stats: Arc::new(crate::gc::GcStats::default()),
            default_toolchain: "default".to_string(),
            cancels: Arc::new(Mutex::new(CancelTable::default())),
            stream: broadcast::channel(STREAM_CHANNEL_CAPACITY).0,
            shedder: LoadShedder::new(DEFAULT_CONCURRENCY_LIMIT),
        }
    }

    /// 挂载远程缓存（协议层与指标共享同一实例）。
    pub fn with_cache(mut self, cache: RemoteCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// 记录执行后端。
    pub fn with_executor(mut self, executor: ExecutorKind) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    /// 允许 git 来源构建（安全敏感，调用方需显式开启）。
    pub fn with_git_source(mut self, allow: bool) -> Self {
        self.allow_git_source = allow;
        self
    }

    /// 记录 worker 数与默认工具链（供 `/metrics` 标注）。
    pub fn with_runtime_info(
        mut self,
        workers: usize,
        default_toolchain: impl Into<String>,
    ) -> Self {
        self.workers = workers;
        self.default_toolchain = default_toolchain.into();
        self
    }

    /// 设置并发上限（须在 `load_shed_middleware` 读取之前调用）。
    pub fn with_concurrency_limit(mut self, limit: usize) -> Self {
        self.shedder = LoadShedder::new(limit);
        self
    }

    /// 广播一条流信号。
    ///
    /// 没有订阅者时 `Err` 是正常情况（没人看日志），直接忽略。
    pub fn publish(&self, signal: StreamSignal) {
        let _ = self.stream.send(signal);
    }

    /// 订阅流信号。
    pub fn subscribe(&self) -> broadcast::Receiver<StreamSignal> {
        self.stream.subscribe()
    }

    /// 为构建注册取消通道，返回接收端。
    pub async fn register_cancel(&self, id: BuildId) -> watch::Receiver<bool> {
        self.cancels.lock().await.register(id)
    }

    /// 移除取消通道，并清掉可能残留的取消意图。
    pub async fn remove_cancel(&self, id: BuildId) {
        self.cancels.lock().await.remove(id);
    }

    /// 请求取消构建。
    pub async fn signal_cancel(&self, id: BuildId) -> bool {
        self.cancels.lock().await.signal(id)
    }
}

impl CancelTable {
    /// 注册通道；若此前已登记取消意图，立即补发。
    fn register(&mut self, id: BuildId) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        if self.pending.remove(&id) {
            // 接收端 `rx` 就在手上，发送必然有接收者。
            let _ = tx.send(true);
        }
        self.live.insert(id, tx);
        rx
    }

    /// 移除通道并清掉待生效意图。
    fn remove(&mut self, id: BuildId) {
        self.live.remove(&id);
        self.pending.remove(&id);
    }

    /// 请求取消。返回是否被接受。
    fn signal(&mut self, id: BuildId) -> bool {
        match self.live.get(&id) {
            Some(tx) => tx.send(true).is_ok(),
            // 通道还没注册（认领与注册之间的窗口）：记住意图。
            None => {
                self.pending.insert(id);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> CancelTable {
        CancelTable::default()
    }

    /// 这是本模块最关键的不变量：取消请求绝不能因为「来得早了一点」而丢失。
    ///
    /// 背景：worker 认领构建（状态置 `dispatched`）后要先拉源码才注册通道，
    /// git 场景下这段窗口可能有几分钟。窗口内的取消如果直接丢弃，接口仍会
    /// 返回 200 与原样的 `dispatched` 记录——用户以为取消成功，构建却跑完了。
    #[test]
    fn cancel_before_register_is_not_lost() {
        let mut t = table();
        let id = BuildId::new();

        assert!(t.signal(id), "取消应当被接受");
        let mut rx = t.register(id);
        assert!(*rx.borrow_and_update(), "注册通道时必须补发取消意图");
    }

    #[test]
    fn cancel_after_register_is_delivered_live() {
        let mut t = table();
        let id = BuildId::new();

        let mut rx = t.register(id);
        assert!(!*rx.borrow_and_update(), "初始应为未取消");

        assert!(t.signal(id));
        assert!(*rx.borrow_and_update(), "取消应送达运行中的构建");
    }

    #[test]
    fn register_without_cancel_stays_false() {
        let mut t = table();
        let mut rx = t.register(BuildId::new());
        assert!(!*rx.borrow_and_update(), "没有取消请求时初值必须是 false");
    }

    /// id 复用时（测试里直接复用，生产中不会），历史取消不能牵连新构建。
    #[test]
    fn remove_clears_pending_intent() {
        let mut t = table();
        let id = BuildId::new();
        assert!(t.signal(id));
        t.remove(id);

        let mut rx = t.register(id);
        assert!(
            !*rx.borrow_and_update(),
            "remove 应清掉待生效意图，否则新构建一出生就被取消"
        );
    }

    /// 多次取消是幂等的：重复点击取消不应产生额外效果，也不应 panic。
    #[test]
    fn repeated_cancel_is_idempotent() {
        let mut t = table();
        let id = BuildId::new();
        assert!(t.signal(id));
        assert!(t.signal(id));
        let mut rx = t.register(id);
        assert!(*rx.borrow_and_update());
    }
}
