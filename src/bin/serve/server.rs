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
    ALLOWED_DOWNLOAD_HOSTS, DetectionPolicy, DownloadError, FormulaPolicy, ImageInput,
    ModelFileSpec, ModelFileState, ModelRole, OcrOutput, OcrRequest, OrtRuntimeFingerprint,
    OutputPolicy, PreprocessPolicy, ProviderPreference, RapidOcrError, RecognitionPolicy,
    StagePlan, WordOutputMode, available_disk_bytes, ort_runtime_fingerprint, ort_runtime_version,
    peak_memory_source, peak_working_set_bytes, verification_stats,
};
use serde_json::{Value, json};

use super::admit::QueueReservation;
use super::download::{
    self, DOWNLOAD_QUEUE_CAPACITY, DownloadCommand, DownloadJob, DownloaderFactory,
};
use super::engine::{BackendProvider, EngineFactory, OcrBackend};
use super::error::{ErrorBody, ServeError};
use super::evaluate::EvalRoot;
use super::export::{self, ExportError, ExportFormat, ExportRequest};
use super::jobs::{
    CancelOutcome, DownloadProgress, JobFailure, JobIdGenerator, JobKind, JobQueue,
    JobState as JobLifecycle, JobStore, JobStoreLimits, Millis,
};
use super::limits::ServeLimits;
use super::model_plan::{
    FormulaDetectorSpec, ModelPlan, ModelReport, ModelSnapshot, PendingDownload, source_label,
};
use super::queue::{DualQueueScheduler, QueueClass, ScheduledJob, SchedulerConfig};
use super::results::{Outcome, ResultStore, SerializeError, Succeeded, serialize_bounded};
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

/// OCR 的队列路由（§8.1、§10.8）。
///
/// M4 起公式路由是**真实**的：`queue=formula` 会被路由到公式管线并在公式队列里排队。
/// 判据只有一条（[`routing_for`]）：服务端必须有一个公式**检测**模型
/// （`--formula-detector`，或模型集里声明的 `formula_detector` role）。
///
/// 为什么没有检测模型就**不**打开路由，而不是"接了但永远没有公式区域"：`FormulaPolicy`
/// 在没有 `detector_path` 时只处理调用方显式声明的区域（`input_regions`），而 HTTP 请求里
/// 没有这种区域，于是公式队列会稳定地返回零个公式区域——一个"看起来成功但没有做任何事"
/// 的路径。宁可明确拒绝（400，理由写在 `disabled_reason` 里，页面据此禁用开关），
/// 也不提供一条静默无效的路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OcrRouting {
    pub formula: bool,
    /// 公式路由不可用时给用户的**文字**理由（进 `/api/models` 的 `formula.disabled_reason`
    /// 与 `/api/status`；页面把它显示在公式开关旁，不只靠颜色/禁用态）。
    pub disabled_reason: Option<String>,
}

impl OcrRouting {
    /// 公式路由可用（检测模型已经在场）。
    pub const fn formula_enabled() -> Self {
        Self {
            formula: true,
            disabled_reason: None,
        }
    }

    /// 公式路由不可用，并给出可定位理由。
    pub fn text_only(reason: impl Into<String>) -> Self {
        Self {
            formula: false,
            disabled_reason: Some(reason.into()),
        }
    }

    /// 查询参数里的 `queue` 取值 → 队列类别。
    pub fn class_for(&self, requested: Option<&str>) -> Result<QueueClass, ServeError> {
        match requested {
            None | Some("text") => Ok(QueueClass::Text),
            Some("formula") if self.formula => Ok(QueueClass::Formula),
            // 公式路由没启用时**不**降级成文本：静默换队列会让 409/503 的判断失去意义。
            Some(_) => Err(ServeError::BadRequest),
        }
    }
}

/// 启动期决定公式路由是否可用（**唯一**判据）。
///
/// 参数是已经解析好的检测模型（`--formula-detector` 优先，其次是模型集里声明的
/// `formula_detector` role，见 `run.rs`）。理由文案里点名用户真正要做的动作。
pub(super) fn routing_for(detector: Option<&Path>) -> OcrRouting {
    match detector {
        Some(_) => OcrRouting::formula_enabled(),
        None => OcrRouting::text_only(
            "formula routing is not enabled on this server: no page formula detector is \
             configured, and without one the formula pipeline can only handle caller-declared \
             regions (there are none over HTTP). Pass --formula-detector <ONNX> (for example \
             pix2text-mfd-1.5.onnx) to enable the formula queue; ordinary OCR is unaffected \
             (docs/05 §4.2, §10.8)",
        ),
    }
}

/// 配置好的公式检测模型在磁盘上的状态（评审 P1-1：检测模型与识别模型同一条完整性规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormulaDetectorStatus {
    /// 文件名（§7.4 脱敏；绝不把本机绝对路径写进响应）。
    pub name: String,
    pub state: ModelFileState,
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
    /// `--allow-provider-fallback`（§7.5）。运行期切换 provider 时**必须**沿用同一个开关，
    /// 否则同一条冻结规则会有两种解释（见 [`ServeShared::switch_provider`]）。
    pub allow_provider_fallback: bool,
    pub routing: OcrRouting,
    /// 页面公式检测模型（M4）：`--formula-detector` 优先，其次是模型集里声明的
    /// `formula_detector` role。`None` = 公式路由不可用（见 [`OcrRouting`]）。
    ///
    /// **哈希跟着路径走**（评审 P1-1）：集合声明过这个文件时它带着声明的 SHA-256，
    /// 库在加载检测器时校验，与识别模型完全对称。
    pub formula_detector: Option<FormulaDetectorSpec>,
    /// `--eval-root` 沙箱（M1 评审 P2-3）：`None` = `/api/evaluate` 整体关闭。
    pub eval_root: Option<EvalRoot>,
    pub engine_factory: EngineFactory,
    pub downloader: DownloaderFactory,
    pub free_space: FreeSpaceFactory,
}

/// 保留的**原图编码字节**（§4.5：只保留编码字节，绝不保留解码结果）。
///
/// 识别前它是待处理的输入，识别成功后它继续留在保留区里，供
/// `/api/jobs/{id}/annotated.png` **按需**重新解码；被保留预算释放（或任务失败/取消）时
/// 条目被移除，该端点随之变成 410 `original_evicted`。
///
/// 字节用 `Arc<[u8]>` 持有：worker 与 HTTP 线程都只克隆这个引用，不复制图片。
struct RetainedOcr {
    bytes: Arc<[u8]>,
    max_side: Option<u32>,
}

/// 任务存储 + 调度器 + 结果存储 + 保留的原图（同一把锁下的一个整体）。
///
/// 原图与任务**同锁**，因此"任务存在"与"原图还在"不可能分叉；之所以不把 `Arc<[u8]>`
/// 放进 `JobStore`（M0c 的纯逻辑类型）：那会让 TTL/淘汰的单元测试也必须携带图片字节。
struct JobState {
    store: JobStore,
    scheduler: DualQueueScheduler,
    results: ResultStore,
    ids: JobIdGenerator,
    /// 仍在保留区里的原图编码字节（§4.5）。任务被淘汰 / 原图被释放时同步移除。
    originals: HashMap<String, RetainedOcr>,
    /// **已经预留、尚未提交**的队列槽位（§4.4 第 4 步，评审 P2-1）。
    ///
    /// 与调度器在**同一把锁**下：容量判定因此是"已排队 + 已预留 >= 容量"，
    /// 两个并发请求不可能都通过检查（先检查后入队的两个临界区正是那个缺口）。
    reserved_text: usize,
    reserved_formula: usize,
}

impl JobState {
    /// 原子地预留一个槽位：容量判定与占位在同一次加锁里完成。
    ///
    /// 预留会让队列"看起来更满"（保守方向），因此提交顺序必须是**先入队、后释放预留**：
    /// 多算的那一格只会让并发请求被保守拒绝，绝不超卖容量。
    fn reserve(&mut self, class: QueueClass) -> bool {
        let used = self.scheduler.queued_len(class) + self.reserved(class);
        if used >= self.scheduler.config().capacity(class) {
            return false;
        }
        *self.reserved_mut(class) += 1;
        true
    }

    /// 归还一个预留（请求被拒绝、读 body 失败、连接中止、panic 展开都会走到这里）。
    fn release(&mut self, class: QueueClass) {
        let slot = self.reserved_mut(class);
        // 只有提交路径会消耗预留；这里仍然用 `saturating_sub`，因为一次"多释放"
        // 绝不能让容量凭空变大（宁可少一格，也不能超卖）。
        *slot = slot.saturating_sub(1);
    }

    fn reserved(&self, class: QueueClass) -> usize {
        match class {
            QueueClass::Text => self.reserved_text,
            QueueClass::Formula => self.reserved_formula,
        }
    }

    fn reserved_mut(&mut self, class: QueueClass) -> &mut usize {
        match class {
            QueueClass::Text => &mut self.reserved_text,
            QueueClass::Formula => &mut self.reserved_formula,
        }
    }

    /// 把 `JobStore` 刚释放掉原图的任务从保留区里真正丢弃（§4.5）。
    ///
    /// `JobStore` 负责**记账**（`retained_bytes`、`original_retained`），字节本身在这里；
    /// 两者必须在同一个锁里收敛，否则 `/api/status` 的 `retained_bytes` 会与真实占用分叉。
    fn sync_originals(&mut self) {
        for id in self.store.take_released_originals() {
            self.originals.remove(&id);
        }
    }

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
    /// 当前生效的运行配置。M3 起它可以在**运行期**被显式切换（provider），因此放在锁后面：
    /// 读取一律用 [`Self::plan_snapshot`] / [`Self::requested_label`]，绝不长时间持有。
    plan: Mutex<ServeConfigPlan>,
    model_plan: ModelPlan,
    token: ServeToken,
    local: LocalOrigin,
    page: String,
    nonce: String,
    allow_download: bool,
    allow_download_hosts: Vec<String>,
    /// `--allow-provider-fallback`（§7.5）：运行期切换 provider 复用**同一个**开关。
    allow_provider_fallback: bool,
    routing: OcrRouting,
    /// 页面公式检测模型（M4）；`None` = 公式路由不可用。
    formula_detector: Option<FormulaDetectorSpec>,
    /// `--eval-root` 沙箱（M1 评审 P2-3）；`None` = `/api/evaluate` 整体关闭。
    eval_root: Option<EvalRoot>,
    engine_state: Mutex<EngineStateMachine>,
    engine: Mutex<Option<Box<dyn OcrBackend>>>,
    /// 引擎加载的互斥：`POST /api/ocr` 的惰性创建、`POST /api/engine/reload` 与 M3 的
    /// provider 切换只能有一个在动引擎（`engine_state` 的锁**不**覆盖加载过程，否则
    /// `/api/status` 在加载期间会被阻塞、也看不到 `loading`/`rebuilding`）。
    ///
    /// **锁序**：永远是 `engine_load` → `engine`（切换 provider 时按这个顺序同时持有两者）。
    engine_load: Mutex<()>,
    /// 上一次**真正建立会话**的耗时（毫秒）；`None` = 还没有建立过。
    engine_load_ms: Mutex<Option<u64>>,
    /// 是否已经有一个 provider 切换在跑（M3）。
    ///
    /// 切换序列比一次请求的合理占线时间长得多（排空 + 两次建会话），因此它在**独立线程**里
    /// 执行、由那个线程写响应（见 `http.rs::spawn_provider_switch`）。这个标志保证同时
    /// 只有一个切换：第二个请求得到 503 `busy`，而不是排队等一个可能很久的序列。
    provider_switch: AtomicBool,
    /// 是否已经有一个 `POST /api/evaluate` 在跑（M4）。
    ///
    /// 评估是**批量**动作（每张图一次完整推理，最多 `--max-eval-cases` 张），因此它在
    /// 独立线程里执行、由那个线程写响应：accept 线程立刻回到循环，`/api/status`、
    /// 两个 OCR 队列与下载任务在整个评估期间照常可用。同时只允许一个评估在跑，
    /// 第二个请求得到 503 `busy`（排队只会让两个客户端都等到一个很长的序列结束）。
    evaluation: AtomicBool,
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

    /// 当前生效的运行配置快照（克隆；调用方不得在持有其它锁时长期持有它）。
    pub fn plan_snapshot(&self) -> ServeConfigPlan {
        lock(&self.plan).clone()
    }

    /// `/api/status` 的 `requested`（生效值，不是"曾经请求过"的值）。
    pub fn requested_label(&self) -> String {
        lock(&self.plan).requested_label()
    }

    pub fn provider_status(&self) -> ProviderStatus {
        self.engine_state().provider_status(&self.requested_label())
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
        self.plan_snapshot().engine.global.min_side_len as u32
    }

    /// `/api/models` 与 OCR 409 的**同一份**模型状态（§7.6 的"字段一致"）。
    pub fn models(&self) -> ModelReport {
        self.model_plan.report()
    }

    pub fn routing(&self) -> OcrRouting {
        self.routing.clone()
    }

    /// 公式队列任务的 `FormulaPolicy`（M4）：识别模型、**集合声明的 SHA-256** 与检测模型
    /// （路径 + **它的**声明哈希）都来自模型集，检测模型只可能来自启动期解析的那一个文件。
    ///
    /// 返回 `None` 只有一种情况：模型集里没有公式识别模型（启动期已拒绝，理论上不可达）。
    pub fn formula_policy(&self) -> Option<FormulaPolicy> {
        let (model_path, sha256) = self.model_plan.formula_recognizer().ok()?;
        Some(FormulaPolicy {
            enabled: true,
            model_path: Some(model_path),
            expected_model_sha256: Some(sha256),
            detector_path: self.formula_detector.as_ref().map(|spec| spec.path.clone()),
            // 评审 P1-1：检测模型的哈希与识别模型走**同一条**规则。集合没有声明它时
            // 这里是 `None`（如实表示"没有可信摘要"），而不是留空后静默加载。
            expected_detector_sha256: self
                .formula_detector
                .as_ref()
                .and_then(|spec| spec.expected_sha256.clone()),
            ..FormulaPolicy::default()
        })
    }

    /// 公式队列的准入判定（§4.4 第 4 步之后、读 body 之前）：公式 role 的文件是否**可用**。
    ///
    /// 判定用的是与 `/api/models` **同一份**逐文件哈希状态（`file.state_in` →
    /// 身份键控的校验缓存），因此"报告损坏"与"拒绝请求"不可能分叉：损坏但存在的公式文件
    /// 现在在**读 body 之前**就是 409 `models_corrupt`，而不是先建任务、再由 worker 失败。
    /// 配置好的检测模型同属公式管线，因此它的状态也在这里判定。
    ///
    /// 成本如实：冷验证真的读盘（启动快照已经做过一次，566 MB 公式模型的实测耗时记在
    /// `/api/models` 的 `verification` 块里）；命中只花一次 `stat`，与文件大小无关。
    pub fn formula_models_ready(&self) -> bool {
        self.models().formula_blocking_names().is_empty() && !self.formula_detector_blocks()
    }

    /// 上面那条预检的响应体（与 `/api/models` 的 `formula` 块同源同序）。
    pub fn formula_blocked_body(&self) -> Body {
        Body::json(409, render_error_body(&self.formula_missing_failure()))
    }

    /// 公式队列的 409 载荷（`code` 由磁盘上的哈希结论决定，`detail` 是公式作用域）。
    fn formula_missing_failure(&self) -> ErrorBody {
        let (_, corrupt) = self.formula_blocking_lists();
        ErrorBody {
            code: if corrupt.is_empty() {
                "models_missing"
            } else {
                "models_corrupt"
            },
            message: "the formula model set is incomplete".to_string(),
            detail: self.formula_detail(),
        }
    }

    /// 公式管线的阻塞清单 `(missing, corrupt)`：模型集里的公式 role **加上**启动期解析出的
    /// 检测模型。
    ///
    /// 检测模型可能来自 `--formula-detector`（不在任何集合里），因此它必须**单独**补进来，
    /// 否则"检测模型损坏"会得到一份空清单：`code` 是 `models_corrupt` 却列不出文件名。
    /// 已经在集合里报过的名字不重复列出（同一个 role 两种来源只报一次）。
    fn formula_blocking_lists(&self) -> (Vec<String>, Vec<String>) {
        let report = self.models();
        let mut missing = report.formula_missing_names();
        let mut corrupt = report.formula_corrupt_names();
        if let Some(status) = self.formula_detector_status()
            && !missing.contains(&status.name)
            && !corrupt.contains(&status.name)
        {
            match status.state {
                ModelFileState::Missing => missing.push(status.name),
                ModelFileState::Corrupt { .. } => corrupt.push(status.name),
                ModelFileState::Present => {}
            }
        }
        (missing, corrupt)
    }

    /// 配置好的公式检测模型在磁盘上的状态（评审 P1-1 的根因回归）。
    ///
    /// 与识别模型**同一条规则**：复用库的 [`ModelFileSpec::state_in`]（唯一的逐文件状态
    /// 实现）与它背后的身份键控校验缓存，因此"`/api/models` 报告它是损坏的"与
    /// "加载它时被拒绝"是同一个结论，不可能分叉。
    ///
    /// `None` = 没有配置检测模型（公式路由本来就不该可用）。没有声明哈希时状态是
    /// `Present`——库只能证明"文件在"，这一点由 `/api/models` 的 `sha256: null` 如实说明。
    pub fn formula_detector_status(&self) -> Option<FormulaDetectorStatus> {
        let spec = self.formula_detector.as_ref()?;
        let name = file_name(&spec.path.to_string_lossy());
        let root = spec.path.parent().unwrap_or_else(|| Path::new("."));
        let expected = spec.expected_sha256.clone().unwrap_or_default();
        let state = match ModelFileSpec::new(
            name.clone(),
            ModelRole::FormulaDetector,
            None,
            expected.clone(),
            String::new(),
        ) {
            Ok(file) => file.state_in(root),
            Err(error) => ModelFileState::Corrupt {
                expected,
                actual: format!("cannot describe the detector file: {error}"),
            },
        };
        Some(FormulaDetectorStatus { name, state })
    }

    /// 检测模型是否**阻塞**公式队列（缺失或损坏）。`Present`（含"存在但没有可信摘要"）
    /// 不阻塞：没有摘要时库无法判定内容对错，把它当成损坏是伪造证据。
    fn formula_detector_blocks(&self) -> bool {
        matches!(
            self.formula_detector_status().map(|status| status.state),
            Some(ModelFileState::Missing | ModelFileState::Corrupt { .. })
        )
    }

    /// 公式队列 409 的 `detail`（**公式**作用域；与 `/api/models` 的 `formula` 块同值）。
    ///
    /// `missing`/`corrupt`/`blocked` 都来自那一份**哈希判定过的**状态：请求路径上不再有
    /// 第二套"只看存在性"的清单（评审 P1-2 的根因是那种廉价清单让损坏文件溜过准入）。
    pub fn formula_detail(&self) -> Value {
        let (missing, corrupt) = self.formula_blocking_lists();
        let mut blocked = missing.clone();
        blocked.extend(corrupt.iter().cloned());
        json!({
            "scope": "formula",
            "missing": missing,
            "corrupt": corrupt,
            "blocked": blocked,
            "source": source_label(self.model_plan.source()),
            "model_dir": REDACTED_MODEL_DIR,
            "detector": self.formula_detector_detail(),
        })
    }

    /// `detail.detector` / `/api/models.formula.detector` 的**同一份**取值。
    fn formula_detector_detail(&self) -> Value {
        let status = self.formula_detector_status();
        json!({
            "configured": self.formula_detector.is_some(),
            "file": status.as_ref().map(|status| status.name.clone()),
            "sha256": self
                .formula_detector
                .as_ref()
                .and_then(|spec| spec.expected_sha256.clone()),
            "state": status.as_ref().map(|status| status.state.as_str()),
        })
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
        let provider = engine_state.provider_status(&self.requested_label());
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
                    // M3 的诊断口径：还在保留区里的原图份数（§4.5 的字节预算对象）。
                    "retained_originals": state.originals.len(),
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
            "formula": self.formula_status_json(&self.models()),
            "limits": {
                "max_body_bytes": self.limits.max_body_bytes,
                "max_result_bytes": self.limits.max_result_bytes,
                "max_export_bytes": self.limits.max_export_bytes,
                "max_download_bytes": self.limits.max_download_bytes,
                "max_retained": self.limits.max_retained,
                "max_retained_bytes": self.limits.max_retained_bytes,
                "max_tombstones": self.limits.max_tombstones,
                // M4：`POST /api/evaluate` 的用例上限（一张清单最多评多少张图）。
                "max_eval_cases": self.limits.max_eval_cases,
            },
            "model_dir": REDACTED_MODEL_DIR,
            "source": source_label(self.model_plan.source()),
            "downloads_allowed": self.allow_download,
            "download_hosts": self.effective_download_hosts(),
            "engine_load_ms": self.engine_load_ms(),
            "allow_provider_fallback": self.allow_provider_fallback,
            "max_side_len": self.plan_snapshot().engine.global.max_side_len,
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
    ///
    /// # M4 的作用域（根因修复，不是加字段）
    ///
    /// M1 的顶层 `complete`/`missing`/`corrupt`/`blocked` 是"所有集合的并集"，因为当时只有
    /// 一条管线。M4 请求了公式集合（566 MB，默认不下载）之后，这个并集会让**普通 OCR**
    /// 因为公式模型没下载而报 409——一个把可选能力变成硬依赖的错误结论。
    /// 因此：
    ///
    /// - 顶层四个字段 = **文本管线**（= 引擎要加载的那些文件；`POST /api/ocr` 的 409 用它）；
    /// - 新增 `formula` 块 = **公式管线**的同一组字段 + 路由是否可用 + 不可用的文字理由；
    /// - `sets[]` 保持 §5.4 的扁平形状不变（页面按 `files[].role` 自己分组，两边判据一致）。
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
            "formula": self.formula_status_json(&report),
            "verification": verification_json(report.cold_this_call()),
            "sets": report.statuses().iter().map(set_status_json).collect::<Vec<_>>(),
        })
    }

    /// `/api/models` 的 `formula` 块（**唯一**实现；`/api/ocr` 的公式 409 复用它）。
    ///
    /// 页面用它给公式开关设门禁：`complete`（模型集齐备）**与** `routing`（服务端真的能把
    /// 请求跑成公式区域）两者都成立才允许勾选，`disabled_reason` 给出文字理由。
    pub fn formula_status_json(&self, report: &ModelReport) -> Value {
        json!({
            "complete": report.formula_complete(),
            "missing": report.formula_missing_names(),
            "corrupt": report.formula_corrupt_names(),
            "blocked": report.formula_blocking_names(),
            "routing": self.routing.formula,
            "disabled_reason": self.routing.disabled_reason,
            "required_roles": ["formula_recognizer"],
            "detector": self.formula_detector_detail(),
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

    /// 一次 `ServeError` → 响应体（**唯一**的映射点；HTTP 层与评估线程共用）。
    ///
    /// §7.6 要求"模型缺失/损坏"的错误体带**与 `/api/models` 同源同值**的清单：这两类错误
    /// 因此在这里补齐 `detail`。映射按**错误种类**而不是按路由：只有 OCR 与评估这两条
    /// 真实推理路径会产生它们，因此不需要在每个调用点重复一次路由判断。
    pub(super) fn error_body(&self, error: &ServeError) -> Body {
        match error {
            ServeError::ModelsMissing | ServeError::ModelsCorrupt => {
                Body::error_with_detail(&self.models_missing_error(), self.models_missing_detail())
            }
            other => Body::error(other),
        }
    }

    /// **原子地**预留一个 OCR 队列槽位（§4.4 第 4 步；评审 P2-1 的根因修复）。
    ///
    /// 准入层在第 4 步调用它：判定与占位是同一次加锁，因此两个并发请求不可能都通过
    /// "队列没满"的预检。返回的凭据要么被 [`Self::submit_ocr`] 提交成真正的队列条目，
    /// 要么在请求失败/中止时由 `Drop` 释放（容量不会泄漏）。
    ///
    /// 为什么不是"先 `queue_full()` 再 `enqueue()`"：那是两个临界区，中间可以插入任意多个
    /// 并发请求，每一个都已经把大 body 读进内存——"拒绝时不读 body"这条保证因此不成立。
    pub fn reserve_queue_slot(self: &Arc<Self>, class: QueueClass) -> Option<QueueReservation> {
        {
            let mut state = lock(&self.jobs);
            if !state.reserve(class) {
                return None;
            }
        }
        let shared = Arc::clone(self);
        Some(QueueReservation::new(move || {
            lock(&shared.jobs).release(class);
        }))
    }

    /// 调度器里是否还有任务（OCR worker 在**取任务之前**判断"要不要建引擎"）。
    fn has_queued_work(&self) -> bool {
        !lock(&self.jobs).scheduler.is_empty()
    }

    /// `POST /api/ocr`：准入已经在 http 层完成，这里只建任务并入队（§4.4 第 7 步）。
    ///
    /// `reservation` 是第 4 步预留的槽位（准入层给出）。它让"检查 → 入队"之间不再有
    /// 并发窗口：入队成功即提交；本函数任何一次提前返回都会让凭据被丢弃并归还容量。
    pub fn submit_ocr(
        &self,
        bytes: Vec<u8>,
        class: QueueClass,
        max_side: Option<u32>,
        reservation: Option<QueueReservation>,
    ) -> Result<Value, ServeError> {
        // §7.6 + M2 的惰性创建：模型缺失/引擎不可用在这里拒绝（不建任务、不入队）。
        // 提前返回时 `reservation` 在函数结束时被丢弃 → 槽位归还（锁在它之前释放，
        // 因为局部变量先于参数析构）。
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
        // M3 起这份字节**不再**在识别后被丢掉（§4.5：annotated.png 按需重新解码），
        // 它的账目由 `JobStore::original_bytes` 承担，释放时经 `sync_originals` 同步移除。
        state.originals.insert(
            id.clone(),
            RetainedOcr {
                bytes: Arc::from(bytes.into_boxed_slice()),
                max_side,
            },
        );
        // 新任务可能把字节预算推过上限：`insert` 内部已经按"先释放原图、再淘汰任务"处理，
        // 这里把释放结果落成真正的丢弃。
        state.sync_originals();
        state.sync_positions();
        drop(state);
        // **提交完成**：队列入队已经在同一个临界区里发生，现在归还预留（入队在先、
        // 释放在后：多算的那一格只会让并发请求被保守拒绝，绝不超卖）。
        drop(reservation);
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
    /// 成功 → 结果的 JSON（用**有界写入器**重新序列化，与 `export?format=json`
    /// 逐字节相同；长度上限仍是 `--max-result-mb`）；
    /// 失败 → **重放原始状态码与错误体**
    /// （`422 unsupported_input` / `413 result_too_large` / `503 engine_unavailable`）；
    /// 未完成（含已取消）→ 409 `job_not_finished`，任务的确切状态由
    /// `GET /api/jobs/{id}` 如实给出。
    pub fn job_result(&self, id: &str) -> Result<Body, ServeError> {
        let snapshot = self.job_snapshot(id)?;
        match snapshot.state {
            JobLifecycle::Succeeded => {
                let Some(output) = snapshot.output else {
                    return Err(ServeError::Internal);
                };
                let value = export::result_json(&output).map_err(ServeError::from)?;
                match serialize_bounded(&value, self.limits.max_result_bytes) {
                    Ok(bytes) => Ok(Body::json(200, bytes)),
                    Err(SerializeError::TooLarge) => Err(ServeError::ResultTooLarge),
                    Err(SerializeError::Internal(reason)) => {
                        eprintln!("serve: cannot serialize the result of {id}: {reason}");
                        Err(ServeError::Internal)
                    }
                }
            }
            JobLifecycle::Failed => match snapshot.failure {
                Some((status, body)) => Ok(Body::json(status, render_error_body(&body))),
                None => Err(ServeError::Internal),
            },
            JobLifecycle::Queued | JobLifecycle::Running | JobLifecycle::Cancelled => {
                Err(ServeError::JobNotFinished)
            }
        }
    }

    /// `GET /api/jobs/{id}/annotated.png`（§4.2、§4.5）。
    ///
    /// 原图只保留**编码字节**，这里按需重新解码（库的 `LoadImage`）、用
    /// `output::visualize::draw_output` 叠加检测框、再编码成 PNG。
    ///
    /// - 任务不在 / 已淘汰 → 404 `job_not_found` / 410 `job_evicted`（[`Self::job_snapshot`]）；
    /// - 还没有结果（排队/运行中/失败/取消）→ **409 `job_not_finished`**（与 `/result` 同语义：
    ///   没有区域可叠，不能凭空造一张图）；
    /// - 结果在、原图被保留预算释放 → **410 `original_evicted`**（§4.2 冻结的结论）。
    pub fn annotated_png(&self, id: &str) -> Result<Body, ServeError> {
        let snapshot = self.job_snapshot(id)?;
        if snapshot.state != JobLifecycle::Succeeded {
            return Err(ServeError::JobNotFinished);
        }
        let Some(output) = snapshot.output else {
            return Err(ServeError::Internal);
        };
        let Some(original) = snapshot.original else {
            return Err(ServeError::OriginalEvicted);
        };
        // 标注 PNG 没有文档预算，因此这里不会出现 `TooLarge`。
        let png = export::annotated_png(original, &output)
            .map_err(|error| export_failure(error, 0, ""))?;
        Ok(Body::typed(200, "image/png", png))
    }

    /// `GET /api/jobs/{id}/export?format=json|md|html`（§4.2、§4.6、§9.5）。
    ///
    /// - `json`：与 `/result` **同一份** JSON（库的 `to_output_json` + `plain_text` +
    ///   `timing_ledger`），上限仍是 `--max-result-mb`（§4.6）且不得超过 `--max-export-mb`；
    /// - `md`：库的 `to_output_markdown`；
    /// - `html`：库的报告渲染器 + **[`ReportMode::Static`]**（正文不含任何 `<script`）+
    ///   标注图 `data:` 内嵌，因此导出文件脱离服务仍可查看（§9.5 第 5 条）。
    ///
    /// 三者都经**有界写入器**：超限是 413 `export_too_large`（`detail` 指向
    /// `/api/jobs/{id}/annotated.png`），**绝不**截断、也绝不返回一条图片链接已死的文档。
    pub fn export(&self, id: &str, format: ExportFormat) -> Result<Body, ServeError> {
        let snapshot = self.job_snapshot(id)?;
        if snapshot.state != JobLifecycle::Succeeded {
            return Err(ServeError::JobNotFinished);
        }
        let Some(output) = snapshot.output else {
            return Err(ServeError::Internal);
        };
        let limit = self.limits.max_export_bytes;
        let annotated = format!("/api/jobs/{id}/annotated.png");
        let document = match format {
            ExportFormat::Json => export::json_document(&output, limit, snapshot.serialized_bytes),
            ExportFormat::Markdown => export::markdown_document(&ExportRequest {
                output: &output,
                png: &[],
                title: &format!("ocr-{id}"),
                limit_bytes: limit,
            }),
            ExportFormat::Html => {
                // HTML 必须是**真正可离线使用**的单文件：没有内嵌图片就没有意义，
                // 因此原图被释放时如实报 410，而不是发一条外链失效的文档（§9.5 第 5 条）。
                let Some(original) = snapshot.original else {
                    return Err(ServeError::OriginalEvicted);
                };
                let png = export::annotated_png(original, &output)
                    .map_err(|error| export_failure(error, limit, &annotated))?;
                export::html_document(&ExportRequest {
                    output: &output,
                    png: &png,
                    title: &format!("ocr-{id}"),
                    limit_bytes: limit,
                })
            }
        };
        let bytes = document.map_err(|error| export_failure(error, limit, &annotated))?;
        Ok(Body::typed(200, format.content_type(), bytes))
    }

    /// 一次任务查询在**同一把锁**下取得的全部结论。
    ///
    /// 正是这一步消除了"判定成功 → 结果已被淘汰"的分叉：状态、结果与原图三者要么
    /// 一起存在，要么一起不存在。
    fn job_snapshot(&self, id: &str) -> Result<JobSnapshot, ServeError> {
        let state = lock(&self.jobs);
        let record = state.store.record(id)?;
        let outcome = state.results.get(id);
        let (output, serialized_bytes, failure) = match outcome {
            Some(Outcome::Succeeded(succeeded)) => (
                Some(Arc::clone(&succeeded.output)),
                succeeded.serialized_bytes,
                None,
            ),
            Some(Outcome::Failed(status, body)) => (None, 0, Some((*status, body.clone()))),
            None => (None, 0, None),
        };
        Ok(JobSnapshot {
            state: record.state,
            output,
            serialized_bytes,
            failure,
            original: state
                .originals
                .get(id)
                .map(|retained| Arc::clone(&retained.bytes)),
        })
    }

    /// `POST /api/jobs/{id}/cancel`（§4.3 与 §6.6）。
    ///
    /// 排队中 → 立即取消（并从调度器/保留原图里移除）；运行中的**下载** → 登记取消请求
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
        }
        // 取消的任务不会有注释图：`cancel` 已经在记账层释放了原图，这里把字节真正丢掉。
        state.sync_originals();
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

    /// OCR worker 用：取走待处理的原图**引用**。
    ///
    /// M3 起它**不**移除保留区里的字节（识别成功后 `/annotated.png` 还要用它）：
    /// 返回的是 `Arc` 克隆，图片本身一份都不复制。
    fn retained_ocr(&self, id: &str) -> Option<(Arc<[u8]>, Option<u32>)> {
        lock(&self.jobs)
            .originals
            .get(id)
            .map(|retained| (Arc::clone(&retained.bytes), retained.max_side))
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
            state.sync_originals();
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
        // 失败任务的原图已经在记账层释放；成功任务的原图可能因字节预算被释放。
        state.sync_originals();
    }

    /// 建立一次会话（**唯一**实现：钉住模型路径 → 调工厂 → 记录耗时）。
    ///
    /// `docs/05` §5.3/§7.6：引擎的模型路径一律由 `--model-dir` 的模型集钉住，
    /// 而不是由 `--config` 的 `model_path` 决定；探测与真实加载用同一段代码。
    fn create_session(
        &self,
        plan: &ServeConfigPlan,
    ) -> (Result<Box<dyn OcrBackend>, RapidOcrError>, u64) {
        let started = Instant::now();
        let mut config = plan.engine.clone();
        let result = match self.model_plan.pin_engine_paths(&mut config) {
            Ok(()) => (self.engine_factory)(&config),
            Err(error) => Err(RapidOcrError::ModelResolve(error.to_string())),
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        (result, elapsed_ms)
    }

    /// 惰性创建 / 显式重建引擎（§7.6）。
    ///
    /// 这就是 `EngineStateMachine::{begin_loading, models_still_missing}` 的**生产者**：
    ///
    /// - 模型齐备 → `Loading → Ready|Failed`（真实建立会话；`Loading` 期间 `engine_state`
    ///   的锁**不被持有**，因此 `/api/status` 能看到 `loading`，新 OCR 请求排队而不被拒绝）；
    /// - 模型仍缺失 → 保持 `blocked_models_missing` 并刷新缺失清单（`models_still_missing`）；
    /// - `force = false`（`POST /api/ocr` 的惰性路径）在已经 `Ready` 且引擎在场时立即返回，
    ///   `force = true`（`POST /api/engine/reload` 的**无请求体**形式）即使已 `Ready`
    ///   也重新建立会话（用户的显式意图是"按磁盘上的当前文件重新加载"）。
    ///
    /// **M3 的 provider 切换不经过这里**：它走
    /// [`Self::apply_provider`] 的 `Ready → Rebuilding → …` 序列（暂停新任务 → 排空 →
    /// 销毁旧 engine → 建立新 engine），因为那是"切换设置"，与"按当前文件重建"是两件事。
    /// M4 起 `POST /api/evaluate` 也在自己的线程里调它（与 OCR worker 走同一条惰性路径：
    /// 会话绝不在 accept 线程上建立）。
    pub(super) fn ensure_engine_loaded(&self, force: bool) -> EngineLoad {
        let plan = self.plan_snapshot();
        if !force
            && matches!(self.engine_state(), EngineState::Ready { .. })
            && lock(&self.engine).is_some()
        {
            return EngineLoad::Ready;
        }
        // 一次只允许一个加载者（惰性路径、reload 与 provider 切换可能同时到达）。
        let _loading = lock(&self.engine_load);
        if !force
            && matches!(self.engine_state(), EngineState::Ready { .. })
            && lock(&self.engine).is_some()
        {
            return EngineLoad::Ready;
        }
        self.load_engine(&plan)
    }

    /// `engine_load` 已经持有的加载路径（见 [`Self::ensure_engine_loaded`] 的文档）。
    fn load_engine(&self, plan: &ServeConfigPlan) -> EngineLoad {
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
        let (result, elapsed_ms) = self.create_session(plan);
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
                    plan.requested_label(),
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
    /// 请求体**可选**：不带 body 是 M2 的"按磁盘上的当前文件重建会话"；带
    /// `{"provider": "cpu"|"directml"|"cuda"}` 是 M3 的**显式设置应用**
    /// （[`Self::apply_provider`]，含 `Ready → Rebuilding → …` 的完整序列）。
    ///
    /// 响应体里的 `engine` 与 `/api/status` 的 `engine` **是同一个值**（冻结的
    /// `EngineState` 形状），因此"模型仍缺失"时客户端读到的是
    /// `{"state":"blocked_models_missing","missing":[…]}`——**不是**一句模糊的失败。
    /// `missing`/`corrupt` 与 `/api/models` 同源同值；`load_ms` 是**本次调用**的墙钟耗时
    /// （`/api/status` 的 `engine_load_ms` 是上一次真正建立会话的耗时）。
    ///
    /// **两种形态都由 `http.rs` 在独立线程里调用**（那个线程负责写响应），因此
    /// `/api/status` 与 `POST /api/ocr` 在整个建会话序列期间照常可用——无 body 的
    /// "按当前文件重建"与显式 provider 切换在这一条上没有区别（评审 P2-2）。
    pub fn reload_engine(&self, provider: Option<ProviderPreference>) -> Result<Value, ServeError> {
        let started = Instant::now();
        let Some(requested) = provider else {
            let outcome = self.ensure_engine_loaded(true);
            return Ok(self.engine_payload(outcome, started, None, None));
        };
        self.apply_provider(requested, started)
    }

    /// 取得"由我执行这次建会话序列"的资格（同时只允许一个）。
    ///
    /// 第二个并发请求拿到 `None` → 503 `busy`（**不排队**：排队只会让两个客户端都等到
    /// 一个很长的序列结束，而结果还是后者的设置生效）。凭据持有 `Arc`，因此它可以被
    /// 移动到执行切换的那个线程里。
    pub fn begin_provider_switch(self: &Arc<Self>) -> Option<ProviderSwitchGuard> {
        self.provider_switch
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(ProviderSwitchGuard {
            shared: Arc::clone(self),
        })
    }

    /// 取得"由我执行这次评估"的资格（同时只允许一个，M4）。
    ///
    /// 与 [`Self::begin_provider_switch`] 同一形状：第二个请求得到 503 `busy`，
    /// 凭据在线程退出时（含 panic）释放。
    pub fn begin_evaluation(self: &Arc<Self>) -> Option<EvaluationGuard> {
        self.evaluation
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(EvaluationGuard {
            shared: Arc::clone(self),
        })
    }

    /// `POST /api/evaluate` 的用例上限（`--max-eval-cases`）。
    pub fn max_eval_cases(&self) -> usize {
        self.limits.max_eval_cases
    }

    /// `--eval-root` 沙箱；`None` = 评估端点整体关闭（见 [`super::evaluate::EvalRoot`]）。
    pub fn eval_root(&self) -> Option<&EvalRoot> {
        self.eval_root.as_ref()
    }

    /// 运行期切换 provider 的**唯一**入口（§7.5、§7.6、M3）。
    ///
    /// 顺序逐条对应 `docs/05` §11 的 M3 清单：
    ///
    /// 1. **校验**：用启动期**同一套**规则（[`ServeConfigPlan::validate`]：provider 名称、
    ///    对应 feature 是否编译进来、`fail_if_provider_unavailable` 的冻结语义）先算出新配置。
    ///    配置非法（例如 `--features directml-provider` 没打开的构建里请求 `directml`）→
    ///    **400 `bad_request`** + `detail.reason` 是库侧原文，**任何状态都不动**；
    /// 2. **暂停新任务**：`Ready → Rebuilding`（§7.6 的合法边）。此刻 `POST /api/ocr` 的准入是
    ///    `OcrAdmission::Queue`——**入队（202）而不是拒绝**（§7.6 原文：`Loading`/`Rebuilding`
    ///    期间新请求排队；状态机里那条规则就是为它冻结的）；
    /// 3. **排空**：`engine_load` 已经被本线程持有，worker 只有在拿到它之后才会去拿 `engine`，
    ///    因此随后获取 `engine` 锁会**恰好等到正在进行的那次推理结束**；从这一刻起既没有
    ///    推理在跑，也不会有新的推理开始（在跑的推理不会被中断，§4.3）；
    /// 4. **销毁旧 engine → 建立新 engine**（顺序与 [`Self::load_engine`] 一致：旧会话先丢，
    ///    避免失败时留下一个与 `/api/status` 不一致的可用引擎）；
    /// 5. `Rebuilding → Ready`（成功）或 `Rebuilding → Failed`（失败）；
    /// 6. **失败恢复旧 engine**：用旧配置重建会话。成功 → 状态回到 `Ready`、生效配置回到旧值
    ///    （如实反映"切换没生效"），响应 `outcome = "rolled_back"` 并带 `error`；
    ///    连旧引擎也起不来 → 明确的 `Failed`，`reason` 里**两个原因都写**。
    ///
    /// 非 `Ready` 状态（`BlockedModelsMissing`/`Failed`/`Loading`）没有"旧引擎要销毁、
    /// 运行中任务要排空"这件事：应用设置后走 `Loading → Ready|Failed`（与 M2 同一条路径）。
    fn apply_provider(
        &self,
        requested: ProviderPreference,
        started: Instant,
    ) -> Result<Value, ServeError> {
        let old_plan = self.plan_snapshot();
        let new_plan = old_plan
            .clone()
            .with_provider(requested, self.allow_provider_fallback)
            .map_err(|error| ServeError::ProviderRejected {
                provider: rapid_ocr_rs::format_provider_preference(requested),
                reason: error.to_string(),
            })?;

        // 整个切换序列都在 `engine_load` 的临界区里：worker 的 `ensure_engine_loaded`
        // 会一直等到切换结束，因此它**看不到** `Rebuilding`（它只会在切换完成后继续），
        // 而 `/api/status` 能看到——状态锁不被持有。
        let _loading = lock(&self.engine_load);

        {
            let mut machine = lock(&self.engine_state);
            if machine.begin_rebuild().is_err() {
                // 状态在上一行之后变了（例如并发的惰性创建把它推到了 `Failed`）：
                // 按普通加载路径应用设置，不假装进入过 `Rebuilding`。
                drop(machine);
                *lock(&self.plan) = new_plan.clone();
                let outcome = self.load_engine(&new_plan);
                return Ok(self.engine_payload(outcome, started, None, None));
            }
        }
        // `requested` 立刻生效：`Rebuilding` 期间 `/api/status` 报告**新**的 requested，
        // 而 `selected_ep` / `fallback_to_cpu` 是 `null`（`EngineState::provider_status`
        // 的未知态语义：不把"还不知道"伪装成 `false`）。
        *lock(&self.plan) = new_plan.clone();

        // 排空 + 销毁旧 engine：拿到 `engine` 锁即"在跑的推理已经结束"。
        let mut engine = lock(&self.engine);
        *engine = None;
        let (created, session_ms) = self.create_session(&new_plan);
        *lock(&self.engine_load_ms) = Some(session_ms);
        match created {
            Ok(backend) => {
                let provider = backend.provider();
                *engine = Some(backend);
                drop(engine);
                // 状态锁在**单独的块**里，绝不跨到 `engine_payload`（它自己也要读状态锁，
                // 而 `std::sync::Mutex` 不可重入）。
                {
                    let mut machine = lock(&self.engine_state);
                    let _ = machine.load_succeeded(
                        new_plan.requested_label(),
                        provider.selected_ep,
                        provider.fallback_to_cpu,
                    );
                }
                Ok(self.engine_payload(EngineLoad::Ready, started, None, None))
            }
            Err(new_error) => {
                let new_reason = new_error.to_string();
                // 恢复旧引擎：用**旧配置**重建会话（旧对象已经被销毁，只能重建）。
                let (restored, rollback_ms) = self.create_session(&old_plan);
                *lock(&self.engine_load_ms) = Some(rollback_ms);
                match restored {
                    Ok(backend) => {
                        let provider = backend.provider();
                        *engine = Some(backend);
                        drop(engine);
                        // 生效配置回到旧值：`/api/status` 必须报告**真正在跑的那个** provider。
                        *lock(&self.plan) = old_plan.clone();
                        {
                            let mut machine = lock(&self.engine_state);
                            let _ = machine.load_succeeded(
                                old_plan.requested_label(),
                                provider.selected_ep,
                                provider.fallback_to_cpu,
                            );
                        }
                        Ok(self.engine_payload(
                            EngineLoad::Ready,
                            started,
                            Some(new_reason),
                            Some(rollback_ms),
                        ))
                    }
                    Err(restore_error) => {
                        let reason = format!(
                            "switching the execution provider to {} failed ({new_reason}); \
                             restoring the previous engine ({}) also failed ({restore_error})",
                            new_plan.requested_label(),
                            old_plan.requested_label()
                        );
                        drop(engine);
                        {
                            let mut machine = lock(&self.engine_state);
                            let _ = machine.load_failed(reason.clone());
                        }
                        Ok(self.engine_payload(
                            EngineLoad::Failed,
                            started,
                            Some(reason),
                            Some(rollback_ms),
                        ))
                    }
                }
            }
        }
    }

    /// reload / provider 切换的响应体（`engine` 与 `/api/status` 同形同值）。
    ///
    /// `error` 非空且 `outcome` 是 `ready` 时，`outcome` 报 **`rolled_back`**：
    /// 请求的设置没有生效、旧引擎仍在服务，客户端据此可以区分"切过去了"与"没切成"。
    fn engine_payload(
        &self,
        outcome: EngineLoad,
        started: Instant,
        error: Option<String>,
        rollback_ms: Option<u64>,
    ) -> Value {
        let report = self.models();
        let provider = self.provider_status();
        let outcome_label = match (&error, outcome) {
            (Some(_), EngineLoad::Ready) => "rolled_back",
            _ => match outcome {
                EngineLoad::Ready => "ready",
                EngineLoad::BlockedModelsMissing => "blocked_models_missing",
                EngineLoad::Failed => "failed",
            },
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        json!({
            "outcome": outcome_label,
            "engine": self.engine_state(),
            "provider": provider,
            "requested": provider.requested,
            "selected_ep": provider.selected_ep,
            "fallback_to_cpu": provider.fallback_to_cpu,
            "missing": report.missing_names(),
            "corrupt": report.corrupt_names(),
            "source": source_label(self.model_plan.source()),
            "model_dir": REDACTED_MODEL_DIR,
            "load_ms": elapsed_ms,
            "rollback_ms": rollback_ms,
            "error": error,
        })
    }
}

/// [`ServeShared::begin_provider_switch`] 的资格凭据：`Drop` 时释放（panic 也释放）。
pub(super) struct ProviderSwitchGuard {
    shared: Arc<ServeShared>,
}

impl Drop for ProviderSwitchGuard {
    fn drop(&mut self) {
        self.shared.provider_switch.store(false, Ordering::SeqCst);
    }
}

/// [`ServeShared::begin_evaluation`] 的资格凭据：`Drop` 时释放（panic 也释放）。
pub(super) struct EvaluationGuard {
    shared: Arc<ServeShared>,
}

impl Drop for EvaluationGuard {
    fn drop(&mut self) {
        self.shared.evaluation.store(false, Ordering::SeqCst);
    }
}

/// 模型缺失/损坏时的引擎失败说明（点名文件，可定位）。
fn missing_models_reason(blocking: &[String]) -> String {
    format!(
        "the model files are missing or corrupt, so no session can be created: {}",
        blocking.join(", ")
    )
}

/// 一次任务查询的完整结论（同一把锁下取出，见 [`ServeShared::job_snapshot`]）。
struct JobSnapshot {
    state: JobLifecycle,
    /// `Succeeded` 时的结构化结果。
    output: Option<Arc<OcrOutput>>,
    /// worker 实测的结果序列化长度（导出 JSON 的 `--max-export-mb` 拒绝里要用它）。
    serialized_bytes: u64,
    /// `Failed` 时要重放的状态码与错误体。
    failure: Option<(u16, ErrorBody)>,
    /// 仍在保留区里的**编码原图**（`None` = 已被保留预算释放，或本就没有）。
    original: Option<Arc<[u8]>>,
}

/// 导出/标注失败的统一映射。
///
/// - `Decode`：原图解码失败仍是库的错误分类（422 `unsupported_input` 等）；
/// - `TooLarge`：只在导出文档里有意义（`limit_bytes` / `annotated` 由调用方给出；
///   标注 PNG 没有文档预算，因此那条路径不会产生它）；
/// - `Render` / `Internal`：500（渲染器或序列化器报错，不是客户端的问题）。
fn export_failure(error: ExportError, limit_bytes: u64, annotated: &str) -> ServeError {
    match error {
        ExportError::TooLarge { observed_bytes } => ServeError::ExportTooLarge {
            limit_bytes,
            observed_bytes,
            annotated: annotated.to_string(),
        },
        ExportError::Decode(error) => ServeError::from(error),
        ExportError::Render(reason) | ExportError::Internal(reason) => {
            eprintln!("serve: export/annotation failed: {reason}");
            ServeError::Internal
        }
    }
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

    /// 任意二进制体（导出文档、标注 PNG）。
    pub fn typed(status: u16, content_type: &'static str, bytes: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
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
            plan: Mutex::new(context.plan),
            model_plan: context.model_plan,
            token: context.token,
            local: context.local,
            page: context.page,
            nonce: context.nonce,
            allow_download: context.allow_download,
            allow_download_hosts: context.allow_download_hosts,
            allow_provider_fallback: context.allow_provider_fallback,
            routing: context.routing,
            formula_detector: context.formula_detector,
            eval_root: context.eval_root,
            engine_state: Mutex::new(machine),
            engine: Mutex::new(backend),
            engine_load: Mutex::new(()),
            engine_load_ms: Mutex::new(None),
            provider_switch: AtomicBool::new(false),
            evaluation: AtomicBool::new(false),
            jobs: Mutex::new(JobState {
                store: JobStore::new(JobStoreLimits::from_limits(&context.limits)),
                scheduler: DualQueueScheduler::new(SchedulerConfig::from_limits(&context.limits)),
                results: ResultStore::new(
                    context.limits.max_retained,
                    context.limits.max_retained_bytes,
                ),
                ids: JobIdGenerator::default(),
                originals: HashMap::new(),
                reserved_text: 0,
                reserved_formula: 0,
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
    // `Loading`（`POST /api/engine/reload` 与惰性创建都走 `load_engine`）。
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
///
/// # M3：建会话在**取任务之前**
///
/// `ensure_engine_loaded` 只有在 `engine_load` 空闲时才返回——provider 切换期间它一直
/// 被切换线程持有，因此本 worker 会在**还没有把任何任务标成 `running`** 的时候等在那里。
/// 这有两个直接好处（§7.6 的"暂停新任务 → 排空"）：
///
/// 1. 排空判据是干净的：`Rebuilding` 期间不存在"状态是 running、其实在等引擎"的任务；
/// 2. 切换结束后本 worker 直接看到 `Ready`（新引擎），队列里的任务用**新**引擎执行，
///    `/api/ocr` 在切换期间返回的是 202 `queued`（`OcrAdmission::Queue`），不是拒绝。
fn ocr_worker(runtime: Arc<ServeShared>) {
    while !runtime.is_shutting_down() {
        if !runtime.has_queued_work() {
            wait_for_work(&runtime);
            continue;
        }
        match runtime.ensure_engine_loaded(false) {
            EngineLoad::Ready => {}
            EngineLoad::BlockedModelsMissing => {
                // 与 `/api/models` 同源同值的 409（`code`/`detail` 都是同一份计算）。
                fail_next_queued(&runtime, runtime.models_missing_outcome());
                continue;
            }
            EngineLoad::Failed => {
                let reason = runtime
                    .engine_failure_reason()
                    .unwrap_or_else(|| "the OCR engine could not be created".to_string());
                fail_next_queued(&runtime, failure(503, "engine_unavailable", reason));
                continue;
            }
        }
        let Some(scheduled) = next_scheduled(&runtime) else {
            continue;
        };
        // M3：原图留在保留区里（`/annotated.png` 还要用它），这里只克隆引用。
        let Some((bytes, max_side)) = runtime.retained_ocr(&scheduled.id) else {
            // 只有取消能走到这里；防御性记录，绝不把任务永久留在 `Running`。
            runtime.finish(
                &scheduled.id,
                failure(500, "internal", "the queued image is no longer available"),
            );
            continue;
        };
        let outcome = recognize(&runtime, &scheduled.id, bytes, max_side, scheduled.class);
        runtime.finish(&scheduled.id, outcome);
    }
}

/// 引擎建不起来时，把下一个排队任务按同一份错误映射结算（不执行推理）。
fn fail_next_queued(runtime: &ServeShared, outcome: Outcome) {
    let Some(scheduled) = next_scheduled(runtime) else {
        return;
    };
    runtime.finish(&scheduled.id, outcome);
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

/// 在**不取任务**的前提下等一会儿（`has_queued_work` 与 `next_scheduled` 之间的那段空闲）。
fn wait_for_work(runtime: &ServeShared) {
    let state = lock(&runtime.jobs);
    if !state.scheduler.is_empty() || runtime.is_shutting_down() {
        return;
    }
    let _ = runtime.queue_signal.wait_timeout(state, WORKER_POLL);
}

/// 一次识别：按队列类别组装请求（**唯一的**管线选择点）→ 锁引擎 → 推理 → 有界序列化（§4.6）。
///
/// 队列类别**就是**管线选择（§8.3：双队列正是"普通 OCR / 公式 OCR"两条管线）：请求里没有
/// 第二个可能与之矛盾的开关。公式队列的 `FormulaPolicy` 全部由模型集与启动期解析给出
/// （识别模型路径 + 集合声明的 SHA-256 + 检测模型路径），请求体里不允许出现任何路径。
fn recognize(
    runtime: &ServeShared,
    job_id: &str,
    bytes: Arc<[u8]>,
    max_side: Option<u32>,
    class: QueueClass,
) -> Outcome {
    let formula = match class {
        QueueClass::Text => FormulaPolicy::default(),
        QueueClass::Formula => match runtime.formula_policy() {
            Some(policy) => policy,
            // 路由可用却解析不到公式识别模型：模型集在运行期被改动了。
            // 用与 `/api/models` 同源同值的 409 如实失败，而不是静默按文本处理。
            None => return Outcome::Failed(409, runtime.formula_missing_failure()),
        },
    };
    let request = text_request(bytes, max_side, formula);

    let output = match recognize_with(runtime, request) {
        Ok(output) => output,
        Err(error) => {
            if error.status_code() == 500 {
                eprintln!(
                    "serve: recognition failed for job {job_id}: {}",
                    error.message()
                );
            }
            return Outcome::Failed(error.status_code(), error.body());
        }
    };
    serialize_output(output, runtime.limits.max_result_bytes)
}

/// 一次请求的输入 → [`OcrRequest`]（**唯一**实现：OCR worker 的两条管线与
/// `POST /api/evaluate` 的批量路径共用）。
///
/// `stages`/`preprocess`/`detection`/`recognition`/`output` 都是内建默认值：serve 的输入面
/// 只有图像字节、`?max_side=` 与队列类别（§4.2），没有第二个能改变管线形状的请求参数。
pub(super) fn text_request(
    bytes: Arc<[u8]>,
    max_side: Option<u32>,
    formula: FormulaPolicy,
) -> OcrRequest {
    OcrRequest {
        input: ImageInput::Encoded(bytes),
        roi: None,
        scale_hint: None,
        stages: StagePlan::default(),
        preprocess: PreprocessPolicy {
            max_side,
            ..PreprocessPolicy::default()
        },
        detection: DetectionPolicy::default(),
        recognition: RecognitionPolicy {
            words: WordOutputMode::Off,
        },
        output: OutputPolicy::default(),
        formula,
    }
}

/// 锁引擎并推理（**唯一**的引擎调用点：OCR worker 与 `POST /api/evaluate` 共用）。
///
/// 引擎锁只在一次推理期间持有；`ensure_engine_loaded` 的加载/切换路径按
/// `engine_load → engine` 的顺序取锁，因此这里不会与它们交错。
pub(super) fn recognize_with(
    runtime: &ServeShared,
    request: OcrRequest,
) -> Result<OcrOutput, ServeError> {
    let mut engine = lock(&runtime.engine);
    let Some(backend) = engine.as_mut() else {
        return Err(ServeError::EngineUnavailable {
            reason: "the OCR engine is not loaded".to_string(),
        });
    };
    backend.recognize(request).map_err(ServeError::from)
}

/// `OcrOutput` → 有界 JSON（§4.6：超限即中止，绝不先建大 `String`）。
///
/// 字段由 [`super::export::result_json`] 给出（库的 `to_output_json` + `plain_text` +
/// `timing_ledger`），因此 worker 在这里测得的字节数就是 `/result` 与
/// `export?format=json` 的字节数。**保留下来的是结构化结果**（`Arc<OcrOutput>`），
/// 三个格式的导出因此都能用库的渲染器，而不是解析 JSON 拼第二套实现。
fn serialize_output(output: OcrOutput, limit: u64) -> Outcome {
    let value = match export::result_json(&output) {
        Ok(value) => value,
        Err(error) => {
            let serve_error = ServeError::from(error);
            return Outcome::Failed(serve_error.status_code(), serve_error.body());
        }
    };
    match serialize_bounded(&value, limit) {
        Ok(bytes) => {
            let serialized_bytes = bytes.len() as u64;
            Outcome::Succeeded(Succeeded::new(Arc::new(output), serialized_bytes))
        }
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
            // 先落成保留策略刚释放的那批（M3 的字节预算），再按"任务还在不在"收尾。
            state.sync_originals();
            let before = state.originals.len();
            state.originals.retain(|id, _| live.contains(id));
            let dropped_originals = before - state.originals.len();
            if dropped_results > 0 || dropped_originals > 0 {
                eprintln!(
                    "serve: dropped {dropped_results} result(s) and {dropped_originals} retained \
                     original(s)"
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

/// `/api/models` 的 `verification` 块：**哈希校验的成本账**。
///
/// `/api/models` 对每个文件都用库的**身份键控校验缓存**（键 = 路径 + 体积 + mtime）：
/// 首次见到某个身份时真的读盘哈希（566 MB 公式模型约 1 s），命中只花一次 `stat`。
/// 页面每 8 s 轮询一次就绪状态，因此这里把三件事都说清楚：
///
/// - `cold_this_call`：**这一份报告**真的重算了几个文件的摘要（稳态下必须是 0）；
/// - `cold_verifications`/`cache_hits`：进程启动以来的累计；
/// - `last_cold_ms`/`last_cold_bytes`：最近一次冷验证的实测耗时与被读的字节数。
///
/// 读者不必相信一句"已缓存"：`cold_this_call == 0` 就是"这次没有读那 566 MB"的证据。
fn verification_json(cold_this_call: usize) -> Value {
    let stats = verification_stats();
    json!({
        "identity": "path + size + mtime",
        "cold_this_call": cold_this_call,
        "cold_verifications": stats.cold_verifications,
        "cache_hits": stats.cache_hits,
        "entries": stats.entries,
        "last_cold_ms": stats.last_cold_ms(),
        "last_cold_bytes": stats.last_cold_bytes,
        "residual_blind_spot": "a same-size, same-mtime content swap is not detected by the cache",
    })
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

    /// M1 的路由语义在 M4 的形态下仍然成立：公式路由**关闭**时 `queue=formula` 是 400
    /// （不静默降级成文本）；**打开**时它真的进公式队列。
    ///
    /// M4 新增的是"谁决定打开"：`routing_for` 只在配置了页面公式检测模型时打开，
    /// 并把**不开的理由**带进 `/api/models` 的文字说明。
    #[test]
    fn the_routing_refuses_to_silently_use_the_formula_queue() {
        let disabled = OcrRouting::text_only("test: no detector");
        assert_eq!(disabled.class_for(None).expect("text"), QueueClass::Text);
        assert_eq!(
            disabled.class_for(Some("text")).expect("text"),
            QueueClass::Text
        );
        assert!(matches!(
            disabled.class_for(Some("formula")),
            Err(ServeError::BadRequest)
        ));
        assert!(matches!(
            disabled.class_for(Some("bogus")),
            Err(ServeError::BadRequest)
        ));
        assert_eq!(
            disabled.disabled_reason.as_deref(),
            Some("test: no detector"),
            "the page needs a textual reason, not just a disabled control"
        );

        let enabled = OcrRouting::formula_enabled();
        assert_eq!(
            enabled.class_for(Some("formula")).expect("formula"),
            QueueClass::Formula
        );
        assert_eq!(enabled.disabled_reason, None);
        assert_eq!(enabled.class_for(None).expect("text"), QueueClass::Text);
    }

    /// 公式路由的启动期判据（**唯一**）：有检测模型才打开，否则给出可定位理由。
    #[test]
    fn the_formula_routing_is_decided_by_the_detector_alone() {
        let enabled = super::routing_for(Some(Path::new("D:\\m\\pix2text-mfd-1.5.onnx")));
        assert!(enabled.formula);
        assert_eq!(enabled.disabled_reason, None);

        let disabled = super::routing_for(None);
        assert!(!disabled.formula);
        let reason = disabled.disabled_reason.expect("a textual reason");
        assert!(reason.contains("--formula-detector"), "{reason}");
        assert!(reason.contains("ordinary OCR is unaffected"), "{reason}");
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
