//! 双队列调度：容量、双向连续配额与公平性界（§8.2、§8.3）。
//!
//! # 为什么需要双向配额
//!
//! 上一版只限制"连续公式任务数"，于是普通任务持续到达时公式队列会被**永久饿死**。
//! §8.3 因此要求两个方向都有界，本模块把这条要求实现成**一个可以证明的调度策略**：
//!
//! 1. 两个**独立容量**（`--max-queue-text` / `--max-queue-formula`）：
//!    公式洪水无法占用普通队列的槽位（反之亦然），容量满时**立即** `Busy`（§8.2，不阻塞等待）；
//! 2. 调度按轮进行：每轮先取最多 `--max-consecutive-text`（默认 4）个普通任务，
//!    再取最多 `--max-consecutive-formula`（默认 1）个公式任务；
//! 3. **保底**：只要某队列非空，它在一轮内一定被服务至少一次——配额本身 ≥1
//!    （由 [`SchedulerConfig::new`] 强制），因此保底是策略的结构性质，不是特例分支；
//! 4. 某一队列为空时，另一队列**自由连续处理**（不浪费空闲额度）；
//! 5. 配额计数只在**两者都满足**时清零（开新一轮），没有其他清零分支。
//!    这条是 M0c 实测修正的结果：曾经的"空队列重新有任务就清零"会让
//!    **持续到达的细流队列每步都重开一轮**，于是普通队列（取法里优先）永远
//!    轮不到公式队列——正好复现 §8.3 要消灭的那种饿死。
//!    不清零同样安全：由取法可知，`served_text >= T` 与 `served_formula >= F`
//!    不可能同时成立（成立即刻清零），因此新到的普通任务最多等 `F` 个公式任务、
//!    新到的公式任务最多等 `T` 个普通任务。
//!
//! # 可证明的公平性界
//!
//! 记 `T = max_consecutive_text`、`F = max_consecutive_formula`、`C = 该队列容量`。
//!
//! - 一轮最多服务 `T + F` 个任务；
//! - 同一类的两次服务之间，另一类最多被服务 `T` 次（对公式）或 `F` 次（对普通）；
//! - FIFO 下，一个任务前面最多还有 `C - 1` 个同类任务（满队时入队直接 `Busy`），
//!   因此**一个任务从入队到被服务**，另一类被服务的次数不超过 `C * T`（公式）
//!   或 `C * F`（普通）。
//!
//! 这正是 §8.3 要的"等待时间有上界"：公式洪水下普通 OCR 的等待有界，
//! 普通洪水下公式任务的等待同样有界（§8.3 的验收测试**两个方向都要**）。
//! 饥饿策略（"先做普通，普通为空才做公式"）会给出**无界**等待，
//! `queue::tests` 的洪水测试就是为此写的（M0c 报告里记录了它确实会失败）。

use std::collections::VecDeque;

use serde::Serialize;

use super::error::ServeError;
use super::limits::{
    DEFAULT_MAX_CONSECUTIVE_FORMULA, DEFAULT_MAX_CONSECUTIVE_TEXT, DEFAULT_MAX_QUEUE_FORMULA,
    DEFAULT_MAX_QUEUE_TEXT, ServeConfigError, ServeLimits, require_positive_usize,
};

/// 队列类别：普通 OCR 与公式 OCR 分队列（§8.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueClass {
    Text,
    Formula,
}

impl QueueClass {
    /// 机器可读名称（进 `/api/jobs/{id}` 的 `queue` 字段与日志）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Formula => "formula",
        }
    }

    /// 另一队列。
    pub fn other(self) -> Self {
        match self {
            Self::Text => Self::Formula,
            Self::Formula => Self::Text,
        }
    }
}

/// 调度参数（§3 的四个开关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub max_queue_text: usize,
    pub max_queue_formula: usize,
    pub max_consecutive_text: usize,
    pub max_consecutive_formula: usize,
}

impl Default for SchedulerConfig {
    /// 文档 §3 的默认值。
    fn default() -> Self {
        Self {
            max_queue_text: DEFAULT_MAX_QUEUE_TEXT,
            max_queue_formula: DEFAULT_MAX_QUEUE_FORMULA,
            max_consecutive_text: DEFAULT_MAX_CONSECUTIVE_TEXT,
            max_consecutive_formula: DEFAULT_MAX_CONSECUTIVE_FORMULA,
        }
    }
}

impl SchedulerConfig {
    /// 校验取值：容量与连续配额都必须 ≥ 1，否则 §8.3 的保底规则无法成立。
    pub fn new(
        max_queue_text: usize,
        max_queue_formula: usize,
        max_consecutive_text: usize,
        max_consecutive_formula: usize,
    ) -> Result<Self, ServeConfigError> {
        Ok(Self {
            max_queue_text: require_positive_usize("--max-queue-text", max_queue_text)?,
            max_queue_formula: require_positive_usize("--max-queue-formula", max_queue_formula)?,
            max_consecutive_text: require_positive_usize(
                "--max-consecutive-text",
                max_consecutive_text,
            )?,
            max_consecutive_formula: require_positive_usize(
                "--max-consecutive-formula",
                max_consecutive_formula,
            )?,
        })
    }

    /// 从**已经校验过**的 [`ServeLimits`] 构造。
    ///
    /// 仍然走 [`Self::new`]：四个取值的校验（≥ 1，否则 §8.3 的保底规则无法成立）
    /// 只有一份实现。
    pub fn from_limits(limits: &ServeLimits) -> Self {
        Self::new(
            limits.max_queue_text,
            limits.max_queue_formula,
            limits.max_consecutive_text,
            limits.max_consecutive_formula,
        )
        .expect("ServeLimits validates the same four bounds")
    }

    /// 一轮最多服务的任务数（§8.3 的调度粒度）。
    pub fn round_len(&self) -> usize {
        self.max_consecutive_text + self.max_consecutive_formula
    }

    /// 某队列的容量。
    pub fn capacity(&self, class: QueueClass) -> usize {
        match class {
            QueueClass::Text => self.max_queue_text,
            QueueClass::Formula => self.max_queue_formula,
        }
    }

    /// 某队列连续服务的配额。
    pub fn consecutive_quota(&self, class: QueueClass) -> usize {
        match class {
            QueueClass::Text => self.max_consecutive_text,
            QueueClass::Formula => self.max_consecutive_formula,
        }
    }

    /// §8.3 的可证明上界：某类任务从入队到被服务，**另一类**最多被服务多少次。
    pub fn wait_bound(&self, class: QueueClass) -> usize {
        self.capacity(class) * self.consecutive_quota(class.other())
    }
}

/// 被调度器选中的任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledJob {
    pub class: QueueClass,
    pub id: String,
}

/// 双队列调度器（纯逻辑，无线程、无锁）。
///
/// 并发由 M1 的 worker 负责：本类型只回答"下一个该做哪个队列的任务"。
#[derive(Debug, Clone)]
pub struct DualQueueScheduler {
    config: SchedulerConfig,
    text: VecDeque<String>,
    formula: VecDeque<String>,
    /// 本轮已服务的普通任务数。
    served_text: usize,
    /// 本轮已服务的公式任务数。
    served_formula: usize,
}

impl DualQueueScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            text: VecDeque::new(),
            formula: VecDeque::new(),
            served_text: 0,
            served_formula: 0,
        }
    }

    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    /// 入队。队列满时**立即**返回 [`ServeError::Busy`]（503），绝不阻塞等待（§8.2）。
    ///
    /// 返回该队列内的 0 基位置；M1 用它填充 `/api/jobs/{id}` 的 `position`
    /// （每次服务后位置会左移，因此轮询时应重新查询而非缓存）。
    ///
    /// 入队**不会**重置轮次计数：细流队列每步重开一轮会让另一个队列永远轮不到
    /// （见模块文档第 5 条）。
    pub fn enqueue(
        &mut self,
        class: QueueClass,
        id: impl Into<String>,
    ) -> Result<usize, ServeError> {
        if self.queued_len(class) >= self.config.capacity(class) {
            return Err(ServeError::Busy);
        }
        self.queue_mut(class).push_back(id.into());
        Ok(self.queued_len(class) - 1)
    }

    /// 取出下一个要执行的任务（§8.3 的取法见模块文档）。
    pub fn take_next(&mut self) -> Option<ScheduledJob> {
        let class = self.peek_class()?;
        let id = match class {
            QueueClass::Text => self.text.pop_front(),
            QueueClass::Formula => self.formula.pop_front(),
        }?;
        self.record_service(class);
        Some(ScheduledJob { class, id })
    }

    /// 把仍然排队的任务移出（取消排队中的任务时必须调用，见 §4.3）。
    pub fn remove(&mut self, class: QueueClass, id: &str) -> bool {
        let queue = self.queue_mut(class);
        let Some(position) = queue.iter().position(|queued| queued == id) else {
            return false;
        };
        queue.remove(position);
        true
    }

    /// 某任务在该队列中的 0 基位置。
    pub fn position_of(&self, class: QueueClass, id: &str) -> Option<usize> {
        self.queue(class).iter().position(|queued| queued == id)
    }

    /// 某队列当前排队数。
    pub fn queued_len(&self, class: QueueClass) -> usize {
        self.queue(class).len()
    }

    /// 是否两个队列都为空。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.formula.is_empty()
    }

    /// 本轮已服务数（诊断用）。
    pub fn served_in_round(&self, class: QueueClass) -> usize {
        match class {
            QueueClass::Text => self.served_text,
            QueueClass::Formula => self.served_formula,
        }
    }

    fn queue(&self, class: QueueClass) -> &VecDeque<String> {
        match class {
            QueueClass::Text => &self.text,
            QueueClass::Formula => &self.formula,
        }
    }

    fn queue_mut(&mut self, class: QueueClass) -> &mut VecDeque<String> {
        match class {
            QueueClass::Text => &mut self.text,
            QueueClass::Formula => &mut self.formula,
        }
    }

    /// 下一个该服务的队列。
    fn peek_class(&self) -> Option<QueueClass> {
        match (self.text.is_empty(), self.formula.is_empty()) {
            (true, true) => None,
            // 某一队列为空时，另一队列自由连续处理（不浪费空闲额度）。
            (true, false) => Some(QueueClass::Formula),
            (false, true) => Some(QueueClass::Text),
            // 两者都非空：本轮先普通（最多 max_consecutive_text），配额用完再公式。
            (false, false) => {
                if self.served_text < self.config.max_consecutive_text {
                    Some(QueueClass::Text)
                } else {
                    Some(QueueClass::Formula)
                }
            }
        }
    }

    fn record_service(&mut self, class: QueueClass) {
        match class {
            QueueClass::Text => self.served_text += 1,
            QueueClass::Formula => self.served_formula += 1,
        }
        if self.served_text >= self.config.max_consecutive_text
            && self.served_formula >= self.config.max_consecutive_formula
        {
            self.served_text = 0;
            self.served_formula = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{DualQueueScheduler, QueueClass, SchedulerConfig};
    use crate::serve::error::ServeError;

    fn default_scheduler() -> DualQueueScheduler {
        DualQueueScheduler::new(SchedulerConfig::default())
    }

    /// 按期望的类别序列取出任务，并返回 id 序列。
    fn take(scheduler: &mut DualQueueScheduler, expected: &[QueueClass]) -> Vec<String> {
        let mut classes = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..expected.len() {
            let job = scheduler.take_next().expect("a job must be available");
            classes.push(job.class);
            ids.push(job.id);
        }
        assert_eq!(classes, expected, "scheduler order");
        ids
    }

    #[test]
    fn documented_defaults_are_the_documented_values() {
        let config = SchedulerConfig::default();
        assert_eq!(config.max_queue_text, 4);
        assert_eq!(config.max_queue_formula, 2);
        assert_eq!(config.max_consecutive_text, 4);
        assert_eq!(config.max_consecutive_formula, 1);
        assert_eq!(config.round_len(), 5);
        assert_eq!(config.capacity(QueueClass::Text), 4);
        assert_eq!(config.capacity(QueueClass::Formula), 2);
        assert_eq!(config.consecutive_quota(QueueClass::Text), 4);
        assert_eq!(config.consecutive_quota(QueueClass::Formula), 1);
        // 上界：公式 ≤ 2*4，普通 ≤ 4*1。
        assert_eq!(config.wait_bound(QueueClass::Formula), 8);
        assert_eq!(config.wait_bound(QueueClass::Text), 4);
    }

    #[test]
    fn zero_capacities_and_quotas_are_rejected_with_the_flag_name() {
        let cases: [(&str, SchedulerConfig); 4] = [
            (
                "--max-queue-text",
                SchedulerConfig {
                    max_queue_text: 0,
                    ..SchedulerConfig::default()
                },
            ),
            (
                "--max-queue-formula",
                SchedulerConfig {
                    max_queue_formula: 0,
                    ..SchedulerConfig::default()
                },
            ),
            (
                "--max-consecutive-text",
                SchedulerConfig {
                    max_consecutive_text: 0,
                    ..SchedulerConfig::default()
                },
            ),
            (
                "--max-consecutive-formula",
                SchedulerConfig {
                    max_consecutive_formula: 0,
                    ..SchedulerConfig::default()
                },
            ),
        ];
        for (field, config) in cases {
            let error = SchedulerConfig::new(
                config.max_queue_text,
                config.max_queue_formula,
                config.max_consecutive_text,
                config.max_consecutive_formula,
            )
            .expect_err(&format!("{field}=0 must be rejected"));
            assert_eq!(error.field(), field, "error: {error}");
        }
        assert!(
            SchedulerConfig::new(1, 1, 1, 1).is_ok(),
            "minimal positive values must be accepted"
        );
    }

    /// 容量是**每队列独立**的：公式洪水占不到普通队列的槽位，且满时立即 `Busy`。
    #[test]
    fn each_queue_has_its_own_capacity_and_reports_busy_immediately() {
        let mut scheduler = default_scheduler();
        for index in 0..4 {
            scheduler
                .enqueue(QueueClass::Text, format!("t{index}"))
                .expect("up to --max-queue-text");
        }
        let error = scheduler
            .enqueue(QueueClass::Text, "t4")
            .expect_err("a full text queue must be Busy");
        assert!(matches!(error, ServeError::Busy), "error: {error}");
        assert_eq!(error.status_code(), 503);

        // 公式队列仍然是空的：容量独立。
        assert_eq!(scheduler.queued_len(QueueClass::Formula), 0);
        for index in 0..2 {
            scheduler
                .enqueue(QueueClass::Formula, format!("f{index}"))
                .expect("up to --max-queue-formula");
        }
        let error = scheduler
            .enqueue(QueueClass::Formula, "f2")
            .expect_err("a full formula queue must be Busy");
        assert!(matches!(error, ServeError::Busy), "error: {error}");
    }

    #[test]
    fn enqueue_reports_the_zero_based_position() {
        let mut scheduler = default_scheduler();
        assert_eq!(scheduler.enqueue(QueueClass::Text, "a").expect("fits"), 0);
        assert_eq!(scheduler.enqueue(QueueClass::Text, "b").expect("fits"), 1);
        assert_eq!(scheduler.position_of(QueueClass::Text, "b"), Some(1));
        assert_eq!(scheduler.position_of(QueueClass::Text, "missing"), None);
        scheduler.enqueue(QueueClass::Formula, "f").expect("fits");
        assert_eq!(scheduler.position_of(QueueClass::Formula, "f"), Some(0));
    }

    /// 一轮的取法：先最多 4 个普通，再 1 个公式，然后开新一轮。
    #[test]
    fn a_round_serves_text_first_then_formula() {
        let config = SchedulerConfig::new(8, 2, 4, 1).expect("valid");
        let mut scheduler = DualQueueScheduler::new(config);
        for index in 0..8 {
            scheduler
                .enqueue(QueueClass::Text, format!("t{index}"))
                .expect("8 <= max_queue_text");
        }
        for index in 0..2 {
            scheduler
                .enqueue(QueueClass::Formula, format!("f{index}"))
                .expect("2 <= max_queue_formula");
        }
        let ids = take(
            &mut scheduler,
            &[
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Formula,
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Formula,
            ],
        );
        assert_eq!(ids[4], "f0", "the formula slot comes after 4 text jobs");
        assert_eq!(ids[9], "f1", "then a new round starts");
        assert!(scheduler.is_empty());
    }

    /// 一个队列为空时另一队列自由连续处理：即使配额是 1，空队列下的公式任务也连续跑完。
    #[test]
    fn an_empty_queue_lets_the_other_drain_freely() {
        let config = SchedulerConfig::new(4, 5, 4, 1).expect("valid");
        let mut scheduler = DualQueueScheduler::new(config);
        for index in 0..5 {
            scheduler
                .enqueue(QueueClass::Formula, format!("f{index}"))
                .expect("5 <= max_queue_formula");
        }
        take(
            &mut scheduler,
            &[
                QueueClass::Formula,
                QueueClass::Formula,
                QueueClass::Formula,
                QueueClass::Formula,
                QueueClass::Formula,
            ],
        );
        assert!(scheduler.is_empty());
    }

    /// 重新有任务的队列立刻被服务：`served_text >= T` 与 `served_formula >= F`
    /// 不可能同时持续存在，因此新到的任务最多等对方一个配额。
    #[test]
    fn a_refilled_queue_gets_service_without_waiting_for_a_full_quota() {
        let mut scheduler = default_scheduler();
        for index in 0..4 {
            scheduler
                .enqueue(QueueClass::Text, format!("t{index}"))
                .expect("fits");
        }
        take(
            &mut scheduler,
            &[
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Text,
                QueueClass::Text,
            ],
        );
        // 公式队列为空 → 直接自由处理。
        scheduler.enqueue(QueueClass::Formula, "f0").expect("fits");
        take(&mut scheduler, &[QueueClass::Formula]);
        // 普通队列刚重新有任务：立即服务，而不是先等公式配额。
        scheduler.enqueue(QueueClass::Text, "t4").expect("fits");
        take(&mut scheduler, &[QueueClass::Text]);
    }

    #[test]
    fn cancelling_a_queued_job_removes_it_and_keeps_order() {
        let mut scheduler = default_scheduler();
        scheduler.enqueue(QueueClass::Text, "a").expect("fits");
        scheduler.enqueue(QueueClass::Text, "b").expect("fits");
        scheduler.enqueue(QueueClass::Text, "c").expect("fits");
        assert!(scheduler.remove(QueueClass::Text, "b"));
        assert!(!scheduler.remove(QueueClass::Text, "b"), "already removed");
        assert!(!scheduler.remove(QueueClass::Formula, "a"), "wrong queue");
        assert_eq!(scheduler.queued_len(QueueClass::Text), 2);
        let ids = take(&mut scheduler, &[QueueClass::Text, QueueClass::Text]);
        assert_eq!(ids, vec!["a".to_string(), "c".to_string()]);
    }

    // -----------------------------------------------------------------------
    // §8.3 的双向公平性：长确定性序列 + 等待上界。
    // -----------------------------------------------------------------------

    /// 一个任务"等了多久"的口径：从入队到被服务之间，**另一类**被服务的次数。
    ///
    /// 用另一类的服务次数而不是墙钟时间：这是调度策略自身的性质（与推理耗时无关），
    /// 因此在单测里完全确定。从未被服务的任务把计数延续到运行结束，
    /// 于是"饿死"会表现为无界增长（数百），而不是侥幸通过。
    #[derive(Debug, Default, PartialEq, Eq)]
    struct FairnessRun {
        max_wait_text: usize,
        max_wait_formula: usize,
        served_text: usize,
        served_formula: usize,
        pending_text: usize,
        pending_formula: usize,
    }

    fn other_services(class: QueueClass, text: usize, formula: usize) -> usize {
        match class {
            QueueClass::Text => text,
            QueueClass::Formula => formula,
        }
    }

    /// 定向洪水驱动：`flood` 队列每一步都补满到容量，另一队列每一步恰好到达 1 个任务。
    ///
    /// 序列完全确定（固定步数与固定 id 生成），不依赖时间、线程或随机数。
    fn run_flood(config: SchedulerConfig, flood: QueueClass, steps: usize) -> FairnessRun {
        let mut scheduler = DualQueueScheduler::new(config);
        // 每个排队任务 → (类别, 入队时另一类已服务次数)
        let mut enqueued_at: HashMap<String, (QueueClass, usize)> = HashMap::new();
        let mut text_services = 0usize;
        let mut formula_services = 0usize;
        let mut report = FairnessRun::default();
        let mut seq = 0usize;

        for _ in 0..steps {
            while scheduler.queued_len(flood) < config.capacity(flood) {
                let id = format!("{}-flood-{seq}", flood.name());
                seq += 1;
                scheduler
                    .enqueue(flood, id.clone())
                    .expect("the flood queue was just topped up, so it cannot be full");
                enqueued_at.insert(
                    id,
                    (
                        flood,
                        other_services(flood, text_services, formula_services),
                    ),
                );
            }

            let trickle = flood.other();
            let id = format!("{}-trickle-{seq}", trickle.name());
            seq += 1;
            if scheduler.enqueue(trickle, id.clone()).is_ok() {
                enqueued_at.insert(
                    id,
                    (
                        trickle,
                        other_services(trickle, text_services, formula_services),
                    ),
                );
            }

            if let Some(job) = scheduler.take_next() {
                let (class, at) = enqueued_at
                    .remove(&job.id)
                    .expect("every served job was enqueued by this driver");
                assert_eq!(class, job.class);
                let waited = other_services(job.class, text_services, formula_services) - at;
                match job.class {
                    QueueClass::Text => {
                        text_services += 1;
                        report.served_text += 1;
                        report.max_wait_text = report.max_wait_text.max(waited);
                    }
                    QueueClass::Formula => {
                        formula_services += 1;
                        report.served_formula += 1;
                        report.max_wait_formula = report.max_wait_formula.max(waited);
                    }
                }
            }
        }

        // 仍在排队的任务：等待延续到运行结束（饥饿策略在这里暴露）。
        for (_, (class, at)) in enqueued_at {
            let waited = other_services(class, text_services, formula_services) - at;
            match class {
                QueueClass::Text => {
                    report.pending_text += 1;
                    report.max_wait_text = report.max_wait_text.max(waited);
                }
                QueueClass::Formula => {
                    report.pending_formula += 1;
                    report.max_wait_formula = report.max_wait_formula.max(waited);
                }
            }
        }
        report
    }

    /// 两个方向、两个类别共四条断言，用的是同一个可证明上界 [`SchedulerConfig::wait_bound`]。
    fn assert_fair(config: &SchedulerConfig, run: &FairnessRun) {
        for class in [QueueClass::Text, QueueClass::Formula] {
            let bound = config.wait_bound(class);
            let (waited, served, pending) = match class {
                QueueClass::Text => (run.max_wait_text, run.served_text, run.pending_text),
                QueueClass::Formula => (
                    run.max_wait_formula,
                    run.served_formula,
                    run.pending_formula,
                ),
            };
            assert!(
                waited <= bound,
                "{} job waited {waited} other-class services, bound is {bound}: {run:?}",
                class.name()
            );
            assert!(
                pending <= config.capacity(class),
                "{} backlog {pending} exceeds the queue capacity {}: {run:?}",
                class.name(),
                config.capacity(class)
            );
            assert!(served > 0, "{} never ran at all: {run:?}", class.name());
        }
    }

    /// 方向 1：普通任务洪水 → 公式任务的等待有上界（上一版会在这里失败）。
    #[test]
    fn a_text_flood_does_not_starve_formula_jobs() {
        let config = SchedulerConfig::default();
        let run = run_flood(config, QueueClass::Text, 400);
        eprintln!("text flood: {run:?}");
        assert!(
            run.served_formula > 50,
            "formula work must actually run: {run:?}"
        );
        assert_fair(&config, &run);
    }

    /// 方向 2：公式任务洪水 → 普通任务的等待有上界。
    #[test]
    fn a_formula_flood_does_not_starve_text_jobs() {
        let config = SchedulerConfig::default();
        let run = run_flood(config, QueueClass::Formula, 400);
        eprintln!("formula flood: {run:?}");
        assert!(
            run.served_text > 200,
            "text work must actually run: {run:?}"
        );
        assert_fair(&config, &run);
    }

    /// 最锋利的一条：普通队列**永不**为空时，一个公式任务也必须在
    /// `--max-consecutive-text` 个普通任务之内被服务（§8.3 的保底规则）。
    ///
    /// 这正是"先做普通，普通为空才做公式"的朴素策略做不到的事：
    /// 朴素策略下这个公式任务**永远不会**被服务（`wanted` 保持 `None`，测试失败）。
    #[test]
    fn a_single_formula_job_waits_at_most_the_text_quota() {
        let config = SchedulerConfig::default();
        let mut scheduler = DualQueueScheduler::new(config);
        let mut waited = None;
        let mut text_served_since_arrival = 0usize;

        for step in 0..64 {
            // 普通队列始终保持非空（持续灌入）。
            while scheduler.queued_len(QueueClass::Text) < config.max_queue_text {
                let next = scheduler.queued_len(QueueClass::Text);
                scheduler
                    .enqueue(QueueClass::Text, format!("t{step}-{next}"))
                    .expect("the text queue is below capacity");
            }
            if step == 3 {
                scheduler
                    .enqueue(QueueClass::Formula, "the-only-formula-job")
                    .expect("the formula queue is empty");
                text_served_since_arrival = 0;
            }
            match scheduler
                .take_next()
                .expect("text is always available")
                .class
            {
                QueueClass::Text => {
                    if waited.is_none() {
                        text_served_since_arrival += 1;
                    }
                }
                QueueClass::Formula => {
                    waited = Some(text_served_since_arrival);
                    break;
                }
            }
        }

        let waited = waited.expect(
            "the formula job must be served even while the text queue is never empty (§8.3)",
        );
        assert!(
            waited <= config.max_consecutive_text,
            "the formula job waited {waited} text services, quota is {}",
            config.max_consecutive_text
        );
    }

    /// 镜像方向：公式队列**永不**为空时，一个普通任务也必须被服务（上界为公式配额）。
    #[test]
    fn a_single_text_job_waits_at_most_the_formula_quota() {
        let config = SchedulerConfig::default();
        let mut scheduler = DualQueueScheduler::new(config);
        let mut waited = None;
        let mut formula_served_since_arrival = 0usize;

        for step in 0..64 {
            while scheduler.queued_len(QueueClass::Formula) < config.max_queue_formula {
                let next = scheduler.queued_len(QueueClass::Formula);
                scheduler
                    .enqueue(QueueClass::Formula, format!("f{step}-{next}"))
                    .expect("the formula queue is below capacity");
            }
            if step == 2 {
                scheduler
                    .enqueue(QueueClass::Text, "the-only-text-job")
                    .expect("the text queue is empty");
                formula_served_since_arrival = 0;
            }
            match scheduler
                .take_next()
                .expect("formula is always available")
                .class
            {
                QueueClass::Formula => {
                    if waited.is_none() {
                        formula_served_since_arrival += 1;
                    }
                }
                QueueClass::Text => {
                    waited = Some(formula_served_since_arrival);
                    break;
                }
            }
        }

        let waited = waited.expect(
            "the text job must be served even while the formula queue is never empty (§8.3)",
        );
        assert!(
            waited <= config.max_consecutive_formula,
            "the text job waited {waited} formula services, quota is {}",
            config.max_consecutive_formula
        );
    }

    /// 非默认配额同样成立（不是把 4/1 写死的结果）。
    #[test]
    fn fairness_holds_for_non_default_quotas() {
        let config = SchedulerConfig::new(4, 2, 2, 3).expect("valid");
        let text_flood = run_flood(config, QueueClass::Text, 200);
        eprintln!("text flood (2/3): {text_flood:?}");
        assert_fair(&config, &text_flood);

        let formula_flood = run_flood(config, QueueClass::Formula, 200);
        eprintln!("formula flood (2/3): {formula_flood:?}");
        assert_fair(&config, &formula_flood);
    }
}
