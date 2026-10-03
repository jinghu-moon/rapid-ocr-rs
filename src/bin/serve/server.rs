//! serve 的运行期核心：共享状态、双队列调度、worker、有界结果存储与关闭（§4、§7.6、§8）。
//!
//! # 线程模型（§8.1 的逐条落地）
//!
//! ```text
//! main / accept loop（http.rs）  ：recv_timeout(200ms) + 关闭标志；退出用 Server::unblock()
//!   ├── 静态/校验类请求：就地处理（廉价）
//!   ├── POST /api/ocr：准入 → 有界读取 → 建任务 → 进双队列（满 → 503，**不阻塞**）
//!   └── POST /api/models/download：校验 → 建有界下载 channel（M2 的处理体在 download.rs）
//! serve-ocr（**恰好 1 个**，§8.2 引擎 &mut self）
//!   └── 从双队列取任务 → 推理 → 有界序列化 → 有界结果存储
//! serve-download（1 个，独立 channel，M2 接缝）
//! serve-sweeper（1 个）：JobStore 的 TTL/保留上限清理 + 结果/原图同步清理（§4.5）
//! ```
//!
//! 推理**绝不**在 accept 线程上执行：`POST /api/ocr` 只做准入、读体与入队。
//!
//! # 两把锁的边界
//!
//! - `jobs`（[`JobState`]）：任务存储、调度器、结果存储、待处理原图、ID 生成器。
//!   accept 线程与 worker 都只短暂持有它（μs 级：入队/出队/查询，**没有 I/O**）。
//! - `engine`：`Box<dyn OcrBackend>`。**只有** worker 长时间持有（整个推理期间），
//!   因此请求处理路径绝不碰它——`/api/status` 读的是 `engine_state` 这把独立的锁
//!   （状态机），不会因为一次推理而阻塞。
//!
//! 两把锁从不同时持有，因此没有锁序问题。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rapid_ocr_rs::{
    ALLOWED_DOWNLOAD_HOSTS, DetectionPolicy, DownloadError, FormulaPolicy, ImageInput, OcrOutput,
    OcrRequest, OrtRuntimeFingerprint, OutputPolicy, PreprocessPolicy, RapidOcrError,
    RecognitionPolicy, StagePlan, TextOrder, WordOutputMode, available_disk_bytes,
    ort_runtime_fingerprint, ort_runtime_version, peak_memory_source, peak_working_set_bytes,
    to_output_json,
};
use serde_json::{Value, json};

use super::download::{
    self, DOWNLOAD_QUEUE_CAPACITY, DownloadCommand, DownloadJob, DownloaderFactory,
};
use super::engine::{BackendProvider, EngineFactory, OcrBackend};
use super::error::{ErrorBody, ServeError};
use super::jobs::{
    CancelOutcome, DownloadProgress, JobFailure, JobIdGenerator, JobKind, JobQueue,
    JobState as JobLifecycle, JobStore, JobStoreLimits, Millis,
};
use super::limits::ServeLimits;
use super::model_plan::{ModelPlan, ModelReport, ModelSnapshot, PendingDownload, source_label};
use super::queue::{DualQueueScheduler, QueueClass, ScheduledJob, SchedulerConfig};
use super::results::{Outcome, ResultStore, SerializeError, serialize_bounded};
use super::security::{LocalOrigin, ServeToken};
use super::state::{
    EngineState, EngineStateMachine, OcrAdmission, ProviderStatus, ServeConfigPlan, ServiceState,
};

/// 空闲 worker 的等待上限（只为周期性检查关闭标志）。
const WORKER_POLL: Duration = Duration::from_millis(200);
/// TTL 清理线程的周期（§4.5：不依赖访问触发）。
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);
/// 单次读取的块大小（有界流式读取，§4.4 第 6 步）。
pub(super) const READ_CHUNK_BYTES: usize = 64 * 1024;
/// `/api/status` 与 `/api/models` 的模型目录一律脱敏（§7.4、§10.9）。
pub(super) const REDACTED_MODEL_DIR: &str = "<redacted>";

/// 进程内单调毫秒（`jobs::Millis` 的**唯一**口径：进程启动以来的毫秒数）。
pub(super) fn monotonic_ms() -> Millis {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as Millis
}

/// OCR 的队列路由。
///
/// M1 的默认是 [`Self::text_only`]（§10.8：公式路由默认关闭，页面也不发送任何开关）。
/// 公式路由要等 M4 才会接上真实来源；在此之前 `queue=formula` 的显式请求会被拒绝，
/// 而不是悄悄按文本处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OcrRouting {
    pub formula: bool,
}

impl OcrRouting {
    /// M1 生产路径的路由：全部进普通队列。
    pub const fn text_only() -> Self {
        Self { formula: false }
    }

    /// 查询参数里的 `queue` 取值 → 队列类别。
    pub fn class_for(self, requested: Option<&str>) -> Result<QueueClass, ServeError> {
        match requested {
            None | Some("text") => Ok(QueueClass::Text),
            Some("formula") if self.formula => Ok(QueueClass::Formula),
            // 公式路由没启用时**不**降级成文本：静默换队列会让 409/503 的判断失去意义。
            Some(_) => Err(ServeError::BadRequest),
        }
    }
}

/// 磁盘可用空间的来源（可注入，§6.5 的任务级预检）。
///
/// 与库内 `FreeSpaceProbe` 同一个思路：生产实现是库的 `available_disk_bytes`
/// （`GetDiskFreeSpaceExW`），测试注入固定值，于是"磁盘不足 → 507 + 两个数值"
/// 这条分支**不需要真的填满磁盘**就能验证。
pub(super) type FreeSpaceFactory = Arc<dyn Fn(&Path) -> Result<u64, RapidOcrError> + Send + Sync>;

/// 生产路径的探测：库的唯一实现。
pub(super) fn real_free_space() -> FreeSpaceFactory {
    Arc::new(|directory: &Path| available_disk_bytes(directory))
}

/// 一次"惰性创建/重建引擎"的结论（§7.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EngineLoad {
    /// 会话已建立（本次或此前）。
    Ready,
    /// 模型不齐备：状态机已刷新为 `blocked_models_missing` 并带上缺失清单。
    BlockedModelsMissing,
    /// 会话创建失败：状态机是 `failed`，原因在 [`EngineState::Failed`] 里。
    Failed,
}

/// 启动期校验后的全部运行期输入（由 `run.rs` 组装，测试也用它构造运行时）。
pub(super) struct ServeContext {
    pub limits: ServeLimits,
    pub plan: ServeConfigPlan,
    pub model_plan: ModelPlan,
    /// 启动期就绪快照（只算一次：逐文件校验会重新读盘并哈希）。
    pub snapshot: ModelSnapshot,
    pub token: ServeToken,
    pub local: LocalOrigin,
    pub page: String,
    pub nonce: String,
    pub allow_download: bool,
    /// `--allow-download-host` 的**显式**扩展（§6.1 第 3 条；库常量不被修改）。
    pub allow_download_hosts: Vec<String>,
    pub routing: OcrRouting,
    pub engine_factory: EngineFactory,
    pub downloader: DownloaderFactory,
    pub free_space: FreeSpaceFactory,
}

/// 待处理的原图字节（只有 worker 会取走它）。
struct PendingOcr {
    bytes: Vec<u8>,
    max_side: Option<u32>,
}

/// 任务存储 + 调度器 + 结果存储 + 待处理原图（同一把锁下的一个整体）。
///
/// 原图与任务**同锁**，因此"任务存在"与"原图还在"不可能分叉；之所以不把 `Vec<u8>` 放进
/// `JobStore`（M0c 的纯逻辑类型）：那会让 TTL/淘汰的单元测试也必须携带图片字节。
struct JobState {
    store: JobStore,
    scheduler: DualQueueScheduler,
    results: ResultStore,
    ids: JobIdGenerator,
    pending: HashMap<String, PendingOcr>,
}

impl JobState {
    /// 队列里每个任务的 `position` 以**调度器**为唯一事实来源（§4.2 的 `position`）。
    ///
    /// 不属于双队列的任务（下载）由 [`JobQueue::class`] 过滤掉：它们的 `position`
    /// 永远是 `null`，而不是"文本队列里的某个位置"。
    fn sync_positions(&mut self) {
        let queued: Vec<(String, QueueClass)> = self
            .store
            .views(monotonic_ms())
            .into_iter()
            .filter(|view| view.state == JobLifecycle::Queued)
            .filter_map(|view| view.queue.class().map(|class| (view.id, class)))
            .collect();
        for (id, class) in queued {
            let position = self.scheduler.position_of(class, &id);
            let _ = self.store.set_position(&id, position);
        }
    }

    /// 取出下一个要执行的任务并标记 `Running`。
    ///
    /// 取到但存储里已经没有它（被 TTL 淘汰）时跳过，而不是把它留在 `Queued`。
    fn take_next(&mut self) -> Option<ScheduledJob> {
        loop {
            let scheduled = self.scheduler.take_next()?;
            if self.store.start(&scheduled.id, monotonic_ms()).is_ok() {
                self.sync_positions();
                return Some(scheduled);
            }
        }
    }
}

/// 运行期的共享状态（`Arc` 之后才是 `Send + Sync`）。
pub(super) struct ServeShared {
    service: ServiceState,
    limits: ServeLimits,
    plan: ServeConfigPlan,
    model_plan: ModelPlan,
    token: ServeToken,
    local: LocalOrigin,
    page: String,
    nonce: String,
    allow_download: bool,
    allow_download_hosts: Vec<String>,
    routing: OcrRouting,
    engine_state: Mutex<EngineStateMachine>,
    engine: Mutex<Option<Box<dyn OcrBackend>>>,
    /// 引擎加载的互斥：`POST /api/ocr` 的惰性创建与 `POST /api/engine/reload` 只能有一个
    /// 在建立会话（`engine_state` 的锁**不**覆盖加载过程，否则 `/api/status` 在加载期间
    /// 会被阻塞、也看不到 `loading`）。
    engine_load: Mutex<()>,
    /// 上一次**真正建立会话**的耗时（毫秒）；`None` = 还没有建立过。
    engine_load_ms: Mutex<Option<u64>>,
    jobs: Mutex<JobState>,
    queue_signal: Condvar,
    download_tx: SyncSender<DownloadCommand>,
    engine_factory: EngineFactory,
    downloader: DownloaderFactory,
    free_space: FreeSpaceFactory,
    shutting_down: AtomicBool,
}

impl ServeShared {
    pub fn service_state(&self) -> ServiceState {
        self.service
    }

    pub fn engine_state(&self) -> EngineState {
        lock(&self.engine_state).state().clone()
    }

    pub fn provider_status(&self) -> ProviderStatus {
        self.engine_state()
            .provider_status(&self.plan.requested_label())
    }

    pub fn token(&self) -> &ServeToken {
        &self.token
    }

    pub fn local(&self) -> &LocalOrigin {
        &self.local
    }

    pub fn page(&self) -> &str {
        &self.page
    }

    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn limits(&self) -> ServeLimits {
        self.limits
    }

    /// 引擎配置里的 `min_side_len`（`?max_side=` 的下界：低于它会让预处理区间上下界颠倒）。
    pub fn min_side_len(&self) -> u32 {
        self.plan.engine.global.min_side_len as u32
    }

    /// `/api/models` 与 OCR 409 的**同一份**模型状态（§7.6 的"字段一致"）。
    pub fn models(&self) -> ModelReport {
        self.model_plan.report()
    }

    pub fn routing(&self) -> OcrRouting {
        self.routing
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// 请求关闭：worker 会在一轮之内退出。
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.queue_signal.notify_all();
    }

    /// 引擎状态名（启动日志与错误文案用）。
    pub fn engine_state_name(&self) -> &'static str {
        self.engine_state().name()
    }

    /// 上一次**真正建立会话**的耗时（毫秒，§11「M2：模型齐备后惰性创建 engine 并显示耗时」）。
    pub fn engine_load_ms(&self) -> Option<u64> {
        *lock(&self.engine_load_ms)
    }

    /// 引擎 `Failed` 的原因（进 503 的 `detail.reason`）。
    pub(super) fn engine_failure_reason(&self) -> Option<String> {
        match self.engine_state() {
            EngineState::Failed { reason } => Some(reason),
            _ => None,
        }
    }

    /// 与 `GET /api/models` **同源同值**的 409 载荷（HTTP 层与 OCR worker 共用，§7.6）。
    pub(super) fn models_missing_failure(&self) -> ErrorBody {
        let error = self.models_missing_error();
        ErrorBody {
            code: error.code(),
            message: error.message(),
            detail: self.models_missing_detail(),
        }
    }

    /// 模型不齐备时任务的终态载荷（`models_missing` / `models_corrupt`，两者都是 409）。
    pub(super) fn models_missing_outcome(&self) -> Outcome {
        Outcome::Failed(409, self.models_missing_failure())
    }

    /// `/api/status`（§4.2、§7.4、§7.5、§7.6）。
    ///
    /// 模型目录**只给脱敏形式**；ORT 指纹只给文件名 + 体积 + SHA-256（库给出的
    /// `LoadedModule.path` 是本机绝对路径，绝不能进响应）。
    pub fn status_json(&self) -> Value {
        let engine_state = self.engine_state();
        let provider = engine_state.provider_status(&self.plan.requested_label());
        let (queues, retention) = {
            let state = lock(&self.jobs);
            let scheduler = state.scheduler.config();
            // `round_len` / `served_in_round` / `is_empty` 是 §8.3 公平性的可观测口径：
            // 运维与测试都靠它们把"配额在生效"与"猜"区分开。
            let class_json = |class: QueueClass| {
                json!({
                    "used": state.scheduler.queued_len(class),
                    "capacity": scheduler.capacity(class),
                    "consecutive_quota": scheduler.consecutive_quota(class),
                    "served_in_round": state.scheduler.served_in_round(class),
                    "wait_bound": scheduler.wait_bound(class),
                })
            };
            (
                json!({
                    "text": class_json(QueueClass::Text),
                    "formula": class_json(QueueClass::Formula),
                    "round_len": scheduler.round_len(),
                    "idle": state.scheduler.is_empty(),
                }),
                json!({
                    "jobs": state.store.len(),
                    "terminal": state.store.terminal_count(),
                    "retained_bytes": state.store.retained_bytes(),
                    "tombstones": state.store.tombstone_len(),
                    "results": state.results.len(),
                    "result_bytes": state.results.bytes(),
                    "job_ttl_ms": state.store.limits().ttl_ms,
                }),
            )
        };
        json!({
            "state": self.service,
            "engine": engine_state,
            "provider": provider,
            "ort": {
                "version": ort_runtime_version(),
                "fingerprint": public_fingerprint(&ort_runtime_fingerprint()),
            },
            "memory": {
                "peak_working_set_bytes": peak_working_set_bytes(),
                "source": peak_memory_source(),
            },
            "queues": queues,
            "retention": retention,
            "limits": {
                "max_body_bytes": self.limits.max_body_bytes,
                "max_result_bytes": self.limits.max_result_bytes,
                "max_export_bytes": self.limits.max_export_bytes,
                "max_download_bytes": self.limits.max_download_bytes,
                "max_retained": self.limits.max_retained,
                "max_retained_bytes": self.limits.max_retained_bytes,
                "max_tombstones": self.limits.max_tombstones,
            },
            "model_dir": REDACTED_MODEL_DIR,
            "source": source_label(self.model_plan.source()),
            "downloads_allowed": self.allow_download,
            "download_hosts": self.effective_download_hosts(),
            "engine_load_ms": self.engine_load_ms(),
        })
    }

    /// 生效的可信 host 列表（编译期白名单 ∪ `--allow-download-host`，§6.1 第 3 条）。
    ///
    /// 只用于诊断与启动日志：**不是**把库常量改成扩展后的值——库每次下载都收到这份
    /// 显式参数（见 [`Self::download_job`]），常量本身一个字都没变。
    pub fn effective_download_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = ALLOWED_DOWNLOAD_HOSTS
            .iter()
            .map(|host| (*host).to_string())
            .collect();
        hosts.extend(self.allow_download_hosts.iter().cloned());
        hosts
    }

    /// `/api/models`（§5.4；`missing`/`corrupt`/`blocked` 与 OCR 409 的 `detail` 同源同序）。
    pub fn models_json(&self) -> Value {
        let report = self.models();
        json!({
            "model_dir": REDACTED_MODEL_DIR,
            "source": source_label(self.model_plan.source()),
            "downloads_allowed": self.allow_download,
            "complete": report.is_complete(),
            "missing": report.missing_names(),
            "corrupt": report.corrupt_names(),
            "blocked": report.blocking_names(),
            "sets": report.statuses().iter().map(set_status_json).collect::<Vec<_>>(),
        })
    }

    /// `/api/ocr` 的 409 `models_missing` / `models_corrupt` 的 `detail`。
    ///
    /// §7.6 要求"字段与 `/api/models` 一致"：这里**调用同一个** [`Self::models`] 与同一个
    /// 脱敏常量，因此 `missing` / `corrupt` / `blocked` / `source` / `model_dir` 与
    /// `/api/models` 是逐字节相同的值，而不是另算一遍的近似值。
    /// `blocked`（缺失 ∪ 损坏）同时是 `EngineState::BlockedModelsMissing.missing` 的内容。
    ///
    /// # 形状决策（M0c 接缝第 4 条的收口）
    ///
    /// §11.1 的通用错误体是三键 `{code, message, detail}`，而 §7.6 要求 409 里带缺失清单。
    /// 因此：`code` 仍然是 `models_missing` / `models_corrupt`（哪个取决于是否存在损坏文件），
    /// **清单进 `detail`**，并在 `detail` 里复用 `/api/models` 的字段名与值。
    pub fn models_missing_detail(&self) -> Value {
        let report = self.models();
        json!({
            "missing": report.missing_names(),
            "corrupt": report.corrupt_names(),
            "blocked": report.blocking_names(),
            "source": source_label(self.model_plan.source()),
            "model_dir": REDACTED_MODEL_DIR,
        })
    }

    /// 引擎 `BlockedModelsMissing` 时 `/api/ocr` 的 `code`：有损坏文件就是
    /// `models_corrupt`（§5.2 与 §11.1 要求两者可区分：损坏建议重新下载）。
    pub fn models_missing_error(&self) -> ServeError {
        let report = self.models();
        if report.corrupt_names().is_empty() {
            ServeError::ModelsMissing
        } else {
            ServeError::ModelsCorrupt
        }
    }

    /// 目标队列是否已满（§4.4 第 4 步：**读 body 之前**的判定）。
    pub fn queue_full(&self, class: QueueClass) -> bool {
        let state = lock(&self.jobs);
        state.scheduler.queued_len(class) >= state.scheduler.config().capacity(class)
    }

    /// `POST /api/ocr`：准入已经在 http 层完成，这里只建任务并入队（§4.4 第 7 步）。
    pub fn submit_ocr(
        &self,
        bytes: Vec<u8>,
        class: QueueClass,
        max_side: Option<u32>,
    ) -> Result<Value, ServeError> {
        // §7.6 + M2 的惰性创建：模型缺失/引擎不可用在这里拒绝（不建任务、不入队）。
        self.admit_ocr()?;

        let original_bytes = bytes.len() as u64;
        let mut state = lock(&self.jobs);
        let id = state.ids.generate();
        let position = state.scheduler.enqueue(class, id.clone())?;
        if let Err(error) = state.store.insert(
            id.clone(),
            JobKind::Ocr,
            JobQueue::from_class(class),
            original_bytes,
            monotonic_ms(),
        ) {
            state.scheduler.remove(class, &id);
            return Err(error);
        }
        // 原图与任务在**同一把锁**下登记，因此 worker 不可能看到"有任务但没原图"。
        state
            .pending
            .insert(id.clone(), PendingOcr { bytes, max_side });
        state.sync_positions();
        drop(state);
        self.queue_signal.notify_all();
        Ok(json!({
            "job_id": id,
            "kind": JobKind::Ocr.name(),
            "queue": class.name(),
            "position": position,
            "state": JobLifecycle::Queued.name(),
        }))
    }

    /// `POST /api/ocr` 的引擎准入（§7.6 第 3 步 + M2 的惰性创建）。
    ///
    /// - `Ready` → 执行；`Loading`/`Rebuilding` → 入队等待（**不拒绝**）；
    /// - `BlockedModelsMissing` → 先看磁盘上的**当前**事实：若模型已经齐备（典型场景是
    ///   刚下载完），转入 `Loading` 并把会话创建留给 worker（推理/建会话绝不在 accept
    ///   线程）；否则 409 + 与 `/api/models` 同源同值的缺失清单；
    /// - `Failed` → 503 `engine_unavailable`（原因在 `/api/status` 与 `detail.reason` 里）。
    fn admit_ocr(&self) -> Result<(), ServeError> {
        let admission = self.engine_state().ocr_admission();
        if matches!(admission, OcrAdmission::ModelsMissing { .. }) {
            // 只有这一种结论需要看磁盘上的当前事实（下载可能刚刚补齐模型）。
            return self.admit_with_models_on_disk();
        }
        // `Run`/`Queue` → 通过；`Unavailable` → 503（映射只有一份实现）。
        ServeError::from_ocr_admission(admission)
    }

    /// `BlockedModelsMissing` 下的准入：模型现在齐备就转入 `Loading`（惰性创建），否则 409。
    fn admit_with_models_on_disk(&self) -> Result<(), ServeError> {
        let blocking = self.models().blocking_names();
        if blocking.is_empty() {
            // 下载让它齐备了：进入 `Loading`（`BlockedModelsMissing → Loading` 是 §7.6 的
            // 合法转换），真正的会话由 worker 创建——accept 线程绝不建立会话。
            let mut machine = lock(&self.engine_state);
            let _ = machine.begin_loading();
            return Ok(());
        }
        {
            // 仍然缺失：刷新状态机里的清单（`Blocked → Blocked`），
            // 这样 `/api/status` 与 409 的 `detail` 不会各说一套。
            let mut machine = lock(&self.engine_state);
            let _ = machine.models_still_missing(blocking);
        }
        Err(self.models_missing_error())
    }

    /// `POST /api/models/download`（§4.2）。
    ///
    /// 请求体里**只有** `set_id`（§7.2 禁止 URL）。集合严格按 id 解析：未知 id → 404
    /// `model_set_not_found`（并列出已知集合），**没有** `sets[0]` 回落。
    /// 两处同步拒绝都给出**两个数值**（§6.2 的预算、§6.5 的磁盘空间）。
    pub fn submit_download(&self, set_id: &str) -> Result<Value, ServeError> {
        if !self.allow_download {
            return Err(ServeError::DownloadsDisabled);
        }
        if self.model_plan.set_by_id(set_id).is_none() {
            return Err(ServeError::ModelSetNotFound {
                set_id: set_id.to_string(),
                known: self.model_plan.set_ids(),
            });
        }
        let pending = self
            .models()
            .pending_download(set_id)
            .unwrap_or(PendingDownload {
                files: 0,
                bytes: Some(0),
            });
        let budget_bytes = self.limits.download_budget().total_bytes();
        // §6.2：已知总量超过 `--max-download-mb` → 在**发请求之前**拒绝。
        // 错误类型就是库的分类（413 `payload_too_large`，`detail` 里两个数值），
        // 因此同步拒绝与任务内的失败是同一套表示。
        if let Some(total) = pending.bytes
            && total > budget_bytes
        {
            return Err(ServeError::Download(DownloadError::TooLarge {
                limit_bytes: budget_bytes,
                observed_bytes: Some(total),
            }));
        }
        // §6.5：任务级磁盘核算。未知体积按 `--max-download-mb` 计入（宁可提前拒绝，
        // 也不要在写到一半时才发现空间不够）。
        let required = pending.bytes.unwrap_or(budget_bytes);
        let available = (self.free_space)(self.model_plan.model_dir())?;
        if available < required {
            return Err(ServeError::InsufficientDiskSpace {
                required_bytes: required,
                available_bytes: available,
            });
        }

        let id = {
            let mut state = lock(&self.jobs);
            let id = state.ids.generate();
            // 下载任务不属于双队列（§8.1 的独立 channel）：`JobQueue::Download` 是中性类别，
            // `position` 永远是 `null`（见 `JobQueue` 的文档）。
            state.store.insert(
                id.clone(),
                JobKind::ModelDownload,
                JobQueue::Download,
                0,
                monotonic_ms(),
            )?;
            state.store.set_download_progress(
                &id,
                DownloadProgress::planned(pending.files, pending.bytes),
            )?;
            id
        };
        match self.download_tx.try_send(DownloadCommand {
            job_id: id.clone(),
            set_id: set_id.to_string(),
        }) {
            Ok(()) => Ok(json!({
                "job_id": id,
                "kind": JobKind::ModelDownload.name(),
                "queue": JobQueue::Download.name(),
                "state": JobLifecycle::Queued.name(),
                "download": {
                    "files_done": 0,
                    "files_total": pending.files,
                    "bytes_done": 0,
                    "bytes_total": pending.bytes,
                    "current_file": Value::Null,
                },
            })),
            Err(error) => {
                // 有界 channel 满 → 与 OCR 队列满同语义（503 busy），并回收任务。
                {
                    let mut state = lock(&self.jobs);
                    let _ = state.store.cancel(&id, monotonic_ms());
                }
                Err(match error {
                    std::sync::mpsc::TrySendError::Full(_) => ServeError::Busy,
                    std::sync::mpsc::TrySendError::Disconnected(_) => ServeError::Internal,
                })
            }
        }
    }

    /// `GET /api/jobs/{id}`（§4.2 的字段全集）。
    pub fn job_view(&self, id: &str) -> Result<Value, ServeError> {
        let mut state = lock(&self.jobs);
        state.sync_positions();
        let view = state.store.view(id, monotonic_ms())?;
        Ok(serde_json::to_value(view).expect("JobView serialization cannot fail"))
    }

    /// `GET /api/jobs/{id}/result`。
    ///
    /// 成功 → 已序列化的结果字节；失败 → **重放原始状态码与错误体**
    /// （`422 unsupported_input` / `413 result_too_large` / `503 engine_unavailable`）；
    /// 未完成（含已取消）→ 409 `job_not_finished`，任务的确切状态由
    /// `GET /api/jobs/{id}` 如实给出。
    pub fn job_result(&self, id: &str) -> Result<Body, ServeError> {
        let state = lock(&self.jobs);
        let record = state.store.record(id)?;
        let outcome = state.results.get(id);
        match record.state {
            JobLifecycle::Succeeded => match outcome {
                Some(Outcome::Succeeded(bytes)) => Ok(Body::json(200, bytes.clone())),
                _ => Err(ServeError::Internal),
            },
            JobLifecycle::Failed => match outcome {
                Some(Outcome::Failed(status, body)) => {
                    Ok(Body::json(*status, render_error_body(body)))
                }
                _ => Err(ServeError::Internal),
            },
            JobLifecycle::Queued | JobLifecycle::Running | JobLifecycle::Cancelled => {
                Err(ServeError::JobNotFinished)
            }
        }
    }

    /// `POST /api/jobs/{id}/cancel`（§4.3 与 §6.6）。
    ///
    /// 排队中 → 立即取消（并从调度器/待处理原图里移除）；运行中的**下载** → 登记取消请求
    /// （状态仍是 `running`，视图里的 `cancel_requested` 为 `true`，worker 在文件边界兑现）；
    /// 运行中的 OCR 与终态 → 409 `not_cancellable`。
    pub fn cancel_job(&self, id: &str) -> Result<Value, ServeError> {
        let mut state = lock(&self.jobs);
        let outcome = state.store.cancel(id, monotonic_ms())?;
        if outcome == CancelOutcome::Cancelled {
            let queue = state.store.record(id)?.queue;
            if let Some(class) = queue.class() {
                state.scheduler.remove(class, id);
            }
            state.pending.remove(id);
        }
        state.sync_positions();
        let view = state.store.view(id, monotonic_ms())?;
        Ok(serde_json::to_value(view).expect("JobView serialization cannot fail"))
    }

    /// 下载 worker 用：`Queued → Running`（`Err` 表示任务已被取消，调用方必须忽略）。
    pub(super) fn begin_download(&self, id: &str) -> Result<(), ServeError> {
        lock(&self.jobs).store.start(id, monotonic_ms())
    }

    /// 关闭时放弃一条还没开始的下载命令（§4.3：排队中的任务取消是可靠的）。
    ///
    /// M1 在这里调的是 `fail`，但任务此刻是 `Queued` → `Running → Failed` 会被存储拒绝，
    /// 任务于是永远停在 `queued`。虽然进程马上退出、没人会看见，但那是"看起来对"而不是"对"。
    pub(super) fn abandon_download(&self, id: &str) {
        let mut state = lock(&self.jobs);
        let _ = state.store.cancel(id, monotonic_ms());
    }

    /// 下载 worker 用：`Running → Failed`，原因与分类都来自**同一个** `ServeError`
    /// （因此 507/413/502 的 `code` 与 `detail` 里的两个数值不会被压成字符串）。
    pub(super) fn finish_download_failed(&self, id: &str, error: ServeError) {
        let mut state = lock(&self.jobs);
        let _ = state
            .store
            .fail_classified(id, JobFailure::from(&error), monotonic_ms());
    }

    /// 下载 worker 用：`Running → Succeeded`（下载产物不是保留结果，因此结果字节为 0）。
    pub(super) fn finish_download_ok(&self, id: &str) {
        let mut state = lock(&self.jobs);
        let _ = state.store.succeed(id, 0, monotonic_ms());
    }

    /// 下载 worker 用：`Running → Cancelled`（§4.3 的 `running → cancelled` 边）。
    ///
    /// 库只在观察者**在文件边界返回 `false`** 时产生 `DownloadError::Cancelled`；万一出现
    /// "取消结论先到、请求登记后到"的竞态，这里把请求补登记，绝不让任务停在 `running`。
    pub(super) fn finish_download_cancelled(&self, id: &str) {
        let mut state = lock(&self.jobs);
        if !state.store.is_cancel_requested(id) {
            let _ = state.store.cancel(id, monotonic_ms());
        }
        let _ = state.store.finish_cancelled(id, monotonic_ms());
    }

    /// 下载 worker 用：登记/刷新进度（§4.3 的作业形状）。
    pub(super) fn set_download_progress(&self, id: &str, progress: DownloadProgress) {
        let mut state = lock(&self.jobs);
        let _ = state.store.set_download_progress(id, progress);
    }

    /// 下载 worker 用：是否已经收到取消请求（§6.6 的唯一检查点）。
    pub(super) fn is_cancel_requested(&self, id: &str) -> bool {
        lock(&self.jobs).store.is_cancel_requested(id)
    }

    /// 下载 worker 用：一次下载任务的全部输入。
    ///
    /// host 允许列表在这里**显式**拼出来（编译期白名单 ∪ `--allow-download-host`）：
    /// 传递给库的是参数，`ALLOWED_DOWNLOAD_HOSTS` 这个常量一个字都没变（§6.1 第 3 条）。
    pub(super) fn download_job(&self, set_id: &str) -> Option<DownloadJob<'_>> {
        let set = self.model_plan.set_by_id(set_id)?;
        let (connect_timeout, read_timeout) = download::default_timeouts();
        Some(DownloadJob {
            set,
            root: self.model_plan.model_dir(),
            budget_bytes: self.limits.max_download_bytes,
            connect_timeout,
            read_timeout,
            allowed_hosts: self.effective_download_hosts(),
        })
    }

    /// 下载 worker 用：某个集合待下载文件的规模（进度起点的唯一口径）。
    pub(super) fn pending_download(&self, set_id: &str) -> Option<PendingDownload> {
        self.models().pending_download(set_id)
    }

    /// 已知集合 id（未知 `set_id` 的错误里给出它，便于定位）。
    pub(super) fn known_set_ids(&self) -> Vec<String> {
        self.model_plan.set_ids()
    }

    /// OCR worker 用：取走待处理的原图。
    fn take_pending_ocr(&self, id: &str) -> Option<PendingOcr> {
        lock(&self.jobs).pending.remove(id)
    }

    /// OCR worker 用：登记终态载荷并结算任务状态。
    fn finish(&self, job_id: &str, outcome: Outcome) {
        let now = monotonic_ms();
        let mut state = lock(&self.jobs);
        let bytes = outcome.bytes();
        // 失败分类与 `/result` 上重放的是**同一份**状态码与错误体（不再压成一句文本）。
        let failure = match &outcome {
            Outcome::Failed(status, body) => Some(JobFailure::from_body(*status, body)),
            Outcome::Succeeded(_) => None,
        };
        if !state.results.insert(job_id, outcome) {
            let _ = state.store.fail(job_id, "the result store is full", now);
            return;
        }
        match failure {
            None => {
                let _ = state.store.succeed(job_id, bytes, now);
            }
            Some(failure) => {
                let _ = state.store.fail_classified(job_id, failure, now);
            }
        }
    }

    /// 惰性创建 / 显式重建引擎（§7.6）。
    ///
    /// 这就是 `EngineStateMachine::{begin_loading, models_still_missing}` 的**生产者**：
    ///
    /// - 模型齐备 → `Loading → Ready|Failed`（真实建立会话；`Loading` 期间 `engine_state`
    ///   的锁**不被持有**，因此 `/api/status` 能看到 `loading`，新 OCR 请求排队而不被拒绝）；
    /// - 模型仍缺失 → 保持 `blocked_models_missing` 并刷新缺失清单（`models_still_missing`）；
    /// - `force = false`（`POST /api/ocr` 的惰性路径）在已经 `Ready` 且引擎在场时立即返回，
    ///   `force = true`（`POST /api/engine/reload`）即使已 `Ready` 也重新建立会话
    ///   （用户的显式意图是"按磁盘上的当前文件重新加载"）。
    fn ensure_engine_loaded(&self, force: bool) -> EngineLoad {
        if !force
            && matches!(self.engine_state(), EngineState::Ready { .. })
            && lock(&self.engine).is_some()
        {
            return EngineLoad::Ready;
        }
        // 一次只允许一个加载者（惰性路径与 reload 可能同时到达）。
        let _loading = lock(&self.engine_load);
        if !force
            && matches!(self.engine_state(), EngineState::Ready { .. })
            && lock(&self.engine).is_some()
        {
            return EngineLoad::Ready;
        }

        let blocking = self.models().blocking_names();
        {
            let mut machine = lock(&self.engine_state);
            if matches!(machine.state(), EngineState::Loading) {
                // `submit_ocr` 的惰性准入已经把它推进了 `Loading`：不再转换，
                // 但如果在这之后文件消失了，必须如实报失败（`Loading → Failed`）。
                if !blocking.is_empty() {
                    let _ = machine.load_failed(missing_models_reason(&blocking));
                    return EngineLoad::Failed;
                }
            } else if blocking.is_empty() {
                if machine.begin_loading().is_err() {
                    return EngineLoad::Failed;
                }
            } else if matches!(machine.state(), EngineState::BlockedModelsMissing { .. }) {
                // 仍然缺失：刷新清单，状态保持 `blocked_models_missing`。
                let _ = machine.models_still_missing(blocking);
                return EngineLoad::BlockedModelsMissing;
            } else {
                // `Ready`/`Failed` 无法合法进入 `BlockedModelsMissing`（§7.6 的转换表）：
                // 如实报 `Failed` 并点名缺哪些文件，而不是模糊的"引擎不可用"。
                if machine.begin_loading().is_err() {
                    return EngineLoad::Failed;
                }
                let _ = machine.load_failed(missing_models_reason(&blocking));
                return EngineLoad::Failed;
            }
        }

        // 建立会话期间**不持有** `engine_state`（见方法文档）。
        let started = Instant::now();
        let mut config = self.plan.engine.clone();
        let result = match self.model_plan.pin_engine_paths(&mut config) {
            Ok(()) => (self.engine_factory)(&config),
            Err(error) => Err(RapidOcrError::ModelResolve(error.to_string())),
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut provider: Option<BackendProvider> = None;
        let mut reason: Option<String> = None;
        {
            let mut engine = lock(&self.engine);
            // 旧引擎先丢弃：`Failed` 状态下留着一个可用引擎只会让状态与事实不一致。
            *engine = None;
            match result {
                Ok(backend) => {
                    provider = Some(backend.provider());
                    *engine = Some(backend);
                }
                Err(error) => reason = Some(error.to_string()),
            }
        }
        *lock(&self.engine_load_ms) = Some(elapsed_ms);

        let mut machine = lock(&self.engine_state);
        match (provider, reason) {
            (Some(provider), _) => {
                let _ = machine.load_succeeded(
                    self.plan.requested_label(),
                    provider.selected_ep,
                    provider.fallback_to_cpu,
                );
                EngineLoad::Ready
            }
            (None, Some(reason)) => {
                let _ = machine.load_failed(reason);
                EngineLoad::Failed
            }
            (None, None) => {
                let _ = machine.load_failed("the engine factory returned no backend");
                EngineLoad::Failed
            }
        }
    }

    /// `POST /api/engine/reload`（§4.2、§7.6）。
    ///
    /// 响应体里的 `engine` 与 `/api/status` 的 `engine` **是同一个值**（冻结的
    /// `EngineState` 形状），因此"模型仍缺失"时客户端读到的是
    /// `{"state":"blocked_models_missing","missing":[…]}`——**不是**一句模糊的失败。
    /// `missing`/`corrupt` 与 `/api/models` 同源同值；`load_ms` 是**本次** reload 的墙钟耗时
    /// （`/api/status` 的 `engine_load_ms` 是上一次真正建立会话的耗时）。
    pub fn reload_engine(&self) -> Value {
        let started = Instant::now();
        let outcome = self.ensure_engine_loaded(true);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let report = self.models();
        let outcome_label = match outcome {
            EngineLoad::Ready => "ready",
            EngineLoad::BlockedModelsMissing => "blocked_models_missing",
            EngineLoad::Failed => "failed",
        };
        json!({
            "outcome": outcome_label,
            "engine": self.engine_state(),
            "missing": report.missing_names(),
            "corrupt": report.corrupt_names(),
            "source": source_label(self.model_plan.source()),
            "model_dir": REDACTED_MODEL_DIR,
            "load_ms": elapsed_ms,
        })
    }
}

/// 模型缺失/损坏时的引擎失败说明（点名文件，可定位）。
fn missing_models_reason(blocking: &[String]) -> String {
    format!(
        "the model files are missing or corrupt, so no session can be created: {}",
        blocking.join(", ")
    )
}

/// HTTP 层的响应体（状态码 + Content-Type + 字节）。
#[derive(Debug, Clone)]
pub(super) struct Body {
    pub status: u16,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

impl Body {
    pub fn json(status: u16, bytes: Vec<u8>) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            bytes,
        }
    }

    pub fn html(bytes: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            bytes,
        }
    }

    pub fn error(error: &ServeError) -> Self {
        Self::json(error.status_code(), error.render_body().into_bytes())
    }

    /// 带额外 `detail` 字段的错误响应（OCR 409 的"字段与 `/api/models` 一致"）。
    pub fn error_with_detail(error: &ServeError, extra: Value) -> Self {
        let mut body = error.body();
        let Value::Object(extra) = extra else {
            return Self::json(error.status_code(), render_error_body(&body));
        };
        let mut detail = match body.detail {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        for (key, value) in extra {
            detail.insert(key, value);
        }
        body.detail = Value::Object(detail);
        Self::json(error.status_code(), render_error_body(&body))
    }
}

/// 运行期句柄：共享状态 + worker 线程。
pub(super) struct ServeRuntime {
    shared: Arc<ServeShared>,
    workers: Vec<JoinHandle<()>>,
}

impl ServeRuntime {
    /// 建运行时：**先**完成引擎加载（`Loading → Ready|Failed`），再启动 worker。
    ///
    /// 调用方必须**已经**绑定成功（`ServiceState::Starting → Ready` 在这里发生，
    /// §7.6 第 1 步：监听成功即为 Ready）。
    pub fn new(context: ServeContext) -> std::io::Result<Self> {
        let service = ServiceState::Starting
            .listening_succeeded()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let (machine, backend) = load_engine(&context);
        let (download_tx, download_rx) = sync_channel(DOWNLOAD_QUEUE_CAPACITY);
        let shared = Arc::new(ServeShared {
            service,
            limits: context.limits,
            plan: context.plan,
            model_plan: context.model_plan,
            token: context.token,
            local: context.local,
            page: context.page,
            nonce: context.nonce,
            allow_download: context.allow_download,
            allow_download_hosts: context.allow_download_hosts,
            routing: context.routing,
            engine_state: Mutex::new(machine),
            engine: Mutex::new(backend),
            engine_load: Mutex::new(()),
            engine_load_ms: Mutex::new(None),
            jobs: Mutex::new(JobState {
                store: JobStore::new(JobStoreLimits::from_limits(&context.limits)),
                scheduler: DualQueueScheduler::new(SchedulerConfig::from_limits(&context.limits)),
                results: ResultStore::new(
                    context.limits.max_retained,
                    context.limits.max_retained_bytes,
                ),
                ids: JobIdGenerator::default(),
                pending: HashMap::new(),
            }),
            queue_signal: Condvar::new(),
            download_tx,
            engine_factory: context.engine_factory,
            downloader: context.downloader,
            free_space: context.free_space,
            shutting_down: AtomicBool::new(false),
        });

        let mut workers = Vec::with_capacity(3);
        {
            let runtime = Arc::clone(&shared);
            workers.push(spawn("serve-ocr", move || ocr_worker(runtime))?);
        }
        {
            let runtime = Arc::clone(&shared);
            let downloader = Arc::clone(&shared.downloader);
            workers.push(spawn("serve-download", move || {
                download::worker(runtime, download_rx, downloader)
            })?);
        }
        {
            let runtime = Arc::clone(&shared);
            workers.push(spawn("serve-sweeper", move || sweeper(runtime))?);
        }
        Ok(Self { shared, workers })
    }

    pub fn shared(&self) -> &Arc<ServeShared> {
        &self.shared
    }

    /// 停止 worker 并等待它们退出（幂等）。
    pub fn stop(&mut self) {
        self.shared.shutdown();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for ServeRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动期加载引擎（§7.6 第 3 步）：模型齐备 → `Loading → Ready|Failed`，
/// 否则 `BlockedModelsMissing`（模型缺失时**不**创建会话，服务照样 Ready）。
fn load_engine(context: &ServeContext) -> (EngineStateMachine, Option<Box<dyn OcrBackend>>) {
    // `EngineStateMachine::start(Complete)` 已经进入 `Loading`（模型齐备就是要预加载），
    // 因此这里**不**再调 `begin_loading`——它只用于从 `Blocked`/`Ready`/`Failed` 重新进入
    // `Loading`，也就是 M3 的 `POST /api/engine/reload`。
    let mut machine = EngineStateMachine::start(context.snapshot.readiness());
    if !context.snapshot.blocking_names().is_empty() {
        return (machine, None);
    }
    let mut engine_config = context.plan.engine.clone();
    if let Err(error) = context.model_plan.pin_engine_paths(&mut engine_config) {
        let _ = machine.load_failed(error.to_string());
        return (machine, None);
    }
    match (context.engine_factory)(&engine_config) {
        Ok(backend) => {
            let BackendProvider {
                selected_ep,
                fallback_to_cpu,
            } = backend.provider();
            let _ = machine.load_succeeded(
                context.plan.requested_label(),
                selected_ep,
                fallback_to_cpu,
            );
            (machine, Some(backend))
        }
        Err(error) => {
            // provider 不可用 / 会话创建失败：原因必须进 `/api/status`（§7.5/§7.6）。
            let _ = machine.load_failed(error.to_string());
            (machine, None)
        }
    }
}

/// OCR worker：**恰好一个**（引擎 `&mut self`，§8.2）。
///
/// 取出任务后先确保引擎在场（M2 的惰性创建：`POST /api/ocr` 只把状态推进 `Loading`，
/// 会话在这里建立，accept 线程绝不建立会话）。建不起来时任务以**同一份**错误映射失败
/// （409 `models_missing` + 与 `/api/models` 同源同值的清单 / 503 `engine_unavailable` + reason）。
fn ocr_worker(runtime: Arc<ServeShared>) {
    while !runtime.is_shutting_down() {
        let Some(scheduled) = next_scheduled(&runtime) else {
            continue;
        };
        let Some(pending) = runtime.take_pending_ocr(&scheduled.id) else {
            // 只有取消能走到这里；防御性记录，绝不把任务永久留在 `Running`。
            runtime.finish(
                &scheduled.id,
                failure(500, "internal", "the queued image is no longer available"),
            );
            continue;
        };
        let outcome = match runtime.ensure_engine_loaded(false) {
            EngineLoad::Ready => recognize(&runtime, &scheduled.id, pending),
            EngineLoad::BlockedModelsMissing => runtime.models_missing_outcome(),
            EngineLoad::Failed => {
                let reason = runtime
                    .engine_failure_reason()
                    .unwrap_or_else(|| "the OCR engine could not be created".to_string());
                failure(503, "engine_unavailable", reason)
            }
        };
        runtime.finish(&scheduled.id, outcome);
    }
}

/// 等一个任务（阻塞在条件变量上，最长 [`WORKER_POLL`]）。
fn next_scheduled(runtime: &ServeShared) -> Option<ScheduledJob> {
    let mut state = lock(&runtime.jobs);
    loop {
        if let Some(scheduled) = state.take_next() {
            return Some(scheduled);
        }
        if runtime.is_shutting_down() {
            return None;
        }
        let (guard, timeout) = runtime
            .queue_signal
            .wait_timeout(state, WORKER_POLL)
            .unwrap_or_else(|error| error.into_inner());
        state = guard;
        if timeout.timed_out() && runtime.is_shutting_down() {
            return None;
        }
    }
}

/// 一次识别：锁引擎 → 推理 → 有界序列化（§4.6）。
fn recognize(runtime: &ServeShared, job_id: &str, pending: PendingOcr) -> Outcome {
    let request = OcrRequest {
        input: ImageInput::Encoded(Arc::from(pending.bytes)),
        roi: None,
        scale_hint: None,
        stages: StagePlan::default(),
        preprocess: PreprocessPolicy {
            max_side: pending.max_side,
            ..PreprocessPolicy::default()
        },
        detection: DetectionPolicy::default(),
        recognition: RecognitionPolicy {
            words: WordOutputMode::Off,
        },
        output: OutputPolicy::default(),
        // §10.8：公式路由默认关闭（M4 才接线）。
        formula: FormulaPolicy::default(),
    };

    let output = {
        let mut engine = lock(&runtime.engine);
        let Some(backend) = engine.as_mut() else {
            return failure(
                503,
                "engine_unavailable",
                format!("the OCR engine is not loaded (job {job_id})"),
            );
        };
        match backend.recognize(request) {
            Ok(output) => output,
            Err(error) => {
                let serve_error = ServeError::from(error);
                return Outcome::Failed(serve_error.status_code(), serve_error.body());
            }
        }
    };
    serialize_output(&output, runtime.limits.max_result_bytes)
}

/// `OcrOutput` → 有界 JSON（§4.6：超限即中止，绝不先建大 `String`）。
///
/// 字段沿用库的 `to_output_json`（`regions` / `text` / `items` / `formulas` / `timings` / …），
/// 并**额外**给出 `plain_text`：内联页面（docs/05 §9 的冻结契约）的"复制全文"读的就是这个名字，
/// 值与 `text` 逐字节相同（同一个 `plain_text(TextOrder::Reading)`）。
fn serialize_output(output: &OcrOutput, limit: u64) -> Outcome {
    let mut value = match to_output_json(output) {
        Ok(value) => value,
        Err(error) => {
            let serve_error = ServeError::from(error);
            return Outcome::Failed(serve_error.status_code(), serve_error.body());
        }
    };
    if let Value::Object(object) = &mut value {
        object.insert(
            "plain_text".to_string(),
            Value::String(output.plain_text(TextOrder::Reading)),
        );
    }
    match serialize_bounded(&value, limit) {
        Ok(bytes) => Outcome::Succeeded(bytes),
        Err(SerializeError::TooLarge) => {
            let error = ServeError::ResultTooLarge;
            Outcome::Failed(error.status_code(), error.body())
        }
        Err(SerializeError::Internal(reason)) => failure(500, "internal", reason),
    }
}

fn failure(status: u16, code: &'static str, message: impl Into<String>) -> Outcome {
    Outcome::Failed(
        status,
        ErrorBody {
            code,
            message: message.into(),
            detail: Value::Null,
        },
    )
}

/// TTL 清理线程（§4.5：不依赖访问触发，也不依赖请求）。
fn sweeper(runtime: Arc<ServeShared>) {
    while !runtime.is_shutting_down() {
        thread::sleep(SWEEP_INTERVAL);
        let now = monotonic_ms();
        let mut state = lock(&runtime.jobs);
        let report = state.store.tick(now);
        if report.expired_jobs > 0
            || report.evicted_for_count > 0
            || report.evicted_for_bytes > 0
            || report.expired_tombstones > 0
        {
            eprintln!(
                "serve: retention tick expired_jobs={} evicted_for_count={} evicted_for_bytes={} \
                 expired_tombstones={}",
                report.expired_jobs,
                report.evicted_for_count,
                report.evicted_for_bytes,
                report.expired_tombstones
            );
        }
        // 结果与原图的清理与任务存储同步：先快照"还活着的任务"，避免在 `retain` 的
        // 闭包里再次借用同一个 guard。没有任何任务时直接跳过（不做无意义的分配）。
        if !state.store.is_empty() {
            let live: std::collections::HashSet<String> = state
                .store
                .views(now)
                .into_iter()
                .map(|view| view.id)
                .collect();
            let dropped_results = state.results.retain_only(|id| live.contains(id));
            let before = state.pending.len();
            state.pending.retain(|id, _| live.contains(id));
            let dropped_pending = before - state.pending.len();
            if dropped_results > 0 || dropped_pending > 0 {
                eprintln!(
                    "serve: dropped {dropped_results} result(s) and {dropped_pending} pending image(s)"
                );
            }
        }
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_string()).spawn(body)
}

/// 锁的获取：**中毒不传播**。serve 里任何一处 panic 都不该让进程再也回不了响应。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// `/api/status` 的 ORT 指纹（§7.4：只给文件名 + 体积 + SHA-256，绝不给绝对路径）。
fn public_fingerprint(fingerprint: &OrtRuntimeFingerprint) -> Value {
    let module = &fingerprint.runtime_module;
    json!({
        "file": file_name(&module.path),
        "size_bytes": module.size_bytes,
        "sha256": module.sha256,
        "source": fingerprint.runtime_source,
        "complete": fingerprint.is_complete(),
        "providers": fingerprint.provider_dlls.iter().map(|dll| json!({
            "name": dll.name,
            "size_bytes": dll.size_bytes,
            "loaded": dll.loaded,
        })).collect::<Vec<_>>(),
    })
}

/// 只取路径的最后一段（路径脱敏的唯一实现）。
fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// `/api/models` 的单个集合（§5.4 的扁平形状：`state` 是字符串；`corrupt` 时额外给出
/// `expected`/`actual`。库里 `ModelFileState` 的枚举序列化形状不进协议）。
fn set_status_json(status: &rapid_ocr_rs::ModelSetStatus) -> Value {
    json!({
        "id": status.set_id,
        "complete": status.complete,
        "download_bytes_total": status.download_bytes_total,
        "files": status.files.iter().map(|(file, state)| {
            let mut value = json!({
                "name": file.name,
                "role": file.role.as_str(),
                "state": state.as_str(),
                "size_bytes": file.size_bytes,
                "sha256": if file.has_hash() { Value::String(file.sha256.clone()) } else { Value::Null },
                "source_url": if file.has_source_url() { Value::String(file.source_url.clone()) } else { Value::Null },
            });
            if let rapid_ocr_rs::ModelFileState::Corrupt { expected, actual } = state
                && let Value::Object(object) = &mut value
            {
                object.insert("expected".to_string(), Value::String(expected.clone()));
                object.insert("actual".to_string(), Value::String(actual.clone()));
            }
            value
        }).collect::<Vec<_>>(),
    })
}

fn render_error_body(body: &ErrorBody) -> Vec<u8> {
    serde_json::to_vec(body).expect("ErrorBody serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{OcrRouting, REDACTED_MODEL_DIR, monotonic_ms, public_fingerprint};
    use crate::serve::error::ServeError;
    use crate::serve::queue::QueueClass;
    use crate::serve::state::{EngineStateMachine, ModelReadiness};

    #[test]
    fn the_clock_is_monotonic_and_the_redaction_is_a_constant() {
        let first = monotonic_ms();
        let second = monotonic_ms();
        assert!(second >= first, "{first} -> {second}");
        assert_eq!(REDACTED_MODEL_DIR, "<redacted>");
    }

    #[test]
    fn the_m1_routing_refuses_to_silently_use_the_formula_queue() {
        assert_eq!(
            OcrRouting::text_only().class_for(None).expect("text"),
            QueueClass::Text
        );
        assert_eq!(
            OcrRouting::text_only()
                .class_for(Some("text"))
                .expect("text"),
            QueueClass::Text
        );
        assert!(matches!(
            OcrRouting::text_only().class_for(Some("formula")),
            Err(ServeError::BadRequest)
        ));
        assert!(matches!(
            OcrRouting::text_only().class_for(Some("bogus")),
            Err(ServeError::BadRequest)
        ));
        let enabled = OcrRouting { formula: true };
        assert_eq!(
            enabled.class_for(Some("formula")).expect("formula"),
            QueueClass::Formula
        );
    }

    /// 指纹里的绝对路径绝不进响应：只留文件名。
    #[test]
    fn the_public_fingerprint_never_carries_a_directory() {
        let fingerprint = rapid_ocr_rs::ort_runtime_fingerprint();
        let public = public_fingerprint(&fingerprint);
        let text = public.to_string();
        let raw = &fingerprint.runtime_module.path;
        if let Some(parent) = Path::new(raw).parent()
            && !parent.as_os_str().is_empty()
        {
            let directory = parent.to_string_lossy().into_owned();
            assert!(
                !text.contains(&directory),
                "the module directory must not appear: {text}"
            );
        }
        assert!(public.get("file").is_some());
    }

    /// 缺失与损坏都进 `BlockedModelsMissing` 的清单（引擎对两者都不可用）。
    #[test]
    fn readiness_treats_missing_and_corrupt_the_same_way() {
        let machine = EngineStateMachine::start(ModelReadiness::Incomplete {
            missing: vec!["a.onnx".to_string(), "b.onnx".to_string()],
        });
        assert_eq!(machine.state().name(), "blocked_models_missing");
    }
}
