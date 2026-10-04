//! 任务存储：有界保留 + TTL + tombstone（§4.3、§4.5、§8.4）。
//!
//! # 语义
//!
//! - 任务状态：`Queued → Running → Succeeded | Failed | Cancelled`（§4.3）；
//! - 取消：`Queued → Cancelled` **可靠**；`Running` 返回 `NotCancellable`
//!   （M1 不中断推理，**不得**假装取消成功）；终态再取消同样是 `NotCancellable`，
//!   只是 `detail.reason` 为 `job_finished`；
//! - 保留上限：终态任务按"**最旧终态优先**"淘汰，数量与字节**双重**上限；
//! - TTL：由显式 [`JobStore::tick`] 驱动（**不**依赖访问触发，也不睡眠），
//!   因此可以在单测里用注入的时间完整验证；
//! - **tombstone**：淘汰时把 `job_id → evicted_at` 写入有界 FIFO 表，
//!   它有自己的数量上限与 TTL；查询因此能区分
//!   `JobNotFound`(404) 与 `JobEvicted`(410)——这是 410 能成立的前提；
//! - 字节账本：只统计**保留中的原件（编码图片字节）与结果**，原始 `RecImage`
//!   不长期保留（§4.5 的"原图保留"一行）。
//!
//! 活跃任务**永不**被淘汰：淘汰只作用于终态任务，因此字节上限可能被活跃任务短暂超过，
//! 但活跃任务的总量由队列容量与 `--max-body-mb` 双重约束，是有界的。

use std::collections::{BTreeMap, VecDeque};

use serde::Serialize;

use super::error::{ErrorBody, ServeError};
use super::limits::{ServeConfigError, ServeLimits, require_positive_u64, require_positive_usize};
use super::queue::QueueClass;

/// 时间口径：毫秒，由调用方注入（M1 用进程启动以来的单调时钟）。
///
/// 存储内部**不**读时钟，因此 TTL、`elapsed_ms` 都能在单测里确定性地验证。
pub type Millis = u64;

/// 任务类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Ocr,
    /// 模型下载任务（与 OCR 队列分离，§8.1）。
    ModelDownload,
}

impl JobKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ocr => "ocr",
            Self::ModelDownload => "model_download",
        }
    }
}

/// 任务所属的**队列**（§4.2 的 `queue` 字段）。
///
/// # 为什么它不是调度器的 [`QueueClass`]
///
/// M1 让下载任务借用 `QueueClass::Text`（`JobStore` 要一个类别）并把 `position` 恒置为
/// `None`，理由是"它不会出现在任何队列里"。这个理由本身对，但**类型**是错的：调度器只
/// 服务双队列（Text/Formula），而下载任务走 §8.1 的**独立有界 channel**，从不进调度器。
/// 把它们混成一个类型，就会让 `/api/jobs/{id}` 与任何按 `queue` 聚合的诊断报告一个从未
/// 排队的下载任务属于**文本队列**——一条不真实的诊断。因此这里给出中性类别
/// [`Self::Download`]，并把"某个任务是否在双队列里"表达成 [`Self::class`] 的返回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobQueue {
    Text,
    Formula,
    /// 独立下载 channel（§8.1），不属于双队列。
    Download,
}

impl JobQueue {
    /// 机器可读名称（进 `/api/jobs/{id}` 的 `queue` 字段与日志）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Formula => "formula",
            Self::Download => "download",
        }
    }

    /// 它在双队列调度器里的类别；下载任务**没有**（返回 `None`）。
    pub fn class(self) -> Option<QueueClass> {
        match self {
            Self::Text => Some(QueueClass::Text),
            Self::Formula => Some(QueueClass::Formula),
            Self::Download => None,
        }
    }

    pub fn from_class(class: QueueClass) -> Self {
        match class {
            QueueClass::Text => Self::Text,
            QueueClass::Formula => Self::Formula,
        }
    }
}

/// 任务状态（§4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// 是否终态（终态任务才参与 TTL 与保留上限）。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// 任务存储的上限（来自 §3 的 `--max-retained` / `--max-retained-mb` /
/// `--max-tombstones` / `--job-ttl-secs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobStoreLimits {
    pub max_retained: usize,
    pub max_retained_bytes: u64,
    pub max_tombstones: usize,
    pub ttl_ms: Millis,
}

impl JobStoreLimits {
    /// 校验取值：全部必须 ≥ 1（0 会让 §4.5 的语义失效）。
    pub fn new(
        max_retained: usize,
        max_retained_bytes: u64,
        max_tombstones: usize,
        ttl_ms: Millis,
    ) -> Result<Self, ServeConfigError> {
        Ok(Self {
            max_retained: require_positive_usize("--max-retained", max_retained)?,
            max_retained_bytes: require_positive_u64("--max-retained-mb", max_retained_bytes)?,
            max_tombstones: require_positive_usize("--max-tombstones", max_tombstones)?,
            ttl_ms: require_positive_u64("--job-ttl-secs", ttl_ms)?,
        })
    }

    /// 从**已经校验过**的 [`ServeLimits`] 构造。
    ///
    /// 仍然走 [`Self::new`]：四个上限的校验只有一份实现（`ServeLimits` 的校验已经保证了
    /// 取值 > 0，因此这里的 `expect` 是"同一套规则不会被绕过"的编译期可见断言）。
    pub fn from_limits(limits: &ServeLimits) -> Self {
        Self::new(
            limits.max_retained,
            limits.max_retained_bytes,
            limits.max_tombstones,
            limits.job_ttl_ms,
        )
        .expect("ServeLimits validates the same four bounds")
    }
}

/// 下载任务的进度（§4.3 的作业形状：逐文件 done/total、字节 done/total、当前文件名）。
///
/// `files_total` / `bytes_total` 只统计**需要下载**的文件（缺失 ∪ 损坏，即"不是
/// `Present`"的那些）；已经校验通过的文件既不需要网络也不需要空间，因此不参与进度。
/// `bytes_total` 为 `None` 表示至少有一个待下载文件没有声明体积（§6.2 的流式上限兜底）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DownloadProgress {
    /// 已通过 SHA-256 校验并落盘的文件数。
    pub files_done: usize,
    /// 本次任务要下载的文件数。
    pub files_total: usize,
    /// 已写入的字节数（含当前文件正在写入的部分）。
    pub bytes_done: u64,
    /// 待下载文件的体积之和；有未知体积时为 `null`。
    pub bytes_total: Option<u64>,
    /// 正在下载的文件名（没有进行中的文件时为 `null`）。
    pub current_file: Option<String>,
}

impl DownloadProgress {
    /// 任务刚创建（或刚开始执行）时的进度：一个文件都还没完成。
    pub fn planned(files_total: usize, bytes_total: Option<u64>) -> Self {
        Self {
            files_done: 0,
            files_total,
            bytes_done: 0,
            bytes_total,
            current_file: None,
        }
    }
}

/// 失败任务的机器可读分类。
///
/// `status`/`code`/`detail` 与 [`ServeError`] 的 HTTP 映射**同源**：同一个失败原因不会
/// 在 `/api/jobs/{id}` 里被压成一句人类可读文本（那会让客户端只能做字符串匹配）。
/// `message` 同时进 §4.2 冻结的 `error` 字段，因此旧客户端不受影响。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobFailure {
    /// 同一失败在 `/result` 上重放（或同步返回）时的 HTTP 状态码。
    pub status: u16,
    /// §11.1 的机器可读 `code`。
    pub code: &'static str,
    /// 人类可读说明（= `JobView.error`）。
    pub message: String,
    /// `detail` 载荷（例如 `TooLarge` 的 `limit_bytes`/`observed_bytes`）。
    pub detail: serde_json::Value,
}

impl JobFailure {
    pub fn new(
        status: u16,
        code: &'static str,
        message: impl Into<String>,
        detail: serde_json::Value,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            detail,
        }
    }

    /// 由 HTTP 错误映射构造（`Outcome::Failed` 与 `ServeError` 两条来源共用一份实现）。
    pub fn from_body(status: u16, body: &ErrorBody) -> Self {
        Self {
            status,
            code: body.code,
            message: body.message.clone(),
            detail: body.detail.clone(),
        }
    }
}

impl From<&ServeError> for JobFailure {
    fn from(error: &ServeError) -> Self {
        Self {
            status: error.status_code(),
            code: error.code(),
            message: error.message(),
            detail: error.detail(),
        }
    }
}

/// 任务记录（存储内部形态）。
#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub id: String,
    pub kind: JobKind,
    /// 所属队列（下载任务用中性的 [`JobQueue::Download`]，见该类型的文档）。
    pub queue: JobQueue,
    pub state: JobState,
    /// 队列内位置（仅 `Queued` 且属于双队列时有意义）。
    pub position: Option<usize>,
    /// 进入队列的时刻。
    pub queued_ms: Millis,
    /// 开始执行的时刻。
    pub started_ms: Option<Millis>,
    /// 进入终态的时刻。
    pub finished_ms: Option<Millis>,
    /// 保留的**原件**字节数（编码后的图片，不保留解码结果）。
    pub original_bytes: u64,
    /// 原件编码字节**当前是否还在**保留区里（M3）。
    ///
    /// `original_bytes == 0`（下载任务，或空请求体）时为 `false`：那些任务根本没有原图。
    /// 由保留预算释放后也会变成 `false`——此时 `/api/jobs/{id}/annotated.png` 是
    /// **410 `original_evicted`**，而任务与结果仍然可查（§4.2 与 §4.5 的区分）。
    pub original_retained: bool,
    /// 保留的**结果**字节数（序列化后的结果）。
    pub result_bytes: u64,
    /// 失败分类（`Failed` 时存在；`Failed` 之外的终态为 `None`）。
    pub failure: Option<JobFailure>,
    /// 下载进度（`kind = ModelDownload` 时存在）。
    pub download: Option<DownloadProgress>,
    /// 是否已经收到取消请求（运行中的下载任务在**文件边界**兑现它，§6.6）。
    pub cancel_requested: bool,
    /// 插入序号：同一时刻结束的任务用它做稳定排序。
    pub seq: u64,
    /// 创建任务的 HTTP 请求的流动日志 id（M5，仅用于日志关联；不进 `/api/jobs/{id}`）。
    pub request_id: u64,
}

impl JobRecord {
    /// 该任务占用的保留字节（原件 + 结果）。原件被释放后只算结果。
    pub fn total_bytes(&self) -> u64 {
        let original = if self.original_retained {
            self.original_bytes
        } else {
            0
        };
        original.saturating_add(self.result_bytes)
    }

    /// 已耗时：从开始执行（未开始则从入队）到结束（未结束则到 `now`）。
    pub fn elapsed_ms(&self, now: Millis) -> Millis {
        let end = self.finished_ms.unwrap_or(now);
        end.saturating_sub(self.started_ms.unwrap_or(self.queued_ms))
    }

    /// 失败原因的人类可读文本（进 §4.2 冻结的 `error` 字段）。
    pub fn error(&self) -> Option<String> {
        self.failure.as_ref().map(|failure| failure.message.clone())
    }

    /// `/api/jobs/{id}` 的响应视图（字段顺序与 §4.2 一致，新增字段在该表之后）。
    pub fn view(&self, now: Millis) -> JobView {
        JobView {
            id: self.id.clone(),
            kind: self.kind,
            queue: self.queue,
            state: self.state,
            position: self.position,
            queued_ms: self.queued_ms,
            started_ms: self.started_ms,
            elapsed_ms: self.elapsed_ms(now),
            error: self.error(),
            failure: self.failure.clone(),
            download: self.download.clone(),
            cancel_requested: self.cancel_requested,
            original_retained: self.original_retained,
        }
    }
}

/// `/api/jobs/{id}` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobView {
    pub id: String,
    pub kind: JobKind,
    pub queue: JobQueue,
    pub state: JobState,
    pub position: Option<usize>,
    pub queued_ms: Millis,
    pub started_ms: Option<Millis>,
    pub elapsed_ms: Millis,
    pub error: Option<String>,
    /// 机器可读的失败分类（成功/未完成时为 `null`）。
    pub failure: Option<JobFailure>,
    /// 下载进度（仅 `kind = model_download` 时非 `null`）。
    pub download: Option<DownloadProgress>,
    /// 已登记取消请求（运行中的下载任务会在文件边界兑现，§6.6）。
    pub cancel_requested: bool,
    /// 原图编码字节是否仍在保留区里（M3）。为 `false` 时 `/annotated.png` 是 410
    /// `original_evicted`；`/result` 不受影响。
    pub original_retained: bool,
}

/// 取消的结论（§4.3 与 §6.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 排队中：立即取消，**可靠**（§4.3）。
    Cancelled,
    /// 运行中的**下载**任务：已登记取消请求，将在**文件边界**生效（§6.6）。
    ///
    /// 任务状态此刻**不变**（仍是 `running`）——M1 的 OCR 任务在这里返回 409
    /// `not_cancellable`，因为推理不可中断；下载不假装"已取消"，但也不假装"不能取消"。
    CancelRequested,
}

/// 一次 [`JobStore::tick`] 的结果（诊断与测试用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// 因 TTL 到期被淘汰的终态任务。
    pub expired_jobs: usize,
    /// 因数量上限被淘汰的终态任务。
    pub evicted_for_count: usize,
    /// 因字节上限被淘汰的终态任务。
    pub evicted_for_bytes: usize,
    /// 因 TTL 到期被移除的 tombstone。
    pub expired_tombstones: usize,
    /// 因字节上限被**释放原图**（任务与结果保留）的数量（M3）。
    pub released_originals: usize,
}

/// 任务 ID 生成器。
///
/// ID 是"进程内单调序号"，**不是**安全令牌：不可猜测性由 §7.2 的 token 提供
/// （所有 `/api/*` 都需要 token），单调序号让日志、tombstone 与排序都可读可复现。
#[derive(Debug, Default)]
pub struct JobIdGenerator {
    next: u64,
}

impl JobIdGenerator {
    pub fn generate(&mut self) -> String {
        let seq = self.next;
        self.next += 1;
        format!("job-{seq:016x}")
    }
}

/// 有界任务存储。
#[derive(Debug)]
pub struct JobStore {
    limits: JobStoreLimits,
    jobs: BTreeMap<String, JobRecord>,
    /// FIFO tombstone 表：`(job_id, evicted_at)`，越界时淘汰最旧的。
    tombstones: VecDeque<(String, Millis)>,
    retained_bytes: u64,
    next_seq: u64,
    /// 原图刚被释放的任务 id（等待调用方把字节真丢掉，见 [`Self::take_released_originals`]）。
    released_originals: Vec<String>,
}

impl JobStore {
    pub fn new(limits: JobStoreLimits) -> Self {
        Self {
            limits,
            jobs: BTreeMap::new(),
            tombstones: VecDeque::new(),
            retained_bytes: 0,
            next_seq: 0,
            released_originals: Vec::new(),
        }
    }

    pub fn limits(&self) -> JobStoreLimits {
        self.limits
    }

    /// 建任务（`Queued`）。`original_bytes` 是要保留的编码图片字节数。
    ///
    /// `request_id` 是**创建这个任务的那个 HTTP 请求**的流动日志 id（M5）：它随任务记录
    /// 一起存活，因此 worker 在 `running`/终态那两行里能写出同一个 id 的日志——
    /// "一次上传"因此可以从请求行一路追到任务终态，而不需要把 id 穿进调度器。
    /// 它**不进** `/api/jobs/{id}` 的响应（那是冻结契约，不因为日志需要一个内部字段而改变）。
    pub fn insert(
        &mut self,
        id: impl Into<String>,
        kind: JobKind,
        queue: JobQueue,
        original_bytes: u64,
        now: Millis,
        request_id: u64,
    ) -> Result<(), ServeError> {
        let id = id.into();
        if self.jobs.contains_key(&id) {
            // 重复 ID 是程序错误（生成器坏了），不能静默覆盖已有任务。
            return Err(ServeError::Internal);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let record = JobRecord {
            id: id.clone(),
            kind,
            queue,
            state: JobState::Queued,
            position: None,
            queued_ms: now,
            started_ms: None,
            finished_ms: None,
            original_bytes,
            original_retained: original_bytes > 0,
            result_bytes: 0,
            failure: None,
            download: None,
            cancel_requested: false,
            seq,
            request_id,
        };
        self.retained_bytes = self.retained_bytes.saturating_add(record.total_bytes());
        self.jobs.insert(id, record);
        // 新任务带来的字节可能已经越过保留上限：立即按"最旧终态优先"回收。
        self.enforce_retention(now);
        Ok(())
    }

    /// 更新队列内位置（调度器每次取任务后 M1 重新计算）。
    ///
    /// 不属于双队列的任务（下载）**没有**位置：调用方用 [`JobQueue::class`] 过滤。
    pub fn set_position(&mut self, id: &str, position: Option<usize>) -> Result<(), ServeError> {
        self.record_mut(id)?.position = position;
        Ok(())
    }

    /// 登记/刷新下载进度（§4.3 的作业形状）。
    pub fn set_download_progress(
        &mut self,
        id: &str,
        progress: DownloadProgress,
    ) -> Result<(), ServeError> {
        let record = self.record_mut(id)?;
        if record.kind != JobKind::ModelDownload {
            // 给 OCR 任务写下载进度是程序错误：不能静默接受一个不可能的字段组合。
            return Err(ServeError::Internal);
        }
        record.download = Some(progress);
        Ok(())
    }

    /// 是否已经收到取消请求（下载 worker 在**文件边界**查询它，§6.6）。
    ///
    /// 未知/已淘汰的任务返回 `false`：调用方随后会从状态转换里得到结论，
    /// 这里不制造第二个错误来源。
    pub fn is_cancel_requested(&self, id: &str) -> bool {
        self.jobs
            .get(id)
            .is_some_and(|record| record.cancel_requested)
    }

    /// `Queued → Running`。
    pub fn start(&mut self, id: &str, now: Millis) -> Result<(), ServeError> {
        let record = self.record_mut(id)?;
        if record.state != JobState::Queued {
            return Err(ServeError::Internal);
        }
        record.state = JobState::Running;
        record.started_ms = Some(now);
        record.position = None;
        Ok(())
    }

    /// `Running → Succeeded`，并登记保留的结果字节。
    pub fn succeed(&mut self, id: &str, result_bytes: u64, now: Millis) -> Result<(), ServeError> {
        {
            let record = self.record_mut(id)?;
            if record.state != JobState::Running {
                return Err(ServeError::Internal);
            }
            let added = result_bytes.saturating_sub(record.result_bytes);
            record.result_bytes = result_bytes;
            record.state = JobState::Succeeded;
            record.finished_ms = Some(now);
            record.position = None;
            self.retained_bytes = self.retained_bytes.saturating_add(added);
        }
        self.enforce_retention(now);
        Ok(())
    }

    /// `Running → Failed`，原因写入视图的 `error` 字段。
    ///
    /// 只带人类可读文本（`failure` 为 `None`）：需要机器可读分类的调用方用
    /// [`Self::fail_classified`]。
    pub fn fail(
        &mut self,
        id: &str,
        reason: impl Into<String>,
        now: Millis,
    ) -> Result<(), ServeError> {
        self.fail_classified(
            id,
            JobFailure::new(500, "internal", reason, serde_json::Value::Null),
            now,
        )
    }

    /// `Running → Failed`，同时登记机器可读的分类（`code`/`status`/`detail`）。
    ///
    /// 失败任务**永远不会**有注释图（没有 `OcrOutput` 就没有可用于叠加的区域），因此
    /// 这里同时释放它的原图编码字节（§4.5 的字节账本随之如实减少）。
    pub fn fail_classified(
        &mut self,
        id: &str,
        failure: JobFailure,
        now: Millis,
    ) -> Result<(), ServeError> {
        {
            let record = self.record_mut(id)?;
            if record.state != JobState::Running {
                return Err(ServeError::Internal);
            }
            record.state = JobState::Failed;
            record.failure = Some(failure);
            record.finished_ms = Some(now);
            record.position = None;
        }
        self.release_original(id);
        self.enforce_retention(now);
        Ok(())
    }

    /// 取消（§4.3 与 §6.6）。
    ///
    /// - `Queued`（任何类型）→ **立即** `Cancelled`，返回 [`CancelOutcome::Cancelled`]；
    /// - `Running` 且是**下载**任务 → 只登记取消请求（[`CancelOutcome::CancelRequested`]），
    ///   状态**不变**：§6.6 的取消只在文件边界生效，worker 随后用
    ///   [`Self::finish_cancelled`] 落地；
    /// - `Running` 的 OCR 任务与全部终态 → [`ServeError::NotCancellable`]（409），状态不变。
    pub fn cancel(&mut self, id: &str, now: Millis) -> Result<CancelOutcome, ServeError> {
        // 先把两个字段拷出来：下面要按"状态 + 类型"分支，并且分支里会再可变借用存储
        // （释放原图 / 登记取消请求），因此不能把 `record` 的借用带进 match 的臂里。
        let (state, kind) = {
            let record = self.record_mut(id)?;
            (record.state, record.kind)
        };
        match (state, kind) {
            (JobState::Queued, _) => {
                {
                    let record = self.record_mut(id)?;
                    record.state = JobState::Cancelled;
                    record.finished_ms = Some(now);
                }
                // 取消的任务不会有注释图：原图随取消一起释放（字节账本同步）。
                self.release_original(id);
                self.enforce_retention(now);
                Ok(CancelOutcome::Cancelled)
            }
            (JobState::Running, JobKind::ModelDownload) => {
                self.record_mut(id)?.cancel_requested = true;
                Ok(CancelOutcome::CancelRequested)
            }
            (JobState::Running, _) => Err(ServeError::NotCancellable),
            (JobState::Succeeded | JobState::Failed | JobState::Cancelled, _) => {
                Err(ServeError::NotCancellable)
            }
        }
    }

    /// 下载 worker 专用：把**已经登记取消请求**的运行中下载任务落地为 `Cancelled`
    /// （§4.3 的 `running → cancelled` 边；M1 没有它的生产者，因为推理不可中断）。
    pub fn finish_cancelled(&mut self, id: &str, now: Millis) -> Result<(), ServeError> {
        {
            let record = self.record_mut(id)?;
            if record.state != JobState::Running || !record.cancel_requested {
                return Err(ServeError::Internal);
            }
            record.state = JobState::Cancelled;
            record.finished_ms = Some(now);
            record.position = None;
        }
        self.release_original(id);
        self.enforce_retention(now);
        Ok(())
    }

    /// 查询任务：`JobNotFound`(404) / `JobEvicted`(410) / 记录本身。
    pub fn record(&self, id: &str) -> Result<&JobRecord, ServeError> {
        if let Some(record) = self.jobs.get(id) {
            return Ok(record);
        }
        if self.is_tombstoned(id) {
            return Err(ServeError::JobEvicted);
        }
        Err(ServeError::JobNotFound)
    }

    /// `/api/jobs/{id}` 的视图。
    pub fn view(&self, id: &str, now: Millis) -> Result<JobView, ServeError> {
        Ok(self.record(id)?.view(now))
    }

    /// 淘汰时刻（在 tombstone 表内时）。
    pub fn evicted_at(&self, id: &str) -> Option<Millis> {
        self.tombstones
            .iter()
            .find(|(tombstoned, _)| tombstoned == id)
            .map(|(_, at)| *at)
    }

    pub fn is_tombstoned(&self, id: &str) -> bool {
        self.evicted_at(id).is_some()
    }

    /// 显式 TTL 清理（§4.5：由后台线程驱动，不依赖访问触发）。
    ///
    /// 顺序：先按 TTL 淘汰终态任务（写 tombstone），再清理过期 tombstone，
    /// 最后执行数量/字节上限。
    pub fn tick(&mut self, now: Millis) -> TickReport {
        let mut report = TickReport::default();

        let expired: Vec<String> = self
            .jobs
            .values()
            .filter(|record| record.state.is_terminal())
            .filter(|record| {
                record
                    .finished_ms
                    .is_some_and(|finished| finished.saturating_add(self.limits.ttl_ms) <= now)
            })
            .map(|record| record.id.clone())
            .collect();
        for id in expired {
            self.evict(&id, now);
            report.expired_jobs += 1;
        }

        // tombstone 表按 `evicted_at` 单调，因此可以只从队首清理。
        while self
            .tombstones
            .front()
            .is_some_and(|(_, at)| at.saturating_add(self.limits.ttl_ms) <= now)
        {
            self.tombstones.pop_front();
            report.expired_tombstones += 1;
        }

        let retention = self.enforce_retention(now);
        report.evicted_for_count += retention.evicted_for_count;
        report.evicted_for_bytes += retention.evicted_for_bytes;
        report.released_originals += retention.released_originals;
        report
    }

    /// 保留中的总字节数（原件 + 结果）。
    pub fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// 仍在存储里的任务数（含活跃与终态）。
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// 终态任务数（保留上限统计的对象）。
    pub fn terminal_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|record| record.state.is_terminal())
            .count()
    }

    /// tombstone 表长度。
    pub fn tombstone_len(&self) -> usize {
        self.tombstones.len()
    }

    /// 所有任务的视图，按插入顺序（诊断用）。
    pub fn views(&self, now: Millis) -> Vec<JobView> {
        self.jobs.values().map(|record| record.view(now)).collect()
    }

    fn record_mut(&mut self, id: &str) -> Result<&mut JobRecord, ServeError> {
        if self.jobs.contains_key(id) {
            return Ok(self
                .jobs
                .get_mut(id)
                .expect("checked with contains_key just above"));
        }
        if self.is_tombstoned(id) {
            return Err(ServeError::JobEvicted);
        }
        Err(ServeError::JobNotFound)
    }

    /// 释放某个任务的原图编码字节（记账同步）。
    ///
    /// 返回 `true` 表示这次调用真的释放了；调用方**必须**去把那份字节丢掉
    /// （[`Self::take_released_originals`] 会给出 id）。
    ///
    /// 它有两个调用者，语义都是"这份原图再也用不上了"：
    /// 1. 终态且**不可能**再有注释图的路径（失败/取消，见 `fail_classified` 与 `cancel`）；
    /// 2. 字节预算压力下的保留策略（[`Self::enforce_retention`]）。
    pub fn release_original(&mut self, id: &str) -> bool {
        let Some(record) = self.jobs.get_mut(id) else {
            return false;
        };
        if !record.original_retained || record.original_bytes == 0 {
            return false;
        }
        record.original_retained = false;
        let released = record.original_bytes;
        self.retained_bytes = self.retained_bytes.saturating_sub(released);
        self.released_originals.push(id.to_string());
        true
    }

    /// 取走"原图刚被释放"的任务 id（调用方据此丢弃字节；取走后列表清空）。
    pub fn take_released_originals(&mut self) -> Vec<String> {
        std::mem::take(&mut self.released_originals)
    }

    fn evict(&mut self, id: &str, now: Millis) {
        if let Some(record) = self.jobs.remove(id) {
            self.retained_bytes = self.retained_bytes.saturating_sub(record.total_bytes());
            self.push_tombstone(id.to_string(), now);
        }
    }

    fn push_tombstone(&mut self, id: String, now: Millis) {
        self.tombstones.push_back((id, now));
        while self.tombstones.len() > self.limits.max_tombstones {
            self.tombstones.pop_front();
        }
    }

    /// 数量与字节双重上限。
    ///
    /// **数量上限**只能靠淘汰整个终态任务解决（最旧终态优先）。
    ///
    /// **字节上限**分两步（M3 明确下来的顺序，见 `docs/06` 的 M3 记录）：
    /// 1. 先释放**最旧终态任务**的原图编码字节——结果（通常小一个数量级）与任务记录都留下，
    ///    `/result` 与 `/api/jobs/{id}` 照常可用，只有 `/annotated.png` 变成
    ///    **410 `original_evicted`**（§4.2 冻结的结论；没有这一步该 `code` 永远不可达）；
    /// 2. 原图都释放完仍然超限 → 才淘汰整个终态任务（M0c 起的原有行为）。
    ///
    /// 活跃任务（`queued`/`running`）**永不**释放、也永不淘汰：它们的字节由队列容量与
    /// `--max-body-mb` 约束，而 worker 还需要那份原图去识别。
    fn enforce_retention(&mut self, now: Millis) -> RetentionReport {
        let mut report = RetentionReport::default();

        // 1) 数量上限：最旧终态优先。
        while self.terminal_count() > self.limits.max_retained {
            let Some(oldest) = self.oldest_terminal() else {
                break;
            };
            self.evict(&oldest, now);
            report.evicted_for_count += 1;
        }

        // 2) 字节上限：先释放原图，再淘汰整个任务。
        while self.retained_bytes > self.limits.max_retained_bytes {
            if let Some(id) = self.oldest_terminal_with_original() {
                self.release_original(&id);
                report.released_originals += 1;
                continue;
            }
            let Some(oldest) = self.oldest_terminal() else {
                // 只剩活跃任务：它们的字节不由保留上限约束。
                break;
            };
            self.evict(&oldest, now);
            report.evicted_for_bytes += 1;
        }
        report
    }

    fn oldest_terminal(&self) -> Option<String> {
        self.jobs
            .values()
            .filter(|record| record.state.is_terminal())
            .min_by_key(|record| (record.finished_ms.unwrap_or(record.queued_ms), record.seq))
            .map(|record| record.id.clone())
    }

    /// 仍持有原图、且原图字节非零的最旧终态任务（见 [`Self::enforce_retention`]）。
    fn oldest_terminal_with_original(&self) -> Option<String> {
        self.jobs
            .values()
            .filter(|record| record.state.is_terminal())
            .filter(|record| record.original_retained && record.original_bytes > 0)
            .min_by_key(|record| (record.finished_ms.unwrap_or(record.queued_ms), record.seq))
            .map(|record| record.id.clone())
    }
}

/// [`JobStore::enforce_retention`] 的一轮结论（对 `TickReport` 可加）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RetentionReport {
    evicted_for_count: usize,
    evicted_for_bytes: usize,
    released_originals: usize,
}

#[cfg(test)]
mod tests {
    use super::{
        CancelOutcome, DownloadProgress, JobFailure, JobIdGenerator, JobKind, JobQueue, JobState,
        JobStore, JobStoreLimits, Millis, TickReport,
    };
    use crate::serve::error::ServeError;

    /// 测试用的流动日志请求 id（M5）：任务记录里的关联字段。
    ///
    /// 这些用例考察的是存储语义而不是日志关联，因此统一用一个固定值；"请求 id 与任务真的
    /// 对得上"的证据在 `serve::tests` 的端到端用例里（那里比对的是真实请求 id）。
    const TEST_REQUEST_ID: u64 = 1;

    fn limits(
        max_retained: usize,
        bytes: u64,
        tombstones: usize,
        ttl_ms: Millis,
    ) -> JobStoreLimits {
        JobStoreLimits::new(max_retained, bytes, tombstones, ttl_ms).expect("valid test limits")
    }

    fn store() -> JobStore {
        JobStore::new(limits(32, 64 * 1024 * 1024, 256, 600_000))
    }

    fn insert_ocr(store: &mut JobStore, id: &str, bytes: u64, now: Millis) {
        store
            .insert(
                id,
                JobKind::Ocr,
                JobQueue::Text,
                bytes,
                now,
                TEST_REQUEST_ID,
            )
            .expect("fresh ids");
    }

    fn finish(store: &mut JobStore, id: &str, now: Millis) {
        store.start(id, now).expect("queued -> running");
        store.succeed(id, 0, now).expect("running -> succeeded");
    }

    #[test]
    fn job_ids_are_monotonic_and_readable() {
        let mut generator = JobIdGenerator::default();
        let first = generator.generate();
        let second = generator.generate();
        assert_eq!(first, "job-0000000000000000");
        assert_eq!(second, "job-0000000000000001");
        assert_ne!(first, second);
    }

    #[test]
    fn job_store_limits_reject_zero() {
        assert!(JobStoreLimits::new(0, 1024, 256, 600_000).is_err());
        assert!(JobStoreLimits::new(32, 0, 256, 600_000).is_err());
        assert!(JobStoreLimits::new(32, 1024, 0, 600_000).is_err());
        assert!(JobStoreLimits::new(32, 1024, 256, 0).is_err());
        let error = JobStoreLimits::new(0, 1024, 256, 600_000).expect_err("zero is invalid");
        assert_eq!(error.field(), "--max-retained");
    }

    #[test]
    fn queued_to_running_to_succeeded_carries_every_documented_field() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 2048, 100);
        store.set_position("job-0", Some(0)).expect("queued");

        let view = store.view("job-0", 100).expect("present");
        assert_eq!(view.id, "job-0");
        assert_eq!(view.kind, JobKind::Ocr);
        assert_eq!(view.queue, JobQueue::Text);
        assert_eq!(view.state, JobState::Queued);
        assert_eq!(view.position, Some(0));
        assert_eq!(view.queued_ms, 100);
        assert_eq!(view.started_ms, None);
        assert_eq!(view.elapsed_ms, 0);
        assert_eq!(view.error, None);
        assert_eq!(view.failure, None);
        assert_eq!(view.download, None, "an OCR job has no download progress");
        assert!(!view.cancel_requested);

        store.start("job-0", 150).expect("queued -> running");
        let view = store.view("job-0", 400).expect("present");
        assert_eq!(view.state, JobState::Running);
        assert_eq!(view.position, None, "a running job leaves the queue");
        assert_eq!(view.started_ms, Some(150));
        assert_eq!(view.elapsed_ms, 250, "elapsed counts from the start");

        store
            .succeed("job-0", 512, 300)
            .expect("running -> succeeded");
        let view = store.view("job-0", 9_999).expect("present");
        assert_eq!(view.state, JobState::Succeeded);
        assert_eq!(view.elapsed_ms, 150, "elapsed freezes at the finish time");
    }

    #[test]
    fn a_failed_job_records_its_reason() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 16, 0);
        store.start("job-0", 1).expect("-> running");
        store
            .fail("job-0", "the image cannot be decoded", 2)
            .expect("-> failed");
        let view = store.view("job-0", 3).expect("present");
        assert_eq!(view.state, JobState::Failed);
        assert_eq!(view.error.as_deref(), Some("the image cannot be decoded"));
    }

    #[test]
    fn illegal_store_transitions_are_internal_errors() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 16, 0);
        // Queued -> Succeeded 必须被拒绝。
        assert!(matches!(
            store.succeed("job-0", 0, 1).expect_err("must reject"),
            ServeError::Internal
        ));
        // 重复插入同一 ID 也必须被拒绝。
        assert!(matches!(
            store
                .insert("job-0", JobKind::Ocr, JobQueue::Text, 0, 1, TEST_REQUEST_ID)
                .expect_err("duplicate id"),
            ServeError::Internal
        ));
        store.start("job-0", 1).expect("-> running");
        // Running -> Running 非法。
        assert!(matches!(
            store.start("job-0", 2).expect_err("must reject"),
            ServeError::Internal
        ));
    }

    /// §4.3：排队中的任务取消是**可靠**的。
    #[test]
    fn cancelling_a_queued_job_is_reliable() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 32, 10);
        let outcome = store.cancel("job-0", 20).expect("queued cancel must work");
        assert_eq!(outcome, CancelOutcome::Cancelled);
        let view = store.view("job-0", 30).expect("present");
        assert_eq!(view.state, JobState::Cancelled);
        assert_eq!(view.elapsed_ms, 10, "cancelling freezes the elapsed time");
        // 重复取消不再是"取消"，而是 409。
        let error = store.cancel("job-0", 40).expect_err("terminal cancel");
        assert_eq!(error.status_code(), 409);
        assert_eq!(error.code(), "not_cancellable");
    }

    /// §4.3：正在运行的任务**不可取消**，且状态不得被改动（不得假装取消成功）。
    #[test]
    fn cancelling_a_running_job_returns_not_cancellable_and_keeps_running() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 32, 0);
        store.start("job-0", 1).expect("-> running");
        let error = store
            .cancel("job-0", 2)
            .expect_err("running cancel must fail");
        assert_eq!(error.status_code(), 409);
        assert_eq!(error.code(), "not_cancellable");
        assert_eq!(
            store.record("job-0").expect("still present").state,
            JobState::Running,
            "the state must not change"
        );
    }

    /// 终态任务再取消：仍然是 409 `not_cancellable`，状态不变。
    ///
    /// **接缝**：§4.3 还给终态取消留了一个独立的 `job_finished` 文案，而 M0c 的
    /// `ServeError` 变体清单（§11.1）里没有能产生该 `code` 的变体，因此这里与
    /// "运行中不可取消"共用 `not_cancellable`（§11.1 的表格本身也只列了这三个 code）。
    /// M1 若要在 UI 上区分两者，需要新增变体或改用 `/api/jobs/{id}` 的 `state` 判断。
    #[test]
    fn cancelling_a_finished_job_is_also_not_cancellable() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 32, 0);
        finish(&mut store, "job-0", 5);
        let error = store.cancel("job-0", 6).expect_err("finished cancel");
        assert_eq!(error.status_code(), 409);
        assert_eq!(error.code(), "not_cancellable");
        assert_eq!(
            store.record("job-0").expect("present").state,
            JobState::Succeeded
        );
    }

    /// §4.5：数量上限，"最旧终态优先"。
    #[test]
    fn eviction_by_count_removes_the_oldest_terminal_job_first() {
        let mut store = JobStore::new(limits(2, u64::MAX, 16, 1_000_000));
        insert_ocr(&mut store, "old", 0, 0);
        finish(&mut store, "old", 100);
        insert_ocr(&mut store, "middle", 0, 0);
        finish(&mut store, "middle", 200);
        assert_eq!(store.terminal_count(), 2);
        assert_eq!(store.tombstone_len(), 0, "nothing evicted yet");

        insert_ocr(&mut store, "new", 0, 0);
        finish(&mut store, "new", 300);

        assert_eq!(store.terminal_count(), 2);
        assert_eq!(store.tombstone_len(), 1);
        assert!(matches!(
            store.record("old").expect_err("oldest must be evicted"),
            ServeError::JobEvicted
        ));
        assert!(store.record("middle").is_ok());
        assert!(store.record("new").is_ok());
        assert_eq!(
            store.evicted_at("old"),
            Some(300),
            "tombstone keeps evicted_at"
        );
    }

    /// §4.5：字节上限的账本口径是"原件 + 结果"。
    ///
    /// **M3 起的行为**：字节超限时先释放**最旧终态任务的原图**（结果与任务保留，见
    /// [`JobStore::enforce_retention`]），因此这里 a 不再被淘汰，而是丢掉它的 60 字节原图；
    /// "原图都释放完仍然超限才淘汰整个任务"由
    /// `when_only_results_remain_the_oldest_terminal_job_is_evicted` 覆盖。
    #[test]
    fn eviction_by_bytes_uses_the_original_plus_result_budget() {
        let mut store = JobStore::new(limits(32, 100, 16, 1_000_000));
        insert_ocr(&mut store, "a", 60, 0);
        store.start("a", 0).expect("-> running");
        store.succeed("a", 20, 0).expect("-> succeeded");
        assert_eq!(store.retained_bytes(), 80);

        // 新任务带来 60 字节原件 → 140 > 100 → 先释放最旧终态任务 a 的原图（60）。
        insert_ocr(&mut store, "b", 60, 0);
        assert_eq!(
            store.retained_bytes(),
            80,
            "a's original is what paid for it"
        );
        assert_eq!(
            store.record("a").expect("a survives").result_bytes,
            20,
            "the result is still retained and readable"
        );
        assert!(!store.record("a").expect("present").original_retained);
        assert!(store.record("b").is_ok(), "the active job is untouched");
        assert_eq!(store.take_released_originals(), vec!["a".to_string()]);
    }

    /// 活跃任务永不淘汰：字节超限但没有终态任务可淘汰时，账本如实超过上限。
    #[test]
    fn active_jobs_are_never_evicted() {
        let mut store = JobStore::new(limits(32, 100, 16, 1_000_000));
        insert_ocr(&mut store, "a", 60, 0);
        store.start("a", 0).expect("-> running");
        insert_ocr(&mut store, "b", 60, 0);
        assert_eq!(
            store.retained_bytes(),
            120,
            "over budget but both are active"
        );
        assert_eq!(store.len(), 2);
        assert_eq!(store.tombstone_len(), 0);
        assert!(store.record("a").is_ok());
        assert!(store.record("b").is_ok());
    }

    /// §4.5：TTL 由显式 `tick(now)` 驱动，注入时间即可验证，**不需要睡眠**。
    #[test]
    fn ttl_expiry_is_driven_by_tick_with_injected_time() {
        let mut store = JobStore::new(limits(32, u64::MAX, 16, 1_000));
        insert_ocr(&mut store, "job-0", 0, 0);
        finish(&mut store, "job-0", 5_000);

        // 未到期：状态仍是正常结果。
        let report = store.tick(5_999);
        assert_eq!(report, TickReport::default());
        assert!(store.record("job-0").is_ok());

        // 恰好到期 → 淘汰并写 tombstone。
        let report = store.tick(6_000);
        assert_eq!(report.expired_jobs, 1);
        assert_eq!(store.tombstone_len(), 1);
        assert_eq!(store.evicted_at("job-0"), Some(6_000));
        assert!(matches!(
            store.record("job-0").expect_err("expired"),
            ServeError::JobEvicted
        ));
    }

    /// TTL 不作用于活跃任务。
    #[test]
    fn ttl_never_expires_a_queued_or_running_job() {
        let mut store = JobStore::new(limits(32, u64::MAX, 16, 10));
        insert_ocr(&mut store, "queued", 0, 0);
        insert_ocr(&mut store, "running", 0, 0);
        store.start("running", 0).expect("-> running");
        let report = store.tick(1_000_000);
        assert_eq!(report.expired_jobs, 0);
        assert!(store.record("queued").is_ok());
        assert!(store.record("running").is_ok());
    }

    /// §4.5：tombstone 有自己的数量上限，超限时淘汰最旧的记录。
    #[test]
    fn tombstone_capacity_eviction_keeps_only_the_newest_entries() {
        // 保留 1 个终态任务、tombstone 上限 2：依次结束 a..d 会淘汰 a、b、c，
        // 第 3 次写入把最旧的 tombstone(a) 挤出表外。
        let mut store = JobStore::new(limits(1, u64::MAX, 2, 1_000_000));
        for (index, id) in ["a", "b", "c", "d"].iter().enumerate() {
            insert_ocr(&mut store, id, 0, 0);
            finish(&mut store, id, 100 * (index as Millis + 1));
        }
        assert_eq!(store.tombstone_len(), 2, "capacity 2");
        assert_eq!(store.evicted_at("a"), None, "the oldest tombstone is gone");
        assert_eq!(store.evicted_at("b"), Some(300));
        assert_eq!(store.evicted_at("c"), Some(400));
        assert!(store.record("d").is_ok(), "the newest terminal job is kept");
        // tombstone 被挤出后，查询退回 404（不是 410）。
        assert!(matches!(
            store.record("a").expect_err("unknown again"),
            ServeError::JobNotFound
        ));
    }

    /// §4.5：tombstone 也有 TTL；到期后同样的查询变成 404。
    #[test]
    fn tombstone_ttl_expiry_turns_410_back_into_404() {
        let mut store = JobStore::new(limits(1, u64::MAX, 16, 1_000));
        insert_ocr(&mut store, "a", 0, 0);
        finish(&mut store, "a", 100);
        insert_ocr(&mut store, "b", 0, 0);
        finish(&mut store, "b", 200);
        assert!(store.record("a").is_err(), "a was evicted for count");

        // 淘汰发生在 200；TTL 1000 → 1200 到期。
        let report = store.tick(1_199);
        assert_eq!(report.expired_tombstones, 0);
        assert!(matches!(
            store.record("a").expect_err("still tombstoned"),
            ServeError::JobEvicted
        ));

        let report = store.tick(1_200);
        assert_eq!(report.expired_tombstones, 1);
        assert!(matches!(
            store.record("a").expect_err("tombstone expired"),
            ServeError::JobNotFound
        ));
        assert_eq!(store.evicted_at("a"), None);
        // 同一次 tick 里 `b` 也到期了，它自己的 tombstone 仍在窗口内。
        assert_eq!(store.evicted_at("b"), Some(1_200));
        assert_eq!(store.tombstone_len(), 1);
    }

    /// 404 与 410 必须可区分：从未存在 vs 存在但已淘汰。
    #[test]
    fn unknown_jobs_are_404_while_evicted_jobs_are_410() {
        let mut store = JobStore::new(limits(1, u64::MAX, 16, 1_000_000));
        let never = store.view("never-existed", 0).expect_err("404");
        assert_eq!(never.status_code(), 404);
        assert_eq!(never.code(), "job_not_found");

        insert_ocr(&mut store, "gone", 0, 0);
        finish(&mut store, "gone", 10);
        insert_ocr(&mut store, "keeper", 0, 0);
        finish(&mut store, "keeper", 20);

        let evicted = store.view("gone", 30).expect_err("410");
        assert_eq!(evicted.status_code(), 410);
        assert_eq!(evicted.code(), "job_evicted");
        assert!(store.view("keeper", 30).is_ok());

        // 对已淘汰任务做状态操作，同样是 410 而不是 404。
        assert!(matches!(
            store.start("gone", 40).expect_err("410"),
            ServeError::JobEvicted
        ));
        assert!(matches!(
            store.cancel("gone", 40).expect_err("410"),
            ServeError::JobEvicted
        ));
    }

    #[test]
    fn byte_accounting_tracks_originals_and_results_across_eviction() {
        let mut store = store();
        insert_ocr(&mut store, "a", 1_000, 0);
        assert_eq!(store.retained_bytes(), 1_000);
        store.start("a", 0).expect("-> running");
        store.succeed("a", 250, 0).expect("-> succeeded");
        assert_eq!(store.retained_bytes(), 1_250);
        assert_eq!(store.record("a").expect("present").result_bytes, 250);
        assert!(
            store.record("a").expect("present").original_retained,
            "a 64 MB budget holds both the original and the result"
        );

        insert_ocr(&mut store, "b", 500, 0);
        assert_eq!(store.retained_bytes(), 1_750);
    }

    /// §4.5 / §4.2（M3）：字节上限先**释放原图**（结果与任务保留），原图都释放完才淘汰整个任务。
    ///
    /// 这一步是 `/annotated.png` 的 410 `original_evicted` 能成立的前提：任务记录还在、
    /// 结果还能读，只有那份编码原图被预算收回了。
    #[test]
    fn an_over_budget_store_releases_originals_before_evicting_jobs() {
        let mut store = JobStore::new(limits(32, 1_200, 16, 1_000_000));
        insert_ocr(&mut store, "a", 1_000, 0);
        store.start("a", 0).expect("-> running");
        // 1300 > 1200：先释放 a 的原图（1000），任务与结果都留下。
        store.succeed("a", 300, 0).expect("-> succeeded");
        assert_eq!(store.retained_bytes(), 300);
        assert!(store.record("a").is_ok(), "the job itself must survive");
        assert_eq!(store.record("a").expect("present").result_bytes, 300);
        assert!(!store.record("a").expect("present").original_retained);
        assert_eq!(store.take_released_originals(), vec!["a".to_string()]);

        insert_ocr(&mut store, "b", 600, 0);
        store.start("b", 0).expect("-> running");
        store.succeed("b", 100, 0).expect("-> succeeded");
        // 300 + 700 = 1000 ≤ 1200：没有新的释放。
        assert_eq!(store.retained_bytes(), 1_000);
        assert!(store.record("b").expect("present").original_retained);

        insert_ocr(&mut store, "c", 600, 0);
        // 1000 + 600 = 1600 > 1200：释放最旧**仍持有原图**的终态任务（b，600）→ 1000。
        assert_eq!(store.retained_bytes(), 1_000);
        assert!(!store.record("b").expect("present").original_retained);
        assert!(store.record("c").is_ok(), "the active job is never touched");
        assert_eq!(store.take_released_originals(), vec!["b".to_string()]);
    }

    /// 原图都释放完仍然超限 → 才退回"淘汰整个终态任务"（M0c 起的原有行为）。
    #[test]
    fn when_only_results_remain_the_oldest_terminal_job_is_evicted() {
        let mut store = JobStore::new(limits(32, 500, 16, 1_000_000));
        insert_ocr(&mut store, "a", 1_000, 0);
        store.start("a", 0).expect("-> running");
        // 1250 > 500 → 释放 a 的原图 → 250 ≤ 500。
        store.succeed("a", 250, 0).expect("-> succeeded");
        assert_eq!(store.retained_bytes(), 250);
        assert_eq!(store.take_released_originals(), vec!["a".to_string()]);

        insert_ocr(&mut store, "b", 0, 0);
        store.start("b", 1).expect("-> running");
        // 250 + 400 = 650 > 500：没有任何可释放的原图（a 已释放、b 本来就没有），
        // 于是淘汰最旧的终态任务 a。
        store.succeed("b", 400, 1).expect("-> succeeded");
        assert_eq!(store.retained_bytes(), 400);
        assert!(matches!(
            store.record("a").expect_err("evicted"),
            ServeError::JobEvicted
        ));
        assert!(store.record("b").is_ok());
        assert!(store.take_released_originals().is_empty());
    }

    /// 活跃任务的字节由队列容量与 `--max-body-mb` 约束：保留上限**不**动它们。
    #[test]
    fn active_jobs_keep_their_original_even_over_the_byte_budget() {
        let mut store = JobStore::new(limits(32, 100, 16, 1_000_000));
        insert_ocr(&mut store, "queued", 1_000, 0);
        assert_eq!(store.retained_bytes(), 1_000);
        assert!(
            store.record("queued").expect("present").original_retained,
            "the worker still needs that original to decode the image"
        );
        store.start("queued", 1).expect("-> running");
        assert!(store.record("queued").expect("present").original_retained);
        assert!(store.take_released_originals().is_empty());
    }

    /// 失败与取消的任务**永不**会有注释图：它们的原图随终态一起释放。
    #[test]
    fn failed_and_cancelled_jobs_release_their_original() {
        let mut failed = store();
        insert_ocr(&mut failed, "boom", 900, 0);
        failed.start("boom", 0).expect("-> running");
        failed
            .fail_classified(
                "boom",
                JobFailure::new(422, "unsupported_input", "x", serde_json::Value::Null),
                0,
            )
            .expect("-> failed");
        assert!(!failed.record("boom").expect("present").original_retained);
        assert_eq!(failed.retained_bytes(), 0);
        assert_eq!(failed.take_released_originals(), vec!["boom".to_string()]);

        let mut cancelled = store();
        insert_ocr(&mut cancelled, "gone", 700, 0);
        assert_eq!(
            cancelled.cancel("gone", 0).expect("queued cancel"),
            CancelOutcome::Cancelled
        );
        assert!(!cancelled.record("gone").expect("present").original_retained);
        assert_eq!(cancelled.retained_bytes(), 0);
        assert_eq!(
            cancelled.take_released_originals(),
            vec!["gone".to_string()]
        );

        // 显式释放是幂等的，未知 id / 零字节原图都不是错误（返回 false）。
        assert!(!cancelled.release_original("gone"));
        assert!(!cancelled.release_original("nobody"));
    }

    #[test]
    fn model_download_jobs_are_stored_with_their_own_kind_and_queue() {
        let mut store = store();
        store
            .insert(
                "dl-0",
                JobKind::ModelDownload,
                JobQueue::Download,
                0,
                0,
                TEST_REQUEST_ID,
            )
            .expect("fresh id");
        store
            .set_download_progress("dl-0", DownloadProgress::planned(2, Some(300)))
            .expect("a download job carries progress");
        let view = store.view("dl-0", 0).expect("present");
        assert_eq!(view.kind, JobKind::ModelDownload);
        assert_eq!(
            view.queue,
            JobQueue::Download,
            "a download job is not in the text queue"
        );
        assert_eq!(
            view.queue.class(),
            None,
            "only the dual queue has a scheduler class"
        );
        let json = serde_json::to_value(&view).expect("JobView must serialize");
        assert_eq!(json["kind"], "model_download");
        assert_eq!(json["queue"], "download");
        assert_eq!(json["state"], "queued");
        assert_eq!(json["position"], serde_json::Value::Null);
        assert_eq!(json["download"]["files_done"], 0);
        assert_eq!(json["download"]["files_total"], 2);
        assert_eq!(json["download"]["bytes_done"], 0);
        assert_eq!(json["download"]["bytes_total"], 300);
        assert_eq!(json["download"]["current_file"], serde_json::Value::Null);
        assert_eq!(json["cancel_requested"], false);
        assert_eq!(
            json["original_retained"], false,
            "a download job has no original image at all"
        );
    }

    /// §6.6：运行中的**下载**可以取消（登记请求，状态不变），由 worker 在文件边界落地；
    /// 运行中的 **OCR** 仍然 409（推理不可中断，不得假装取消成功）。
    #[test]
    fn a_running_download_can_be_cancelled_at_a_file_boundary_but_ocr_cannot() {
        let mut downloads = store();
        downloads
            .insert(
                "dl-0",
                JobKind::ModelDownload,
                JobQueue::Download,
                0,
                0,
                TEST_REQUEST_ID,
            )
            .expect("fresh id");
        downloads.start("dl-0", 1).expect("-> running");
        assert_eq!(
            downloads
                .cancel("dl-0", 2)
                .expect("a running download accepts a request"),
            CancelOutcome::CancelRequested
        );
        let view = downloads.view("dl-0", 3).expect("present");
        assert_eq!(
            view.state,
            JobState::Running,
            "the state must not pretend the download already stopped"
        );
        assert!(view.cancel_requested);
        assert!(downloads.is_cancel_requested("dl-0"));
        // 只有登记过取消请求的运行中下载任务才能落地为 cancelled。
        downloads.finish_cancelled("dl-0", 4).expect("-> cancelled");
        let view = downloads.view("dl-0", 5).expect("present");
        assert_eq!(view.state, JobState::Cancelled);
        assert_eq!(view.elapsed_ms, 3, "elapsed freezes at the cancel time");
        assert!(view.cancel_requested, "the audit flag stays visible");

        // 没有取消请求就落地 = 伪造取消 → 内部错误。
        downloads
            .insert(
                "dl-1",
                JobKind::ModelDownload,
                JobQueue::Download,
                0,
                0,
                TEST_REQUEST_ID,
            )
            .expect("fresh id");
        downloads.start("dl-1", 1).expect("-> running");
        assert!(matches!(
            downloads
                .finish_cancelled("dl-1", 2)
                .expect_err("only a requested cancel may be finished"),
            ServeError::Internal
        ));
        assert_eq!(
            downloads.record("dl-1").expect("present").state,
            JobState::Running
        );

        // 排队中的下载取消之后，worker 的 `start` 必须失败（它会如实跳过，而不是改回排队）。
        downloads
            .insert(
                "dl-2",
                JobKind::ModelDownload,
                JobQueue::Download,
                0,
                0,
                TEST_REQUEST_ID,
            )
            .expect("fresh id");
        assert_eq!(
            downloads.cancel("dl-2", 3).expect("queued cancel"),
            CancelOutcome::Cancelled
        );
        assert!(
            downloads.start("dl-2", 4).is_err(),
            "the worker must not start a job that was cancelled while queued"
        );

        // OCR：运行中依然是 409 not_cancellable，状态不变。
        let mut ocr_store = store();
        insert_ocr(&mut ocr_store, "job-0", 8, 0);
        ocr_store.start("job-0", 1).expect("-> running");
        let error = ocr_store
            .cancel("job-0", 2)
            .expect_err("a running OCR job is not cancellable");
        assert_eq!(error.code(), "not_cancellable");
        assert_eq!(
            ocr_store.record("job-0").expect("present").state,
            JobState::Running
        );
        assert!(!ocr_store.is_cancel_requested("job-0"));
    }

    /// 失败任务同时给出人类可读文本与机器可读分类（同一份原因，不靠字符串匹配）。
    #[test]
    fn a_classified_failure_carries_its_code_and_detail() {
        let mut store = store();
        store
            .insert(
                "dl-0",
                JobKind::ModelDownload,
                JobQueue::Download,
                0,
                0,
                TEST_REQUEST_ID,
            )
            .expect("fresh id");
        store.start("dl-0", 1).expect("-> running");
        store
            .fail_classified(
                "dl-0",
                JobFailure::new(
                    507,
                    "insufficient_disk_space",
                    "insufficient disk space: 1000 bytes required, 10 bytes available",
                    serde_json::json!({ "required_bytes": 1000, "available_bytes": 10 }),
                ),
                2,
            )
            .expect("-> failed");
        let json = serde_json::to_value(store.view("dl-0", 3).expect("present"))
            .expect("JobView must serialize");
        assert_eq!(json["state"], "failed");
        assert_eq!(json["failure"]["status"], 507);
        assert_eq!(json["failure"]["code"], "insufficient_disk_space");
        assert_eq!(json["failure"]["detail"]["required_bytes"], 1000);
        assert_eq!(json["failure"]["detail"]["available_bytes"], 10);
        assert_eq!(
            json["error"], json["failure"]["message"],
            "the frozen `error` field stays the same text"
        );
    }

    /// `/api/jobs/{id}` 的字段名与 §4.2 完全一致。
    #[test]
    fn job_view_serializes_the_documented_field_names() {
        let mut store = store();
        insert_ocr(&mut store, "job-0", 8, 0);
        store.set_position("job-0", Some(1)).expect("queued");
        let json = serde_json::to_value(store.view("job-0", 4).expect("present"))
            .expect("JobView must serialize");
        for key in [
            "id",
            "kind",
            "queue",
            "state",
            "position",
            "queued_ms",
            "started_ms",
            "elapsed_ms",
            "error",
            "failure",
            "download",
            "cancel_requested",
            "original_retained",
        ] {
            assert!(json.get(key).is_some(), "missing {key}: {json}");
        }
        assert_eq!(json["position"], 1);
        assert_eq!(json["started_ms"], serde_json::Value::Null);
        assert_eq!(json["elapsed_ms"], 4);
        assert_eq!(json["failure"], serde_json::Value::Null);
        assert_eq!(json["original_retained"], true, "{json}");
    }

    /// 一次 tick 可以同时报告四类清理结果。
    #[test]
    fn a_single_tick_reports_every_cleanup_class() {
        let mut store = JobStore::new(limits(1, u64::MAX, 1, 500));
        // 一个到期任务（写 tombstone，挤出旧 tombstone，并被数量上限淘汰）。
        insert_ocr(&mut store, "expired", 0, 0);
        finish(&mut store, "expired", 10);
        insert_ocr(&mut store, "fresh", 0, 0);
        finish(&mut store, "fresh", 6_000);
        let report = store.tick(6_500);
        assert_eq!(report.expired_jobs, 1, "`fresh` is inside its TTL window");
        assert_eq!(report.evicted_for_count, 0);
        assert_eq!(store.tombstone_len(), 1);
    }
}
