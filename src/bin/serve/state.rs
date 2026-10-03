//! 服务状态与引擎状态机（§7.5、§7.6）。
//!
//! # 为什么要拆成两个状态
//!
//! 服务可用性**不依赖模型**：模型缺失时服务必须正常启动，让用户能看到
//! `/api/models`、`/api/status` 并（显式）触发下载；只有**引擎**依赖模型与 provider。
//! 因此 `ServiceState`（监听是否成功）与 `EngineState`（会话是否可用）是两个独立维度。
//!
//! # 启动顺序（§7.6，M0c 已把其中的状态语义固定下来）
//!
//! 1. 绑定硬编码的 `127.0.0.1`（[`super::security`]）→ 失败即退出；
//! 2. **校验运行配置**（[`ServeStartup::validate`]）：各资源上限取值合法 → provider
//!    名称与对应 feature 是否编译进来 → 引擎配置自身合法；任一非法立即退出。
//!    措辞全部来自库里的既有实现（[`ServeConfigPlan`] 使用 `runtime::provider`），
//!    不另写一套；
//! 3. 检查模型集状态（[`ModelReadiness`]）：
//!    - 不齐备 → 服务正常启动，`EngineState::BlockedModelsMissing`；
//!    - 齐备 → 预加载 `Loading` → `Ready`（provider 不可用则 `Failed`，原因写入 `/api/status`）。
//!
//! # 合法转换（**唯一**表）
//!
//! | from | to | 触发 |
//! | --- | --- | --- |
//! | `BlockedModelsMissing` | `BlockedModelsMissing` | 重载时模型仍不齐备（刷新缺失清单） |
//! | `BlockedModelsMissing` | `Loading` | 模型齐备（显式 reload，或下一次 OCR 时惰性创建） |
//! | `Failed` | `Loading` | 显式 `POST /api/engine/reload` |
//! | `Ready` | `Loading` | 显式 `POST /api/engine/reload` |
//! | `Ready` | `Rebuilding` | 运行期切换 provider（M3）：暂停新任务并排空 |
//! | `Rebuilding` | `Ready` | M3：新引擎创建成功（或恢复旧引擎） |
//! | `Rebuilding` | `Failed` | M3：切换失败且旧引擎无法恢复 |
//! | `Loading` | `Ready` | 会话创建成功 |
//! | `Loading` | `Failed` | 会话创建失败（含 provider 不可用） |
//!
//! 不在这张表里的转换一律是 [`TransitionError`]（带 from/to 与合法前驱，便于定位）。
//!
//! # OCR 准入（§7.6 的运行期语义）
//!
//! - `Ready` → 立即执行；
//! - `Loading` / `Rebuilding` → **入队等待**，不失败；
//! - `BlockedModelsMissing` → 409 `models_missing`；
//! - `Failed` → 503 `engine_unavailable`，且**必须**带上 `reason`。

use rapid_ocr_rs::{EngineConfig, ProviderPreference, RapidOcrError};
use serde::Serialize;

use super::limits::{RawServeLimits, ServeConfigError, ServeLimits};

/// 服务状态：监听成功即为 `Ready`（§7.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Starting,
    Ready,
}

impl ServiceState {
    /// `Starting → Ready`。重复转换为非法（避免"启动两次"这类逻辑错误被静默吞掉）。
    pub fn listening_succeeded(self) -> Result<Self, TransitionError> {
        match self {
            Self::Starting => Ok(Self::Ready),
            Self::Ready => Err(TransitionError {
                from: "ready",
                to: "ready",
                allowed_from: "starting".to_string(),
            }),
        }
    }
}

/// 引擎状态（§7.6 原文的字段集合，逐字段保留）。
///
/// 序列化结果直接用于 `/api/status` 的引擎字段：内部标记 `state` +
/// 各状态自己的字段（`missing` / `requested` / `selected_ep` / `fallback_to_cpu` / `reason`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EngineState {
    /// 模型缺失，尚未创建会话。
    BlockedModelsMissing { missing: Vec<String> },
    /// 正在创建会话。
    Loading,
    /// 会话可用。
    Ready {
        requested: String,
        selected_ep: String,
        fallback_to_cpu: bool,
    },
    /// 创建失败（含 provider 不可用）。
    Failed { reason: String },
    /// 运行期切换 provider（M3）。
    ///
    /// 本变体及 [`EngineStateMachine::begin_rebuild`] 是 §7.6 冻结的状态机的一部分，
    /// 有完整的转换测试，但**生产者**（M3 的运行期 provider 切换）尚未落地，
    /// 因此目前没有构造点。M2 的 `POST /api/engine/reload` **不**经过它：
    /// 显式 reload 是"按磁盘上的当前文件重建会话"，与"切换 provider"是两件事。
    #[allow(dead_code)]
    Rebuilding,
}

/// 状态种类（无载荷），用于转换检查与错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateKind {
    BlockedModelsMissing,
    Loading,
    Ready,
    Failed,
    Rebuilding,
}

impl StateKind {
    fn name(self) -> &'static str {
        match self {
            Self::BlockedModelsMissing => "blocked_models_missing",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Rebuilding => "rebuilding",
        }
    }
}

impl EngineState {
    fn kind(&self) -> StateKind {
        match self {
            Self::BlockedModelsMissing { .. } => StateKind::BlockedModelsMissing,
            Self::Loading => StateKind::Loading,
            Self::Ready { .. } => StateKind::Ready,
            Self::Failed { .. } => StateKind::Failed,
            Self::Rebuilding => StateKind::Rebuilding,
        }
    }

    /// 状态名（与序列化里的 `state` 字段同值）。
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }

    /// `/api/status` 的三个 provider 字段（§7.5：**始终**同时给出）。
    ///
    /// 未进入 `Ready` 时 `selected_ep` / `fallback_to_cpu` 是 `null`：
    /// **未知态不得伪装成 `false`**——`false` 是"确认没有回退"这一结论，
    /// 只有真的建立过会话才配得上它。
    pub fn provider_status(&self, requested: &str) -> ProviderStatus {
        match self {
            Self::Ready {
                requested: state_requested,
                selected_ep,
                fallback_to_cpu,
            } => ProviderStatus {
                requested: state_requested.clone(),
                selected_ep: Some(selected_ep.clone()),
                fallback_to_cpu: Some(*fallback_to_cpu),
            },
            _ => ProviderStatus {
                requested: requested.to_string(),
                selected_ep: None,
                fallback_to_cpu: None,
            },
        }
    }

    /// 该状态下 `/api/ocr` 的准入结论（排队 / 拒绝语义见模块文档）。
    pub fn ocr_admission(&self) -> OcrAdmission {
        match self {
            Self::Ready { .. } => OcrAdmission::Run,
            Self::BlockedModelsMissing { missing } => OcrAdmission::ModelsMissing {
                missing: missing.clone(),
            },
            Self::Loading => OcrAdmission::Queue {
                waiting_for: "loading",
            },
            Self::Rebuilding => OcrAdmission::Queue {
                waiting_for: "rebuilding",
            },
            Self::Failed { reason } => OcrAdmission::Unavailable {
                reason: reason.clone(),
            },
        }
    }
}

/// `/api/status` 的 provider 三字段。
///
/// 即使未知也必须序列化出键（值为 `null`），否则前端无法区分
/// "没有这个字段"和"还不知道"；§7.5 明确禁止把未知态显示成 `false`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub requested: String,
    pub selected_ep: Option<String>,
    pub fallback_to_cpu: Option<bool>,
}

/// `/api/ocr` 的准入结论（§7.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrAdmission {
    /// 引擎就绪，立即执行。
    Run,
    /// 引擎正在加载/重建：任务入队等待，**不失败**。
    Queue { waiting_for: &'static str },
    /// 模型不齐备：409 `models_missing`，缺失清单与 `/api/models` 一致。
    ModelsMissing { missing: Vec<String> },
    /// 引擎不可用：503 `engine_unavailable` + `reason`。
    Unavailable { reason: String },
}

/// 模型完备性（库侧 `ModelSet`/`ModelSetStatus` 落地前的本地接缝）。
///
/// M0c **不**解析模型清单（那是 M0a 的 `ModelSet` / 共享逐文件校验函数）；
/// 状态机只接收"是否齐备、缺哪些文件"这一个事实，因此状态机可以脱离文件系统单测。
/// M1 的填充点：调用共享校验函数后，把 `Missing`/`Corrupt` 的文件名放进
/// [`ModelReadiness::Incomplete`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelReadiness {
    Complete,
    Incomplete { missing: Vec<String> },
}

/// 非法状态转换（带 from/to 与合法前驱，可定位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    from: &'static str,
    to: &'static str,
    allowed_from: String,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal engine state transition {} -> {}: {} may only be entered from [{}]",
            self.from, self.to, self.to, self.allowed_from
        )
    }
}

impl std::error::Error for TransitionError {}

/// 引擎状态机：唯一持有 [`EngineState`] 并可执行合法转换的地方。
#[derive(Debug, Clone)]
pub struct EngineStateMachine {
    state: EngineState,
}

impl EngineStateMachine {
    /// 启动期（§7.6 第 3 步）：模型不齐备 → `BlockedModelsMissing`；齐备 → `Loading`。
    pub fn start(readiness: ModelReadiness) -> Self {
        match readiness {
            ModelReadiness::Complete => Self {
                state: EngineState::Loading,
            },
            ModelReadiness::Incomplete { missing } => Self {
                state: EngineState::BlockedModelsMissing { missing },
            },
        }
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    /// 进入 `Loading`：`BlockedModelsMissing`（模型齐备后重载）| `Ready` | `Failed`。
    ///
    /// 启动期**不需要**它：模型齐备时 [`EngineStateMachine::start`] 已经进入 `Loading`。
    /// **生产者（M2）**：`POST /api/ocr` 的惰性准入（模型刚齐备）与
    /// `POST /api/engine/reload`（见 `server.rs::ensure_engine_loaded`）。
    pub fn begin_loading(&mut self) -> Result<(), TransitionError> {
        self.require(
            &[
                StateKind::BlockedModelsMissing,
                StateKind::Ready,
                StateKind::Failed,
            ],
            StateKind::Loading,
        )?;
        self.state = EngineState::Loading;
        Ok(())
    }

    /// `Loading`（或 M3 的 `Rebuilding`）→ `Ready`。
    pub fn load_succeeded(
        &mut self,
        requested: impl Into<String>,
        selected_ep: impl Into<String>,
        fallback_to_cpu: bool,
    ) -> Result<(), TransitionError> {
        self.require(
            &[StateKind::Loading, StateKind::Rebuilding],
            StateKind::Ready,
        )?;
        self.state = EngineState::Ready {
            requested: requested.into(),
            selected_ep: selected_ep.into(),
            fallback_to_cpu,
        };
        Ok(())
    }

    /// `Loading`（或 M3 的 `Rebuilding`）→ `Failed`，原因必须落到 `/api/status`。
    pub fn load_failed(&mut self, reason: impl Into<String>) -> Result<(), TransitionError> {
        self.require(
            &[StateKind::Loading, StateKind::Rebuilding],
            StateKind::Failed,
        )?;
        self.state = EngineState::Failed {
            reason: reason.into(),
        };
        Ok(())
    }

    /// 重载时模型**仍**不齐备：刷新缺失清单，状态保持 `BlockedModelsMissing`。
    ///
    /// **生产者（M2）**：`POST /api/engine/reload` 与 `POST /api/ocr` 的 409 路径
    /// （见 `server.rs::ensure_engine_loaded` / `admit_with_models_on_disk`）。
    pub fn models_still_missing(&mut self, missing: Vec<String>) -> Result<(), TransitionError> {
        self.require(
            &[StateKind::BlockedModelsMissing],
            StateKind::BlockedModelsMissing,
        )?;
        self.state = EngineState::BlockedModelsMissing { missing };
        Ok(())
    }

    /// M3：运行期切换 provider，`Ready → Rebuilding`（此时暂停新任务并排空队列）。
    ///
    /// 见 [`EngineState::Rebuilding`] 的说明：状态机与测试已冻结，M3 才会接上调用点
    /// （M2 的显式 reload 不改变 provider，因此不经过这条边）。
    #[allow(dead_code)]
    pub fn begin_rebuild(&mut self) -> Result<(), TransitionError> {
        self.require(&[StateKind::Ready], StateKind::Rebuilding)?;
        self.state = EngineState::Rebuilding;
        Ok(())
    }

    fn require(&self, allowed: &[StateKind], target: StateKind) -> Result<(), TransitionError> {
        let current = self.state.kind();
        if allowed.contains(&current) {
            return Ok(());
        }
        let allowed_from = allowed
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>()
            .join(", ");
        Err(TransitionError {
            from: current.name(),
            to: target.name(),
            allowed_from,
        })
    }
}

/// 启动期校验失败的分类（§7.6 第 2 步）。
///
/// 这里与 [`EngineState::Failed`] 是**两件不同的事**：
/// 本类型表示"进程不该启动"（配置错误），`Failed` 表示"配置合法但会话建不起来"。
/// 两者的区分正是 §7.5 要求的：provider 的可用性只有在建立会话时才能判定。
#[derive(Debug)]
pub enum StartupConfigError {
    /// 资源上限/开关取值非法（错误文本里带 CLI 开关名）。
    Limit(ServeConfigError),
    /// `--config` 指向的文件读不到或解析失败（错误里带路径）。
    ConfigFile {
        path: std::path::PathBuf,
        source: RapidOcrError,
    },
    /// 引擎配置自身非法（库内 `EngineConfig::validate` 的既有措辞）。
    Engine(RapidOcrError),
    /// provider 名称或对应 feature 非法（库内 `runtime::provider` 的既有措辞）。
    Provider(RapidOcrError),
}

impl std::fmt::Display for StartupConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit(error) => write!(f, "{error}"),
            Self::ConfigFile { path, source } => {
                write!(f, "cannot load --config {}: {source}", path.display())
            }
            Self::Engine(error) => write!(f, "engine configuration rejected: {error}"),
            Self::Provider(error) => write!(f, "provider configuration rejected: {error}"),
        }
    }
}

impl std::error::Error for StartupConfigError {}

impl From<ServeConfigError> for StartupConfigError {
    fn from(error: ServeConfigError) -> Self {
        Self::Limit(error)
    }
}

/// 启动期校验的产物：运行期上限 + 引擎配置计划。
#[derive(Debug, Clone)]
pub struct ServeStartup {
    pub limits: ServeLimits,
    pub plan: ServeConfigPlan,
}

impl ServeStartup {
    /// §7.6 第 2 步的**唯一**入口：先校验上限取值，再校验 provider 与引擎配置。
    ///
    /// 顺序是有意的：上限错误（`--max-body-mb=0`）必须在不触碰 provider 运行库的前提下
    /// 就被拒绝。
    pub fn validate(
        raw_limits: RawServeLimits,
        engine: EngineConfig,
        cli_provider: Option<ProviderPreference>,
        cli_max_side: Option<usize>,
        allow_provider_fallback: bool,
    ) -> Result<Self, StartupConfigError> {
        let limits = raw_limits.validate()?;
        let plan =
            ServeConfigPlan::validate(engine, cli_provider, cli_max_side, allow_provider_fallback)?;
        Ok(Self { limits, plan })
    }
}

/// 启动期校验后的运行配置（§7.5/§7.6 第 2 步的产物）。
///
/// 字段含义：
/// - `engine`：**已应用 CLI 覆盖**的引擎配置（§3 的优先级 CLI > YAML > 内建默认）；
/// - `requested`：本次进程请求的 provider（写进 `/api/status` 的 `requested`）；
/// - `fail_if_provider_unavailable`：serve 侧冻结的回退语义——非 cpu provider 默认**强制**
///   不回退，只有显式 `--allow-provider-fallback` 才允许回退（§7.5）。
#[derive(Debug, Clone)]
pub struct ServeConfigPlan {
    pub engine: EngineConfig,
    pub requested: ProviderPreference,
    pub fail_if_provider_unavailable: bool,
}

impl ServeConfigPlan {
    /// §7.6 第 2 步：应用 CLI 覆盖并校验运行配置，非法立即失败（调用方负责退出）。
    ///
    /// - `cli_provider`：CLI 的 `--provider`（`None` = 未给出 → 保留 YAML 值，§3 的优先级规则）；
    /// - `cli_max_side`：CLI 的 `--max-side`（`None` = 未给出）；
    /// - `allow_provider_fallback`：`--allow-provider-fallback`。
    ///
    /// **provider 可用性不在这里判定**（§7.5）：这里只判定"配置合法"，
    /// 即 provider 名称合法、且对应 feature 已编译进本次构建。运行库是否真的可用
    /// 只能在建立会话时得知，因此属于 `Loading → Ready|Failed`。
    /// 探测固定用 `fail_if_provider_unavailable = false`，这样"运行库不可用"
    /// 不会被提前升级成配置错误。
    pub fn validate(
        mut engine: EngineConfig,
        cli_provider: Option<ProviderPreference>,
        cli_max_side: Option<usize>,
        allow_provider_fallback: bool,
    ) -> Result<Self, StartupConfigError> {
        // CLI > YAML > 内建默认：只有 CLI 真的给了值才覆盖（`0` 由下面的
        // `EngineConfig::validate()` 用库内既有措辞拒绝）。
        if let Some(max_side) = cli_max_side {
            engine.global.max_side_len = max_side;
        }
        if let Some(provider) = cli_provider {
            engine.runtime.provider_preference = provider;
        }
        engine.validate().map_err(StartupConfigError::Engine)?;

        let requested = engine.runtime.provider_preference;
        // serve 侧冻结的回退语义（§7.5）：cpu 无所谓；加速 provider 默认强制不回退。
        let fail_if_provider_unavailable =
            requested != ProviderPreference::Cpu && !allow_provider_fallback;
        engine.runtime.fail_if_provider_unavailable = fail_if_provider_unavailable;

        // 只用库里的**同一套**实现做配置探测，因此"feature 未编译进来"这句话
        // 与 `run`/`evaluate` 完全一致。
        match rapid_ocr_rs::resolve_execution_providers(
            &requested,
            engine.runtime.enable_cpu_mem_arena,
            false,
        ) {
            Ok(_) => {}
            Err(RapidOcrError::UnsupportedProvider(message)) => {
                // 名字/feature 级配置错误：立即退出（§7.5 第 3 条）。
                return Err(StartupConfigError::Provider(
                    RapidOcrError::UnsupportedProvider(message),
                ));
            }
            Err(_runtime_fact) => {
                // 运行库不可用等运行期事实：留给 `Loading -> Failed`（§7.5 第 4 条），
                // 启动期不升级为配置错误，也不在这里吞掉——`Failed` 的原因会进 /api/status。
            }
        }

        Ok(Self {
            engine,
            requested,
            fail_if_provider_unavailable,
        })
    }

    /// `requested` 的展示文本（`/api/status` 的 `requested` 字段）。
    pub fn requested_label(&self) -> String {
        rapid_ocr_rs::format_provider_preference(self.requested)
    }
}

#[cfg(test)]
mod tests {
    use rapid_ocr_rs::{EngineConfig, ProviderPreference, RapidOcrError};

    use super::{
        EngineState, EngineStateMachine, ModelReadiness, OcrAdmission, ProviderStatus,
        ServeConfigPlan, ServeStartup, ServiceState, StartupConfigError, TransitionError,
    };
    use crate::serve::error::ServeError;
    use crate::serve::limits::RawServeLimits;

    fn blocked() -> EngineStateMachine {
        EngineStateMachine::start(ModelReadiness::Incomplete {
            missing: vec!["PP-OCRv6_det_medium.onnx".to_string()],
        })
    }

    fn ready() -> EngineStateMachine {
        let mut machine = EngineStateMachine::start(ModelReadiness::Complete);
        machine
            .load_succeeded("cpu", "Cpu", false)
            .expect("Loading -> Ready is legal");
        machine
    }

    #[test]
    fn service_state_starts_then_becomes_ready_once() {
        let starting = ServiceState::Starting;
        let ready = starting.listening_succeeded().expect("Starting -> Ready");
        assert_eq!(ready, ServiceState::Ready);
        assert!(
            ready.listening_succeeded().is_err(),
            "Ready -> Ready must be illegal"
        );
    }

    #[test]
    fn complete_models_start_in_loading_and_incomplete_models_start_blocked() {
        assert_eq!(
            EngineStateMachine::start(ModelReadiness::Complete).state(),
            &EngineState::Loading
        );
        assert_eq!(
            blocked().state(),
            &EngineState::BlockedModelsMissing {
                missing: vec!["PP-OCRv6_det_medium.onnx".to_string()]
            }
        );
    }

    #[test]
    fn loading_transitions_to_ready_with_the_three_provider_fields() {
        let mut machine = EngineStateMachine::start(ModelReadiness::Complete);
        machine
            .load_succeeded("directml(device_id=0)", "DirectMl", false)
            .expect("Loading -> Ready");
        assert_eq!(
            machine.state(),
            &EngineState::Ready {
                requested: "directml(device_id=0)".to_string(),
                selected_ep: "DirectMl".to_string(),
                fallback_to_cpu: false,
            }
        );
    }

    #[test]
    fn loading_transitions_to_failed_and_records_the_reason() {
        let mut machine = EngineStateMachine::start(ModelReadiness::Complete);
        machine
            .load_failed("DirectML is unavailable in the loaded ONNX Runtime")
            .expect("Loading -> Failed");
        match machine.state() {
            EngineState::Failed { reason } => {
                assert!(reason.contains("unavailable"), "reason: {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn failed_can_only_be_left_through_loading() {
        let mut machine = EngineStateMachine::start(ModelReadiness::Complete);
        machine.load_failed("boom").expect("Loading -> Failed");
        let error = machine
            .load_succeeded("cpu", "Cpu", false)
            .expect_err("Failed -> Ready must be rejected");
        assert_eq!(error.from, "failed");
        assert_eq!(error.to, "ready");
        assert!(error.allowed_from.contains("loading"), "error: {error}");
        assert_eq!(machine.state().name(), "failed");

        machine.begin_loading().expect("Failed -> Loading");
        assert_eq!(machine.state().name(), "loading");
        machine
            .load_succeeded("cpu", "Cpu", false)
            .expect("Loading -> Ready");
    }

    #[test]
    fn ready_reload_goes_ready_loading_ready() {
        let mut machine = ready();
        machine.begin_loading().expect("Ready -> Loading (reload)");
        assert_eq!(machine.state().name(), "loading");
        machine
            .load_succeeded("cpu", "Cpu", true)
            .expect("Loading -> Ready");
        match machine.state() {
            EngineState::Ready {
                fallback_to_cpu, ..
            } => assert!(*fallback_to_cpu),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn ready_reload_can_end_in_failed() {
        let mut machine = ready();
        machine.begin_loading().expect("Ready -> Loading");
        machine
            .load_failed("session creation failed")
            .expect("Loading -> Failed");
        assert_eq!(machine.state().name(), "failed");
    }

    #[test]
    fn blocked_models_missing_can_be_refreshed_or_enter_loading() {
        let mut machine = blocked();
        machine
            .models_still_missing(vec!["a.onnx".to_string(), "b.onnx".to_string()])
            .expect("BlockedModelsMissing -> BlockedModelsMissing");
        assert_eq!(
            machine.state(),
            &EngineState::BlockedModelsMissing {
                missing: vec!["a.onnx".to_string(), "b.onnx".to_string()]
            }
        );
        machine.begin_loading().expect("models are complete now");
        assert_eq!(machine.state().name(), "loading");

        let error = machine
            .models_still_missing(vec!["c.onnx".to_string()])
            .expect_err("Loading -> BlockedModelsMissing must be rejected");
        assert_eq!(error.from, "loading");
        assert_eq!(error.to, "blocked_models_missing");
    }

    #[test]
    fn rebuilding_is_reachable_from_ready_and_back_to_ready_or_failed() {
        let mut machine = ready();
        machine.begin_rebuild().expect("Ready -> Rebuilding");
        assert_eq!(
            machine.state(),
            &EngineState::Rebuilding,
            "M3 runtime provider switch"
        );

        let mut restored = machine.clone();
        restored
            .load_succeeded("cpu", "Cpu", false)
            .expect("Rebuilding -> Ready");
        assert_eq!(restored.state().name(), "ready");

        machine
            .load_failed("the new provider cannot be created")
            .expect("Rebuilding -> Failed");
        assert_eq!(machine.state().name(), "failed");
    }

    #[test]
    fn rebuilding_cannot_be_entered_from_loading_or_blocked() {
        let mut loading = EngineStateMachine::start(ModelReadiness::Complete);
        let error = loading.begin_rebuild().expect_err("Loading -> Rebuilding");
        assert_eq!(error.from, "loading");
        assert_eq!(error.to, "rebuilding");
        assert!(error.allowed_from.contains("ready"), "error: {error}");

        let mut blocked = blocked();
        let error = blocked
            .begin_rebuild()
            .expect_err("BlockedModelsMissing -> Rebuilding");
        assert_eq!(error.from, "blocked_models_missing");

        let mut rebuilding = ready();
        rebuilding.begin_rebuild().expect("Ready -> Rebuilding");
        assert!(
            rebuilding.begin_rebuild().is_err(),
            "Rebuilding -> Rebuilding must be illegal"
        );
    }

    #[test]
    fn illegal_transitions_report_from_and_to_and_legal_predecessors() {
        let mut blocked = blocked();
        let error: TransitionError = blocked
            .load_succeeded("cpu", "Cpu", false)
            .expect_err("BlockedModelsMissing -> Ready must be rejected");
        assert_eq!(error.from, "blocked_models_missing");
        assert_eq!(error.to, "ready");
        assert!(error.allowed_from.contains("loading"), "error: {error}");
        assert!(error.allowed_from.contains("rebuilding"), "error: {error}");
        let text = error.to_string();
        assert!(text.contains("blocked_models_missing -> ready"), "{text}");
    }

    #[test]
    fn ocr_admission_queues_while_loading_and_rebuilding_and_never_fails() {
        let mut machine = EngineStateMachine::start(ModelReadiness::Complete);
        match machine.state().ocr_admission() {
            OcrAdmission::Queue { waiting_for } => assert_eq!(waiting_for, "loading"),
            other => panic!("Loading must queue new OCR work, got {other:?}"),
        }
        assert!(ServeError::from_ocr_admission(machine.state().ocr_admission()).is_ok());

        machine
            .load_succeeded("cpu", "Cpu", false)
            .expect("-> Ready");
        machine.begin_rebuild().expect("Ready -> Rebuilding");
        match machine.state().ocr_admission() {
            OcrAdmission::Queue { waiting_for } => assert_eq!(waiting_for, "rebuilding"),
            other => panic!("Rebuilding must queue new OCR work, got {other:?}"),
        }
        assert!(ServeError::from_ocr_admission(machine.state().ocr_admission()).is_ok());
    }

    #[test]
    fn ocr_admission_reports_missing_models_and_engine_failure() {
        let blocked = blocked();
        match blocked.state().ocr_admission() {
            OcrAdmission::ModelsMissing { missing } => {
                assert_eq!(missing, vec!["PP-OCRv6_det_medium.onnx".to_string()]);
            }
            other => panic!("expected ModelsMissing, got {other:?}"),
        }
        let error = ServeError::from_ocr_admission(blocked.state().ocr_admission())
            .expect_err("models missing must be an error");
        assert_eq!(error.status_code(), 409);
        assert_eq!(error.code(), "models_missing");

        let mut failed = EngineStateMachine::start(ModelReadiness::Complete);
        failed.load_failed("no EP available").expect("-> Failed");
        let error = ServeError::from_ocr_admission(failed.state().ocr_admission())
            .expect_err("failed engine must be an error");
        assert_eq!(error.status_code(), 503);
        assert_eq!(error.code(), "engine_unavailable");
        assert_eq!(error.detail()["reason"], "no EP available");
    }

    #[test]
    fn ready_admission_is_run() {
        let machine = ready();
        assert_eq!(machine.state().ocr_admission(), OcrAdmission::Run);
        assert!(ServeError::from_ocr_admission(machine.state().ocr_admission()).is_ok());
    }

    /// §7.5 / §9：三个 provider 字段始终存在；未知态是 `null`，**不是** `false`。
    #[test]
    fn provider_status_never_disguises_unknown_as_false() {
        let loading = EngineState::Loading;
        let status = loading.provider_status("directml(device_id=0)");
        assert_eq!(
            status,
            ProviderStatus {
                requested: "directml(device_id=0)".to_string(),
                selected_ep: None,
                fallback_to_cpu: None,
            }
        );
        let json = serde_json::to_value(&status).expect("ProviderStatus must serialize");
        assert_eq!(json["requested"], "directml(device_id=0)");
        assert!(json["selected_ep"].is_null(), "json: {json}");
        assert!(
            json["fallback_to_cpu"].is_null(),
            "unknown must be null, not false: {json}"
        );

        let ready = EngineState::Ready {
            requested: "directml(device_id=0)".to_string(),
            selected_ep: "Cpu".to_string(),
            fallback_to_cpu: true,
        };
        let json = serde_json::to_value(ready.provider_status("ignored")).expect("serializes");
        assert_eq!(json["requested"], "directml(device_id=0)");
        assert_eq!(json["selected_ep"], "Cpu");
        assert_eq!(json["fallback_to_cpu"], true);
    }

    /// `/api/status` 的引擎字段必须可 JSON 化，且带 `state` 标记。
    #[test]
    fn engine_state_is_serializable_for_api_status() {
        let cases: [(EngineState, &str); 5] = [
            (
                EngineState::BlockedModelsMissing {
                    missing: vec!["a.onnx".to_string()],
                },
                "blocked_models_missing",
            ),
            (EngineState::Loading, "loading"),
            (
                EngineState::Ready {
                    requested: "cpu".to_string(),
                    selected_ep: "Cpu".to_string(),
                    fallback_to_cpu: false,
                },
                "ready",
            ),
            (
                EngineState::Failed {
                    reason: "boom".to_string(),
                },
                "failed",
            ),
            (EngineState::Rebuilding, "rebuilding"),
        ];
        for (state, expected) in cases {
            let json = serde_json::to_value(&state).expect("EngineState must serialize");
            assert_eq!(json["state"], expected, "json: {json}");
        }
        let json = serde_json::to_value(EngineState::BlockedModelsMissing {
            missing: vec!["a.onnx".to_string()],
        })
        .expect("serializes");
        assert_eq!(json["missing"][0], "a.onnx");
    }

    #[test]
    fn startup_rejects_invalid_limits_before_touching_the_provider() {
        let error = ServeStartup::validate(
            RawServeLimits {
                max_body_mb: 0,
                ..Default::default()
            },
            EngineConfig::default(),
            Some(ProviderPreference::Cuda { device_id: 0 }),
            None,
            false,
        )
        .expect_err("a zero body limit must be fatal");
        match error {
            StartupConfigError::Limit(inner) => {
                assert_eq!(inner.field(), "--max-body-mb");
            }
            other => panic!("expected a limit error, got {other}"),
        }
    }

    /// §7.5 第 3 条：请求一个本次构建没有编译进来的 provider 必须在启动期失败，
    /// 且错误文本来自库里的 `runtime::provider`（"not compiled in"）。
    #[test]
    #[cfg(not(feature = "directml-provider"))]
    fn startup_rejects_a_provider_whose_feature_is_not_compiled_in() {
        let error = ServeStartup::validate(
            RawServeLimits::default(),
            EngineConfig::default(),
            Some(ProviderPreference::DirectMl { device_id: 0 }),
            None,
            false,
        )
        .expect_err("directml without the feature must be fatal");
        let text = error.to_string();
        assert!(text.contains("not compiled in"), "{text}");
        assert!(text.contains("directml-provider"), "{text}");
        match error {
            StartupConfigError::Provider(RapidOcrError::UnsupportedProvider(_)) => {}
            other => panic!("expected a provider config error, got {other}"),
        }
    }

    /// §7.5：`--provider directml` 默认强制不回退；显式 `--allow-provider-fallback` 才允许。
    /// CPU 不受该规则影响。
    #[test]
    fn provider_fallback_policy_is_frozen_by_default() {
        let cpu = ServeConfigPlan::validate(EngineConfig::default(), None, None, false)
            .expect("the default config must be valid");
        assert_eq!(cpu.requested, ProviderPreference::Cpu);
        assert_eq!(cpu.requested_label(), "cpu");
        assert!(
            !cpu.fail_if_provider_unavailable,
            "cpu must not carry the accelerator fallback policy"
        );

        let plan = ServeConfigPlan::validate(
            EngineConfig::default(),
            Some(ProviderPreference::Cpu),
            None,
            true,
        )
        .expect("cpu is always resolvable");
        assert!(!plan.fail_if_provider_unavailable);
    }

    /// §3：优先级 CLI > `--config` YAML > 内建默认（这里用"给/不给 CLI 值"两种情形证明）。
    #[test]
    fn cli_overrides_beat_yaml_and_yaml_beats_the_builtin_default() {
        let mut yaml = EngineConfig::default();
        yaml.global.max_side_len = 1234;
        yaml.runtime.provider_preference = ProviderPreference::Cpu;

        // 不给 CLI 值：YAML 生效。
        let plan = ServeConfigPlan::validate(yaml.clone(), None, None, false)
            .expect("the YAML config must be valid");
        assert_eq!(plan.engine.global.max_side_len, 1234);

        // 给了 CLI 值：CLI 覆盖 YAML。
        let plan =
            ServeConfigPlan::validate(yaml, Some(ProviderPreference::Cpu), Some(2000), false)
                .expect("the overridden config must be valid");
        assert_eq!(plan.engine.global.max_side_len, 2000);

        // 两者都不给：库的内建默认（2000）。
        let plan = ServeConfigPlan::validate(EngineConfig::default(), None, None, false)
            .expect("defaults must be valid");
        assert_eq!(plan.engine.global.max_side_len, 2000);
    }

    /// `--max-side=0` 由库内既有的 `EngineConfig::validate` 拒绝（带字段名），不另写措辞。
    #[test]
    fn cli_max_side_zero_is_rejected_by_the_library_validation() {
        let error = ServeConfigPlan::validate(EngineConfig::default(), None, Some(0), false)
            .expect_err("--max-side=0 must be fatal");
        let text = error.to_string();
        assert!(text.contains("max_side_len"), "{text}");
        assert!(text.contains("greater than zero"), "{text}");
    }
}
