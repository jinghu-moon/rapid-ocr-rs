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

use super::error::ServeError;
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
    pub fn from_limits(limits: &ServeLimits) -> Self {
        Self {
            max_retained: limits.max_retained,
            max_retained_bytes: limits.max_retained_bytes,
            max_tombstones: limits.max_tombstones,
            ttl_ms: limits.job_ttl_ms,
        }
    }
}

/// 任务记录（存储内部形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: String,
    pub kind: JobKind,
    pub class: QueueClass,
    pub state: JobState,
    /// 队列内位置（仅 `Queued` 时有意义）。
    pub position: Option<usize>,
    /// 进入队列的时刻。
    pub queued_ms: Millis,
    /// 开始执行的时刻。
    pub started_ms: Option<Millis>,
    /// 进入终态的时刻。
    pub finished_ms: Option<Millis>,
    /// 保留的**原件**字节数（编码后的图片，不保留解码结果）。
    pub original_bytes: u64,
    /// 保留的**结果**字节数（序列化后的结果）。
    pub result_bytes: u64,
    /// 终态原因（`Failed` 时是错误文本）。
    pub error: Option<String>,
    /// 插入序号：同一时刻结束的任务用它做稳定排序。
    pub seq: u64,
}

impl JobRecord {
    /// 该任务占用的保留字节（原件 + 结果）。
    pub fn total_bytes(&self) -> u64 {
        self.original_bytes.saturating_add(self.result_bytes)
    }

    /// 已耗时：从开始执行（未开始则从入队）到结束（未结束则到 `now`）。
    pub fn elapsed_ms(&self, now: Millis) -> Millis {
        let end = self.finished_ms.unwrap_or(now);
        end.saturating_sub(self.started_ms.unwrap_or(self.queued_ms))
    }

    /// `/api/jobs/{id}` 的响应视图（字段顺序与 §4.2 一致）。
    pub fn view(&self, now: Millis) -> JobView {
        JobView {
            id: self.id.clone(),
            kind: self.kind,
            queue: self.class,
            state: self.state,
            position: self.position,
            queued_ms: self.queued_ms,
            started_ms: self.started_ms,
            elapsed_ms: self.elapsed_ms(now),
            error: self.error.clone(),
        }
    }
}

/// `/api/jobs/{id}` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobView {
    pub id: String,
    pub kind: JobKind,
    pub queue: QueueClass,
    pub state: JobState,
    pub position: Option<usize>,
    pub queued_ms: Millis,
    pub started_ms: Option<Millis>,
    pub elapsed_ms: Millis,
    pub error: Option<String>,
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
}

impl JobStore {
    pub fn new(limits: JobStoreLimits) -> Self {
        Self {
            limits,
            jobs: BTreeMap::new(),
            tombstones: VecDeque::new(),
            retained_bytes: 0,
            next_seq: 0,
        }
    }

    pub fn limits(&self) -> JobStoreLimits {
        self.limits
    }

    /// 建任务（`Queued`）。`original_bytes` 是要保留的编码图片字节数。
    pub fn insert(
        &mut self,
        id: impl Into<String>,
        kind: JobKind,
        class: QueueClass,
        original_bytes: u64,
        now: Millis,
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
            class,
            state: JobState::Queued,
            position: None,
            queued_ms: now,
            started_ms: None,
            finished_ms: None,
            original_bytes,
            result_bytes: 0,
            error: None,
            seq,
        };
        self.retained_bytes = self.retained_bytes.saturating_add(record.total_bytes());
        self.jobs.insert(id, record);
        // 新任务带来的字节可能已经越过保留上限：立即按"最旧终态优先"回收。
        self.enforce_retention(now);
        Ok(())
    }

    /// 更新队列内位置（调度器每次取任务后 M1 重新计算）。
    pub fn set_position(&mut self, id: &str, position: Option<usize>) -> Result<(), ServeError> {
        self.record_mut(id)?.position = position;
        Ok(())
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
    pub fn fail(
        &mut self,
        id: &str,
        reason: impl Into<String>,
        now: Millis,
    ) -> Result<(), ServeError> {
        {
            let record = self.record_mut(id)?;
            if record.state != JobState::Running {
                return Err(ServeError::Internal);
            }
            record.state = JobState::Failed;
            record.error = Some(reason.into());
            record.finished_ms = Some(now);
            record.position = None;
        }
        self.enforce_retention(now);
        Ok(())
    }

    /// 取消（§4.3）。
    ///
    /// 返回该任务所属队列，便于 M1 同时把它从调度器里移出。
    /// `Running` 与终态一律返回 [`ServeError::NotCancellable`]（409），状态**不变**。
    pub fn cancel(&mut self, id: &str, now: Millis) -> Result<QueueClass, ServeError> {
        let record = self.record_mut(id)?;
        match record.state {
            JobState::Queued => {
                record.state = JobState::Cancelled;
                record.finished_ms = Some(now);
                let class = record.class;
                self.enforce_retention(now);
                Ok(class)
            }
            JobState::Running => Err(ServeError::NotCancellable),
            JobState::Succeeded | JobState::Failed | JobState::Cancelled => {
                Err(ServeError::NotCancellable)
            }
        }
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

        let (for_count, for_bytes) = self.enforce_retention(now);
        report.evicted_for_count += for_count;
        report.evicted_for_bytes += for_bytes;
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

    /// 数量与字节双重上限：只淘汰终态任务，**最旧终态优先**。
    ///
    /// 返回 `(因数量淘汰数, 因字节淘汰数)`。
    fn enforce_retention(&mut self, now: Millis) -> (usize, usize) {
        let mut for_count = 0;
        let mut for_bytes = 0;
        loop {
            let over_count = self.terminal_count() > self.limits.max_retained;
            let over_bytes = self.retained_bytes > self.limits.max_retained_bytes;
            if !over_count && !over_bytes {
                break;
            }
            let Some(oldest) = self.oldest_terminal() else {
                // 只剩活跃任务：活跃任务不淘汰（其总量由队列容量与 body 上限约束）。
                break;
            };
            self.evict(&oldest, now);
            if over_count {
                for_count += 1;
            } else {
                for_bytes += 1;
            }
        }
        (for_count, for_bytes)
    }

    fn oldest_terminal(&self) -> Option<String> {
        self.jobs
            .values()
            .filter(|record| record.state.is_terminal())
            .min_by_key(|record| (record.finished_ms.unwrap_or(record.queued_ms), record.seq))
            .map(|record| record.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{JobIdGenerator, JobKind, JobState, JobStore, JobStoreLimits, Millis, TickReport};
    use crate::serve::error::ServeError;
    use crate::serve::queue::QueueClass;

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
            .insert(id, JobKind::Ocr, QueueClass::Text, bytes, now)
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
        assert_eq!(view.queue, QueueClass::Text);
        assert_eq!(view.state, JobState::Queued);
        assert_eq!(view.position, Some(0));
        assert_eq!(view.queued_ms, 100);
        assert_eq!(view.started_ms, None);
        assert_eq!(view.elapsed_ms, 0);
        assert_eq!(view.error, None);

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
                .insert("job-0", JobKind::Ocr, QueueClass::Text, 0, 1)
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
        let class = store.cancel("job-0", 20).expect("queued cancel must work");
        assert_eq!(class, QueueClass::Text);
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

    /// §4.5：字节上限同样只淘汰终态任务。
    #[test]
    fn eviction_by_bytes_uses_the_original_plus_result_budget() {
        let mut store = JobStore::new(limits(32, 100, 16, 1_000_000));
        insert_ocr(&mut store, "a", 60, 0);
        store.start("a", 0).expect("-> running");
        store.succeed("a", 20, 0).expect("-> succeeded");
        assert_eq!(store.retained_bytes(), 80);

        // 新任务带来 60 字节原件 → 140 > 100 → 淘汰最旧的终态任务 a。
        insert_ocr(&mut store, "b", 60, 0);
        assert_eq!(store.retained_bytes(), 60);
        assert!(matches!(
            store.record("a").expect_err("evicted for bytes"),
            ServeError::JobEvicted
        ));
        assert!(store.record("b").is_ok());
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

        insert_ocr(&mut store, "b", 500, 0);
        assert_eq!(store.retained_bytes(), 1_750);
        // 淘汰 a 后账本必须减掉它的全部占用。
        let mut tight = JobStore::new(limits(1, 1_200, 16, 1_000_000));
        insert_ocr(&mut tight, "a", 1_000, 0);
        tight.start("a", 0).expect("-> running");
        tight.succeed("a", 250, 0).expect("-> succeeded");
        insert_ocr(&mut tight, "b", 500, 0);
        assert_eq!(tight.retained_bytes(), 500, "a's 1250 bytes were released");
        assert!(matches!(
            tight.record("a").expect_err("evicted"),
            ServeError::JobEvicted
        ));
    }

    #[test]
    fn model_download_jobs_are_stored_with_their_own_kind_and_queue() {
        let mut store = store();
        store
            .insert("dl-0", JobKind::ModelDownload, QueueClass::Formula, 0, 0)
            .expect("fresh id");
        let view = store.view("dl-0", 0).expect("present");
        assert_eq!(view.kind, JobKind::ModelDownload);
        assert_eq!(view.queue, QueueClass::Formula);
        let json = serde_json::to_value(&view).expect("JobView must serialize");
        assert_eq!(json["kind"], "model_download");
        assert_eq!(json["queue"], "formula");
        assert_eq!(json["state"], "queued");
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
        ] {
            assert!(json.get(key).is_some(), "missing {key}: {json}");
        }
        assert_eq!(json["position"], 1);
        assert_eq!(json["started_ms"], serde_json::Value::Null);
        assert_eq!(json["elapsed_ms"], 4);
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
