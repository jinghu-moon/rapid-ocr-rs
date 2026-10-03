//! 统一的运行时档案：把 ORT 线程、Rayon 线程与公式批大小收敛成**一份**可解释的策略。
//!
//! # 为什么需要它
//!
//! 重构前 `det.runtime` / `cls.runtime` / `rec.runtime` 是三份**总是相同**的
//! [`RuntimeConfig`]：每个 ORT 会话各自按 `auto_tune_threads` 调优到
//! `min(可用并行度, 物理核数)`，Rayon 走的是另一条默认路径（未显式设置时直接用
//! 逻辑核数），而全局线程池初始化失败被 `let _ = builder.build_global();` 静默吞掉。
//! 于是“这个进程到底用了多少线程”没有任何单一解释处：必须同时读三份配置、ORT 的
//! 自动调优规则和 Rayon 的默认规则，而且失败是无声的。
//!
//! 现在只有一份运行时配置（`EngineConfig::runtime`），由 [`RuntimeProfile::resolve`]
//! 解析成 [`ThreadPlan`]；三个 ORT 会话都从 [`RuntimeProfile::session_runtime`] 取设置，
//! Rayon 全局线程池也由同一个 plan 初始化并且**必须成功**（显式请求无法生效时返回
//! [`RapidOcrError::Config`]，不再静默）。
//!
//! # 解析规则（全部由本模块的测试覆盖）
//!
//! plan 里三个线程字段都是 `Option<usize>`，`None` 的语义是**不配置**：
//! 交给 ORT / Rayon 自己的默认值，而不是替它们编一个数字。
//!
//! - `budget` = `min(std::thread::available_parallelism(), num_cpus::get_physical())`，
//!   至少 1。
//! - 显式值优先：`intra_threads` / `inter_threads` / `rayon_threads` 只要给了 `Some(n)`
//!   且 `n > 0`，就原样进入 plan（`ort_intra` 显式时 `source = Explicit`）。
//! - 其余字段取决于 `auto_tune_threads`（这是公开字段的既有含义，
//!   **`false` 必须表示“不要自动配置”**）：
//!   - `true` → 自动策略：`ort_intra = Some(budget)`、`ort_inter = Some(1)`
//!     （ORT 的 inter 并行对本 workload 无益）、`rayon = Some(clamp(budget / 4, 1, 8))`；
//!   - `false` → 三个字段全为 `None`，一个都不配置。
//! - `sessions`：分类器启用时 3（det + cls + rec），否则 2（det + rec）。
//!
//! `session_runtime()` 返回的 [`RuntimeConfig`] 里 `auto_tune_threads = false`：线程数
//! 已经在 plan 里一次算清楚，再让 ORT 自动调优就会重新引入第二套规则。
//! plan 里的 `None` 会原样透传给 `OrtSession`（即不调用 `with_intra_threads` /
//! `with_inter_threads`），因此“未配置”在执行侧也是真的未配置。
//!
//! # 为什么 plan 必须是 `Option`
//!
//! `auto_tune_threads = false` 曾经被本模块忽略：这里无条件算出
//! `ort_intra = budget` 并写进 `session_runtime()`，而 `runtime/session.rs` 当时的
//! `derive_runtime_threads` 却认为 `false` 表示“不自动配置 ORT 线程”。同一份
//! [`RuntimeConfig`] 于是通过 `RapidOcrEngine` 和通过 `FormulaSession` /
//! `formula_bench` / `formula_eval` 会得到不同行为。现在只有这一份解析实现
//! （`runtime/session.rs` 的重复逻辑已删除），`None` 让“未配置”成为可表达、可报告的状态。
//!
//! # 这是简化，不是加速
//!
//! 本机实测矩阵（12 张真实页面，`max_side_len = 2000`，
//! `tests/baseline/windows-baseline/thread-matrix.json`）：
//!
//! | ORT intra | Rayon | p50 (ms) |
//! | --------- | ----- | -------- |
//! | 16        | 16    | 978.6    |
//! | 16        | 4     | 969.4    |
//! | 8         | 8     | 964.5    |
//! | 4         | 8     | 920.0    |
//! | 8         | 4     | 1114.8   |
//!
//! 这些差异落在这台机器的 run-to-run 噪声里（±10–15%，同一份二进制之间也曾相差 39%）。
//! 因此本模块是**用一条可解释的策略做的简化**，**不声称任何加速**，也没有任何
//! 性能声明依赖它；这里唯一的收益是“线程设置只有一个解释处，且初始化失败不再静默”。

use serde::{Deserialize, Serialize};

use crate::{
    config::{ProviderPreference, RuntimeConfig},
    error::{RapidOcrError, Result},
};

/// 自动调优时的线程预算：`min(逻辑核数, 物理核数)`，至少 1。
///
/// 这是本 crate 里**唯一**的线程预算定义处：它只被 [`RuntimeProfile::plan`] 使用，
/// 也是“这个进程按自动策略会用多少线程”的唯一解释。
fn auto_tuned_thread_budget() -> usize {
    let physical_cores = num_cpus::get_physical().max(1);
    let available = std::thread::available_parallelism()
        .ok()
        .map(|value| value.get())
        .unwrap_or(1);
    available.clamp(1, physical_cores)
}

/// ORT intra 线程数的来源：用户显式指定，还是本机预算自动推导。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSource {
    Auto,
    Explicit,
}

/// 一次解析的线程分配结果（可序列化，便于基准报告直接内嵌）。
///
/// `None` 表示**该值未被配置**，运行时会使用 ORT / Rayon 自己的默认值；报告里的
/// `null` 就是这个意思，不是“0 线程”，也不是“未知”。
///
/// `rayon` 是**生效值**：如果进程里已经存在 Rayon 全局线程池（Rayon 只允许一个），
/// [`RuntimeProfile::resolve`] 会把它改写成实际的池大小；未配置（`None`）时不会去改
/// 任何东西，也不会因为“已有池”而报错。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPlan {
    pub source: ThreadSource,
    /// 用于分配的线程总预算。
    pub budget: usize,
    /// 每个 ORT 会话的 intra 线程数；`None` = 不配置，用 ORT 默认值。
    pub ort_intra: Option<usize>,
    /// 每个 ORT 会话的 inter 线程数；`None` = 不配置，用 ORT 默认值。
    pub ort_inter: Option<usize>,
    /// Rayon 全局线程池的线程数（生效值）；`None` = 不配置，用 Rayon 默认值。
    pub rayon: Option<usize>,
    /// 共享该预算的 ORT 会话数量。
    pub sessions: usize,
}

/// 解析后的运行时档案：provider 选择、内存 arena 策略、公式批大小与线程 plan。
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeProfile {
    pub provider: ProviderPreference,
    pub cpu_mem_arena: bool,
    pub fail_if_provider_unavailable: bool,
    pub formula_batch: usize,
    pub threads: ThreadPlan,
}

impl RuntimeProfile {
    /// 解析配置并初始化 Rayon 全局线程池；线程池无法满足显式请求时返回错误。
    pub fn resolve(runtime: &RuntimeConfig, classifier_enabled: bool) -> Result<Self> {
        let mut profile = Self::plan(runtime, classifier_enabled);
        profile.apply_rayon_global_pool()?;
        Ok(profile)
    }

    /// 纯函数部分：只根据配置与本机预算推导 plan，不产生任何副作用。
    ///
    /// 显式值永远优先；未被显式指定的字段在 `auto_tune_threads = true` 时按自动策略
    /// 填充，在 `false` 时保持 `None`（不配置）。
    pub fn plan(runtime: &RuntimeConfig, classifier_enabled: bool) -> Self {
        let budget = auto_tuned_thread_budget().max(1);
        let explicit_intra = runtime.intra_threads.filter(|value| *value > 0);
        let explicit_inter = runtime.inter_threads.filter(|value| *value > 0);
        let explicit_rayon = runtime.rayon_threads.filter(|value| *value > 0);
        let source = if explicit_intra.is_some() {
            ThreadSource::Explicit
        } else {
            ThreadSource::Auto
        };
        // `auto_tune_threads = false` 表示“不要自动配置”：三个字段保持 `None`，
        // 由 ORT / Rayon 使用自己的默认值。这是公开字段的既有含义，必须在这里生效，
        // 否则同一份 `RuntimeConfig` 在引擎路径与公式路径上会得到不同行为。
        let auto = runtime.auto_tune_threads;
        let ort_intra = explicit_intra.or(auto.then_some(budget));
        let ort_inter = explicit_inter.or(auto.then_some(1));
        let rayon = explicit_rayon.or_else(|| auto.then(|| (budget / 4).clamp(1, 8)));
        Self {
            provider: runtime.provider_preference,
            cpu_mem_arena: runtime.enable_cpu_mem_arena,
            fail_if_provider_unavailable: runtime.fail_if_provider_unavailable,
            formula_batch: runtime.formula_batch,
            threads: ThreadPlan {
                source,
                budget,
                ort_intra,
                ort_inter,
                rayon,
                sessions: if classifier_enabled { 3 } else { 2 },
            },
        }
    }

    /// 初始化 Rayon 全局线程池，并把 `threads.rayon` 修正为**生效值**。
    ///
    /// Rayon 每个进程只允许一个全局线程池，`build_global()` 在第二次调用（或进程里
    /// 已有池）时必然失败，所以失败本身不是错误：
    ///
    /// - `rayon = None`（未配置）→ **什么都不做**：不去建立池，也不把“已经存在池”
    ///   当成错误。请求方明确说过不要配置，这里就不能替它做主；
    /// - 池不存在 → 按 plan 建立，`rayon` 保持请求值；
    /// - 池已存在且线程数与请求一致 → 成功；
    /// - 池已存在且线程数不同：`ThreadSource::Explicit`（用户显式指定过线程数）时返回
    ///   [`RapidOcrError::Config`] 说明无法生效；`Auto` 时接受已有池并把 `rayon`
    ///   改写成实际值，让报告与执行保持一致。
    pub fn apply_rayon_global_pool(&mut self) -> Result<()> {
        let Some(requested) = self.threads.rayon else {
            return Ok(());
        };
        if rayon::ThreadPoolBuilder::new()
            .num_threads(requested)
            .build_global()
            .is_ok()
        {
            return Ok(());
        }

        let actual = rayon::current_num_threads();
        if actual == requested {
            return Ok(());
        }
        if self.threads.source == ThreadSource::Explicit {
            return Err(RapidOcrError::Config(format!(
                "the Rayon global thread pool is already initialised with {actual} threads, \
                 so the requested rayon_threads = {requested} cannot be applied; Rayon allows \
                 only one global pool per process"
            )));
        }
        self.threads.rayon = Some(actual);
        Ok(())
    }

    /// 单个 ORT 会话的运行时设置：三个阶段（以及公式会话）都从这里取。
    ///
    /// 三个阶段**刻意共享同一个 plan**：它们总是同时运行在同一条管线上，分别设置只会
    /// 让总线程数失控。这里**刻意不提供** per-stage override——当前没有这个需求；
    /// 将来若真的需要，正确的做法是加一个显式的 override 字段，而不是把整份 runtime
    /// 配置重新复制三份。
    ///
    /// plan 里的 `None` 原样透传，因此 `auto_tune_threads = false` 且没有显式值时，
    /// `OrtSession` 看到的是 `None`（不调用 `with_*_threads`），ORT 保留自己的默认值。
    pub fn session_runtime(&self) -> RuntimeConfig {
        RuntimeConfig {
            intra_threads: self.threads.ort_intra,
            inter_threads: self.threads.ort_inter,
            // plan 已经把该算的都算完了；这里再让 ORT 自动调优就会重新引入第二套规则。
            auto_tune_threads: false,
            rayon_threads: self.threads.rayon,
            enable_cpu_mem_arena: self.cpu_mem_arena,
            fail_if_provider_unavailable: self.fail_if_provider_unavailable,
            provider_preference: self.provider,
            formula_batch: self.formula_batch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeProfile, ThreadPlan, ThreadSource, auto_tuned_thread_budget};
    use crate::{
        config::{ProviderPreference, RuntimeConfig},
        error::RapidOcrError,
    };

    /// 显式设置必须逐字段生效，且不依赖本机核数。
    #[test]
    fn explicit_values_win_and_are_marked_explicit() {
        let runtime = RuntimeConfig {
            intra_threads: Some(6),
            inter_threads: Some(2),
            auto_tune_threads: false,
            rayon_threads: Some(3),
            enable_cpu_mem_arena: false,
            fail_if_provider_unavailable: true,
            provider_preference: ProviderPreference::Cpu,
            formula_batch: 5,
        };
        let profile = RuntimeProfile::plan(&runtime, false);
        assert_eq!(profile.threads.source, ThreadSource::Explicit);
        assert_eq!(profile.threads.ort_intra, Some(6));
        assert_eq!(profile.threads.ort_inter, Some(2));
        assert_eq!(profile.threads.rayon, Some(3));
        assert_eq!(profile.threads.sessions, 2);
        assert_eq!(profile.formula_batch, 5);
        assert!(!profile.cpu_mem_arena);
        assert!(profile.fail_if_provider_unavailable);

        assert_eq!(
            RuntimeProfile::plan(&runtime, true).threads.sessions,
            3,
            "enabling the classifier adds its session to the shared budget"
        );
    }

    /// auto 路径的数值规则：intra = 预算，inter = 1，rayon = clamp(预算/4, 1, 8)。
    #[test]
    fn auto_values_follow_the_documented_formula() {
        let budget = auto_tuned_thread_budget().max(1);
        let profile = RuntimeProfile::plan(&RuntimeConfig::default(), false);
        assert_eq!(profile.threads.source, ThreadSource::Auto);
        assert_eq!(profile.threads.budget, budget);
        assert_eq!(profile.threads.ort_intra, Some(budget));
        assert_eq!(profile.threads.ort_inter, Some(1));
        assert_eq!(profile.threads.rayon, Some((budget / 4).clamp(1, 8)));
        assert!(
            (1..=8).contains(&profile.threads.rayon.expect("auto plan sets rayon")),
            "the auto Rayon share must stay small and bounded"
        );
    }

    /// **P1-2 根因回归**：`auto_tune_threads = false` 且没有任何显式值时，三个线程字段
    /// 必须都是 `None`（不配置），而不是被 profile 悄悄填成自动策略的数字。
    ///
    /// 旧行为：`ort_intra = Some(budget)`、`rayon = Some(clamp(budget/4,1,8))`，
    /// `session_runtime()` 把它们当成显式值下发 —— 于是公开字段
    /// `auto_tune_threads = false` 在引擎路径上被完全忽略。
    #[test]
    fn auto_tune_disabled_configures_nothing() {
        let runtime = RuntimeConfig {
            auto_tune_threads: false,
            ..RuntimeConfig::default()
        };
        let profile = RuntimeProfile::plan(&runtime, false);
        assert_eq!(profile.threads.source, ThreadSource::Auto);
        assert_eq!(profile.threads.ort_intra, None);
        assert_eq!(profile.threads.ort_inter, None);
        assert_eq!(profile.threads.rayon, None);

        // 透传到会话必须仍然是 `None`：`OrtSession` 只有拿到 `Some` 才会调用
        // `with_intra_threads` / `with_inter_threads`。
        let session = profile.session_runtime();
        assert_eq!(session.intra_threads, None);
        assert_eq!(session.inter_threads, None);
        assert_eq!(session.rayon_threads, None);
        // plan 已经解析完毕，ORT 自己那套 auto_tune 也必须关掉，避免第二套规则。
        assert!(!session.auto_tune_threads);
    }

    /// `auto_tune_threads = false` 与显式 intra 混用：显式值仍然生效，其余保持未配置。
    #[test]
    fn auto_tune_disabled_keeps_explicit_intra_only() {
        let runtime = RuntimeConfig {
            intra_threads: Some(6),
            auto_tune_threads: false,
            ..RuntimeConfig::default()
        };
        let profile = RuntimeProfile::plan(&runtime, false);
        assert_eq!(profile.threads.source, ThreadSource::Explicit);
        assert_eq!(profile.threads.ort_intra, Some(6));
        assert_eq!(profile.threads.ort_inter, None);
        assert_eq!(profile.threads.rayon, None);

        let session = profile.session_runtime();
        assert_eq!(session.intra_threads, Some(6));
        assert_eq!(session.inter_threads, None);
        assert_eq!(session.rayon_threads, None);
    }

    /// `auto_tune_threads = false` 且 `rayon` 未配置时，`resolve` 不得建立全局池，
    /// 也不得因为“进程里已经有池”而报错 —— 请求方明确说过不要配置。
    #[test]
    fn unconfigured_rayon_is_left_alone_even_with_an_existing_pool() {
        let _ = rayon::ThreadPoolBuilder::new().build_global();
        let before = rayon::current_num_threads();
        let runtime = RuntimeConfig {
            auto_tune_threads: false,
            ..RuntimeConfig::default()
        };
        let profile = RuntimeProfile::resolve(&runtime, false)
            .expect("not configuring Rayon must never fail, even if a pool already exists");
        assert_eq!(profile.threads.rayon, None);
        assert_eq!(
            rayon::current_num_threads(),
            before,
            "an unconfigured plan must not resize the live pool"
        );
    }

    /// `0` 与 `None` 在解析层等价于“未设置”，不能让 0 线程进入任何会话。
    #[test]
    fn zero_is_treated_as_unset() {
        let runtime = RuntimeConfig {
            intra_threads: Some(0),
            inter_threads: Some(0),
            rayon_threads: Some(0),
            auto_tune_threads: true,
            ..RuntimeConfig::default()
        };
        let profile = RuntimeProfile::plan(&runtime, false);
        assert_eq!(profile.threads.source, ThreadSource::Auto);
        assert!(profile.threads.ort_intra.is_some_and(|value| value >= 1));
        assert_eq!(profile.threads.ort_inter, Some(1));
        assert!(profile.threads.rayon.is_some_and(|value| value >= 1));
    }

    /// `session_runtime()` 是三个阶段的唯一来源：auto_tune 必须关闭，线程数必须来自 plan。
    #[test]
    fn session_runtime_reflects_the_plan() {
        let runtime = RuntimeConfig {
            intra_threads: Some(5),
            inter_threads: None,
            auto_tune_threads: true,
            rayon_threads: None,
            enable_cpu_mem_arena: false,
            fail_if_provider_unavailable: true,
            provider_preference: ProviderPreference::Cpu,
            formula_batch: 9,
        };
        let profile = RuntimeProfile::plan(&runtime, false);
        let session = profile.session_runtime();
        assert_eq!(session.intra_threads, Some(5));
        assert_eq!(session.inter_threads, Some(1));
        assert_eq!(session.rayon_threads, profile.threads.rayon);
        assert!(
            !session.auto_tune_threads,
            "ORT must not re-derive threads after the plan was resolved"
        );
        assert!(!session.enable_cpu_mem_arena);
        assert!(session.fail_if_provider_unavailable);
        assert_eq!(session.provider_preference, ProviderPreference::Cpu);
        assert_eq!(session.formula_batch, 9);
    }

    /// 线程 plan 必须能直接内嵌进基准报告，并且“未配置”必须序列化成 `null`
    /// （而不是 0 或缺失字段），否则报告会说谎。
    #[test]
    fn thread_plan_serialises_for_reports() {
        let plan = ThreadPlan {
            source: ThreadSource::Auto,
            budget: 14,
            ort_intra: Some(14),
            ort_inter: Some(1),
            rayon: Some(3),
            sessions: 2,
        };
        let value = serde_json::to_value(&plan).expect("thread plan must serialise");
        assert_eq!(value["source"], serde_json::json!("auto"));
        assert_eq!(value["budget"], serde_json::json!(14));
        assert_eq!(value["rayon"], serde_json::json!(3));
        let round_trip: ThreadPlan =
            serde_json::from_value(value).expect("thread plan must deserialise");
        assert_eq!(round_trip, plan);

        let unconfigured = ThreadPlan {
            source: ThreadSource::Auto,
            budget: 14,
            ort_intra: None,
            ort_inter: None,
            rayon: None,
            sessions: 2,
        };
        let value = serde_json::to_value(&unconfigured).expect("thread plan must serialise");
        assert_eq!(value["ort_intra"], serde_json::Value::Null);
        assert_eq!(value["ort_inter"], serde_json::Value::Null);
        assert_eq!(value["rayon"], serde_json::Value::Null);

        let explicit = serde_json::to_value(ThreadSource::Explicit).expect("source serialises");
        assert_eq!(explicit, serde_json::json!("explicit"));
    }

    /// 显式请求与“已经存在的全局线程池”冲突时必须**报错**，不能静默换数字。
    ///
    /// 顺序无关：先把全局池确定下来（可能是本测试建的，也可能早就被别的测试建好了），
    /// 再请求一个必然不同的值。`build_global` 每个进程只成功一次，所以这条路径在
    /// 任何执行顺序下都会命中。
    #[test]
    fn explicit_rayon_request_conflicting_with_an_existing_pool_is_reported() {
        let _ = rayon::ThreadPoolBuilder::new().build_global();
        let actual = rayon::current_num_threads();
        assert!(actual > 0, "rayon must report a live global pool");
        let requested = actual + 1;

        let runtime = RuntimeConfig {
            intra_threads: Some(requested),
            inter_threads: Some(1),
            auto_tune_threads: false,
            rayon_threads: Some(requested),
            ..RuntimeConfig::default()
        };
        let error = RuntimeProfile::resolve(&runtime, false)
            .expect_err("an explicit Rayon request that cannot be applied must fail");
        assert!(
            matches!(error, RapidOcrError::Config(_)),
            "unexpected error: {error}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&actual.to_string()) && message.contains(&requested.to_string()),
            "the error must name both the actual and the requested thread counts: {message}"
        );
    }

    /// auto plan 容忍已经存在的全局池，但必须把实际值写进 plan（报告不能撒谎）。
    #[test]
    fn auto_plan_adopts_and_reports_an_existing_pool() {
        let _ = rayon::ThreadPoolBuilder::new().build_global();
        let profile = RuntimeProfile::resolve(&RuntimeConfig::default(), false)
            .expect("the auto plan must tolerate an existing global pool");
        assert_eq!(
            profile.threads.rayon,
            Some(rayon::current_num_threads()),
            "the reported Rayon value must be the effective one"
        );
        assert_eq!(
            profile.session_runtime().rayon_threads,
            profile.threads.rayon
        );
    }

    /// 显式请求与已存在的池**线程数相同**时必须成功：值相同就没有冲突可言。
    ///
    /// 同样顺序无关：先读出现有池的实际线程数，再显式请求同一个值。
    #[test]
    fn explicit_request_matching_the_existing_pool_is_accepted() {
        let _ = rayon::ThreadPoolBuilder::new().build_global();
        let actual = rayon::current_num_threads();
        let runtime = RuntimeConfig {
            intra_threads: Some(actual),
            inter_threads: Some(1),
            auto_tune_threads: false,
            rayon_threads: Some(actual),
            ..RuntimeConfig::default()
        };
        let profile = RuntimeProfile::resolve(&runtime, false)
            .expect("an explicit request equal to the live pool size must be accepted");
        assert_eq!(profile.threads.source, ThreadSource::Explicit);
        assert_eq!(profile.threads.rayon, Some(actual));
        assert_eq!(profile.session_runtime().rayon_threads, Some(actual));
    }
}
