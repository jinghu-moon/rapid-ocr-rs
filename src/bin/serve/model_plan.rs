//! 模型集的解析（§5.3 的单一来源规则）、**这次运行的模型计划**、就绪判定（§7.6 第 3 步）
//! 与引擎路径绑定。
//!
//! # 为什么 `--model-dir` 是唯一权威（本模块的核心决策）
//!
//! 引擎既能按 `--config` 里显式的 `model_path` 加载文件，也能按 `model_store_dir` 从
//! 默认表下载；若两条路径都留着，`/api/models`（按目录 + 来源表算出来的状态）与引擎
//! **真正加载的文件**就会是两个来源——这正是 §5.3 要根除的"双权威"。
//!
//! 因此 serve 的规则是：
//!
//! 1. 模型集只由 `--model-dir` 与**单一来源**（`<model-dir>/manifest.json` 存在时只用它，
//!    否则只用 `assets/default_models.yaml`）决定；
//! 2. 引擎的 `det.model_path` / `cls.model_path` / `rec.model.model_path` /
//!    `rec.model.rec_keys_path` **一律由该模型集里对应 role 的文件名拼出**，
//!    配置里的同名项被覆盖（§3 的 `CLI flag > --config YAML > 内建默认` 在模型目录上
//!    同样成立：`--model-dir` 是 CLI 侧的值）；
//! 3. 三个 `allow_download` 一律置 `false`：读请求不得触发 600+ MB 的网络与磁盘副作用
//!    （§0.2），下载是显式的 M2 动作。
//!
//! 这样"引擎要加载的文件"与"`/api/models` 报告状态的文件"在结构上就是同一份清单。
//!
//! # 一份运行计划（**唯一**来源）
//!
//! [`ModelPlan::resolve`] 在启动期算出**这次运行真的会加载哪些文件**（[`PlanFile`]），
//! 并且这是唯一的判据：A1（`--reverify-models`）、A2（`POST /api/models/reverify`）、
//! `/api/models` 的 `pipelines` 块、`/api/ocr` 两条管线的准入，以及
//! `pin_engine_paths` / `FormulaPolicy` 这两条加载路径，全部读同一份清单。
//!
//! 计划 = 文本管线（detector + recognizer + dictionary，`global.use_cls` 时再加 classifier）
//! **加上**（**仅当公式路由启用时**）公式管线的 formula_recognizer 与 formula_detector。
//! "公式路由启用"只有一个判据：解析出了一个页面公式检测模型——`--formula-detector`，
//! 或模型集里声明的 `formula_detector` role（CLI 优先）。公式未启用时公式模型
//! **不在计划里**，因此 A1/A2 一个字节都不读它们（566 MB 的识别模型不再被无条件哈希）。
//!
//! 模型**库存**（`/api/models` 的 `sets[]` 与 `formula` 块）仍然是"单一来源描述的一整
//! 个目录"：页面要能显示公式集合的体积、并按集合显式下载（§5.3、§5.4）。库存与计划的
//! 区别是"目录里有什么"与"这次运行会加载什么"——两者都由本模块给出，且**只有这里**给。
//!
//! # 没有声明摘要时"验证"的含义（文档化规则）
//!
//! `--formula-detector` 指向的文件可能不属于任何模型集，于是没有可信摘要。此时
//! "verified" 只意味着三件事：**存在**、**读得出来**、**看起来是一份 ONNX**
//! （protobuf 序言：字段 1 = `ir_version`，varint 取值 1..=64，见
//! [`undeclared_digest_failure`]）。它是启发式，**不是**完整性证明：响应里该文件的
//! `sha256` 是 `null`，而 A1/A2 仍然会为它**真的算一个摘要**并如实报告算出来的值。
//! 集合声明了摘要时，判定完全按库的逐文件校验（哈希不匹配 = `corrupt`）。
//! 两条规则都只在 [`PlanFile::state`] 这一处实现。
//!
//! # 两条管线的结论仍然分开
//!
//! 计划按 role 分组（[`Pipeline::of`]）：文本组的结论驱动引擎状态机与 `/api/ocr` 的 409，
//! 公式组的结论驱动公式队列的 409（`detail.scope = "formula"`）。公式模型缺失/损坏
//! **绝不**让普通 OCR 变成 409，反之亦然。

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rapid_ocr_rs::{
    DefaultModelSelection, EngineConfig, ModelFileSpec, ModelFileState, ModelRequest, ModelRole,
    ModelSet, ModelSetStatus, ModelSource, ModelSourceKind, RapidOcrError,
};

use super::state::ModelReadiness;

/// 一条管线（§8.1 的双队列各自需要哪些 role）。
///
/// 判定**只按 role**，不按"集合 id"或集合在响应里的顺序：本地清单只产生一个集合、
/// 默认表产生两个，同一条规则（见 [`Pipeline::of`]）在两种来源下都必须成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pipeline {
    Text,
    Formula,
}

impl Pipeline {
    /// 两条管线的稳定顺序（错误文案、分组报告与日志都用它）。
    pub const ALL: [Self; 2] = [Self::Text, Self::Formula];

    /// 某个 role 属于哪条管线（**唯一**实现，与库的
    /// `ModelRequest::{text_roles, formula_roles}` 是同一个划分）。
    pub fn of(role: ModelRole) -> Self {
        match role {
            ModelRole::FormulaDetector | ModelRole::FormulaRecognizer => Self::Formula,
            ModelRole::Detector
            | ModelRole::Classifier
            | ModelRole::Recognizer
            | ModelRole::Dictionary
            | ModelRole::Tokenizer => Self::Text,
        }
    }

    /// 稳定的小写名称（`/api/models/reverify` 的 `files[].pipeline` 与日志用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Formula => "formula",
        }
    }
}

/// 一个"阻塞就绪"的模型文件：缺失或损坏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlockingFile {
    pub name: String,
    /// 它在哪条管线里（分组依据；见 [`Pipeline`]）。
    pub pipeline: Pipeline,
    /// `true` = 存在但哈希/内容不对（§5.2 的 `Corrupt`），文案与退出行为都不同。
    pub corrupt: bool,
}

/// 计划里一个文件在某一刻的状态（**唯一**的解析结果，供分组报告与准入共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanFileStatus {
    pub name: String,
    pub role: ModelRole,
    pub pipeline: Pipeline,
    pub state: ModelFileState,
}

/// 运行计划里的一个文件：**这次运行真的会加载它**。
///
/// 它是"哪些文件属于这次运行"的唯一表示：A1、A2、`/api/models` 的 `pipelines` 块、
/// 两条管线的准入与两条加载路径都读这份清单。`--formula-detector` 指向的文件可以
/// 在模型目录之外（因此它带着自己的绝对路径，而不是一个集合里的文件名）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanFile {
    /// 文件名（不泄露绝对路径；与 `/api/models` 的 `files[].name` 同值）。
    pub name: String,
    pub role: ModelRole,
    /// 绝对路径（检测模型可能在模型目录之外）。
    pub path: PathBuf,
    /// 模型集声明的 SHA-256；`None` = 没有可信摘要（见模块文档的文档化规则）。
    pub declared_sha256: Option<String>,
    pub pipeline: Pipeline,
}

impl PlanFile {
    /// 是否有集合声明的可信摘要（决定 [`Self::state`] 走哪条规则）。
    pub fn has_declared_digest(&self) -> bool {
        self.declared_sha256.is_some()
    }

    /// 声明摘要（没有时为空串；库的 `ModelFileSpec` 用空串表示"没有哈希"）。
    fn expected(&self) -> String {
        self.declared_sha256.clone().unwrap_or_default()
    }

    /// 用库的类型重建这个文件的描述：文件名校验与"声明摘要 vs 实际摘要"的判定规则
    /// **只能有一份实现**，因此这里不自己写比较。
    fn spec(&self) -> Result<ModelFileSpec, RapidOcrError> {
        ModelFileSpec::new(
            self.name.clone(),
            self.role,
            None,
            self.expected(),
            String::new(),
        )
    }

    /// 文件所在目录（检测模型可以在模型目录之外，因此根由路径自身给出）。
    fn root(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// 这个文件**此刻**的状态（**唯一实现**，准入与报告都用它）。
    ///
    /// - 声明了摘要 → 库的逐文件校验（[`ModelFileSpec::state_in`]，背后是身份键控的
    ///   哈希缓存）；
    /// - 没有声明摘要 → 只能证明"在、读得出来、像 ONNX"（[`undeclared_digest_failure`]），
    ///   绝不假装校验过内容：响应里的 `sha256` 因此是 `null`。
    pub fn state(&self) -> ModelFileState {
        let expected = self.expected();
        let spec = match self.spec() {
            Ok(spec) => spec,
            Err(error) => {
                return ModelFileState::Corrupt {
                    expected,
                    actual: format!("the planned file cannot be described: {error}"),
                };
            }
        };
        if self.has_declared_digest() {
            return spec.state_in(self.root());
        }
        match spec.state_in(self.root()) {
            ModelFileState::Missing => ModelFileState::Missing,
            _ => match undeclared_digest_failure(&self.path) {
                None => ModelFileState::Present,
                Some(actual) => ModelFileState::Corrupt { expected, actual },
            },
        }
    }

    /// 逐文件结论（报告与 `/api/models` 的 `pipelines.files[]` 共用）。
    pub fn status(&self) -> PlanFileStatus {
        PlanFileStatus {
            name: self.name.clone(),
            role: self.role,
            pipeline: self.pipeline,
            state: self.state(),
        }
    }
}

/// 启动期解析的模型清单与**运行计划**（本次进程唯一的模型事实）。
#[derive(Debug, Clone)]
pub(super) struct ModelPlan {
    model_dir: PathBuf,
    source: ModelSourceKind,
    /// 单一来源描述的**库存**（`/api/models` 的 `sets[]` 与 `formula` 块用它）。
    sets: Vec<ModelSet>,
    /// **运行计划**：这次运行真的会加载的文件（A1/A2/准入/加载路径用它）。
    planned: Vec<PlanFile>,
    /// 解析出的公式检测模型；`None` = 公式路由不可用（公式模型因此不在计划里）。
    detector: Option<FormulaDetectorSpec>,
}

impl ModelPlan {
    /// §5.3 的单一来源选择 + 与 `--config` 一致的管线选择 + 这次运行的模型计划。
    ///
    /// 第三个参数是 `--formula-detector`（CLI 侧的值）：它是否在场**唯一**决定公式管线
    /// 是否属于这次运行，因此计划必须在这里一次算完，而不是由调用方事后拼装。
    pub fn resolve(
        model_dir: &Path,
        engine: &EngineConfig,
        formula_detector: Option<&Path>,
    ) -> Result<Self, ModelPlanError> {
        let source = ModelSource::select(model_dir).map_err(ModelPlanError::Source)?;
        if matches!(source.kind(), ModelSourceKind::DefaultTable) {
            // 默认表按"一个 model_type 覆盖整条管线"选择（`DefaultModelSelection` 只有一个
            // `model_type` 字段）。det 与 rec 给出不同的 model_type 时，默认表无法同时满足
            // 两者；静默用其中一个会让 `/api/models` 报告的文件与引擎加载的文件不一致。
            if engine.det.model_type != engine.rec.model.model_type {
                return Err(ModelPlanError::ModelTypeConflict {
                    det: engine.det.model_type.as_str(),
                    rec: engine.rec.model.model_type.as_str(),
                });
            }
        }

        // 库存仍然请求两条管线：页面必须能报告公式集合的体积/哈希并按集合显式下载
        // （§5.3、§5.4）。缺少 role 时由库的 `ModelSet::require_roles` 报"缺哪些 role"，
        // 不静默降级。**运行计划是另一件事**——它只含这次真的会加载的文件。
        let request = ModelRequest::text_and_formula(selection(engine));
        let sets = source
            .model_sets(&request)
            .map_err(ModelPlanError::Source)?;
        if sets.is_empty() {
            return Err(ModelPlanError::NoSets);
        }
        let mut plan = Self {
            model_dir: model_dir.to_path_buf(),
            source: source.kind(),
            sets,
            planned: Vec::new(),
            detector: None,
        };

        // 结构性检查（与"这次跑不跑公式"无关）：**被集合声明过的每个 role** 都必须唯一
        // 映射到一个文件。歧义是清单自身的结构错误，不是运行范围问题——静默挑一个会让
        // `/api/models` 报告的文件与引擎加载的文件分叉（§5.3）。公式识别模型声明两次时
        // 即使公式路由关闭也必须在这里被拒绝。
        for role in ModelRole::ALL {
            plan.spec_for(role)?;
        }

        // 公式检测模型：CLI（`--formula-detector`）优先，其次是集合声明的 role。
        // 解析结果 = "路径 + 集合声明的 SHA-256"，且它是否在场唯一决定公式路由。
        plan.detector = plan.resolve_formula_detector(formula_detector)?;
        plan.planned = plan.build_plan(engine.global.use_cls)?;
        Ok(plan)
    }

    pub fn source(&self) -> ModelSourceKind {
        self.source
    }

    /// 模型目录（下载落盘位置、`/api/models` 的 `model_dir` 来源；响应里一律脱敏）。
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// 按 `set_id` 取集合（§4.2 的 `POST /api/models/download` 请求体里**只有**这个 id）。
    ///
    /// 找不到就是找不到：调用方必须报可定位错误，**绝不**回落到 `sets[0]`
    /// （页面每个下载按钮携带自己的 id，"默认下第一个集合"是它明确删掉的语义歧义）。
    pub fn set_by_id(&self, set_id: &str) -> Option<&ModelSet> {
        self.sets.iter().find(|set| set.id == set_id)
    }

    /// 全部集合 id（未知 `set_id` 的错误里给出它，便于定位）。
    pub fn set_ids(&self) -> Vec<String> {
        self.sets.iter().map(|set| set.id.clone()).collect()
    }

    /// **运行计划**（这次运行真的会加载的文件，按模型集声明顺序）。
    pub fn plan_files(&self) -> &[PlanFile] {
        &self.planned
    }

    /// 计划里某条管线的文件（按计划顺序）。
    pub fn plan_files_in(&self, pipeline: Pipeline) -> Vec<&PlanFile> {
        self.plan_files()
            .iter()
            .filter(|file| file.pipeline == pipeline)
            .collect()
    }

    /// 计划里某个 role 的那个文件；`None` = 该 role 不属于这次运行。
    ///
    /// 每个 role 最多出现一次：计划由 [`Self::build_plan`] 在一个地方构造，
    /// 重复声明在解析期就是 [`ModelPlanError::AmbiguousRole`]。
    pub fn plan_file(&self, role: ModelRole) -> Option<&PlanFile> {
        self.planned.iter().find(|file| file.role == role)
    }

    /// 计划里某条管线的阻塞文件（缺失 ∪ 损坏），按计划顺序。
    ///
    /// 判定用 [`PlanFile::state`]（唯一实现），因此"报告的"与"准入拒绝的"不可能分叉。
    pub fn plan_blocking(&self, pipeline: Pipeline) -> Vec<BlockingFile> {
        self.plan_files_in(pipeline)
            .into_iter()
            .filter_map(|file| match file.state() {
                ModelFileState::Present => None,
                ModelFileState::Missing => Some(BlockingFile {
                    name: file.name.clone(),
                    pipeline,
                    corrupt: false,
                }),
                ModelFileState::Corrupt { .. } => Some(BlockingFile {
                    name: file.name.clone(),
                    pipeline,
                    corrupt: true,
                }),
            })
            .collect()
    }

    /// 某条管线是否齐备：计划里每个文件都 `Present`（没有声明摘要的文件在
    /// [`PlanFile::state`] 里已按文档化规则判定）。**不是**"库存里所有集合都齐备"。
    ///
    /// 公式管线不在计划里时它是**空真**（`true`）：那时计划里没有任何公式文件，
    /// 因此也没有任何东西能阻塞公式管线——"在计划里"是另一个字段（[`Self::formula_in_plan`]），
    /// 调用方必须同时看它（`/api/models` 的 `pipelines.formula` 就是这么做的）。
    pub fn plan_complete(&self, pipeline: Pipeline) -> bool {
        self.plan_blocking(pipeline).is_empty()
    }

    /// 公式管线是否属于这次运行（= 解析出了检测模型）。
    pub fn formula_in_plan(&self) -> bool {
        self.detector.is_some()
    }

    /// 配置好的公式检测模型（路径 + 集合声明的 SHA-256）；`None` = 公式路由不可用。
    pub fn formula_detector(&self) -> Option<&FormulaDetectorSpec> {
        self.detector.as_ref()
    }

    /// 每个集合的逐文件状态（§5.2 的唯一实现是库里的 `validate_model_files`）。
    ///
    /// 每次调用都会重新读盘：状态永远报告磁盘上的**当前**事实，而不是启动时的快照
    /// （M2 下载完成后页面必须看到 `present`）。哈希走库的**身份键控校验缓存**
    /// （`model_verify`：键 = 路径 + 体积 + mtime + 首尾各 64 KiB 的局部摘要），因此
    /// 同一个身份只算一次——首次真的读盘（566 MB 公式模型约 0.3 s，实测值记在
    /// `/api/models` 的 `verification` 块里），命中只花一次 `stat`。请求路径与
    /// `/api/models` **共用这一份证据**，因此"报告的"与"准入判定的"不可能分叉。
    pub fn report(&self) -> ModelReport {
        let mut cold_this_call = 0_usize;
        let statuses = self
            .sets
            .iter()
            .map(|set| {
                let (status, computed) = set.status_probed(&self.model_dir);
                cold_this_call += computed;
                status
            })
            .collect();
        ModelReport {
            statuses,
            cold_this_call,
        }
    }

    /// 启动期快照：引擎状态机的输入（§7.6 第 3 步）。**按运行计划分组**。
    pub fn snapshot(&self) -> ModelSnapshot {
        ModelSnapshot {
            model_dir: self.model_dir.clone(),
            source: self.source,
            blocking: self.plan_blocking(Pipeline::Text),
            formula_blocking: self.plan_blocking(Pipeline::Formula),
            formula_in_plan: self.formula_in_plan(),
        }
    }

    /// 把引擎配置的三个模型路径钉在**运行计划**的文件上（见模块文档）。
    pub fn pin_engine_paths(&self, engine: &mut EngineConfig) -> Result<(), ModelPlanError> {
        let det = self.planned_path(ModelRole::Detector)?;
        engine.det.model_path = Some(det);
        engine.det.model_store_dir = Some(self.model_dir.clone());
        engine.det.allow_download = false;

        let rec = self.planned_path(ModelRole::Recognizer)?;
        engine.rec.model.model_path = Some(rec);
        engine.rec.model_store_dir = Some(self.model_dir.clone());
        engine.rec.model.allow_download = false;

        let dict = self.planned_path(ModelRole::Dictionary)?;
        engine.rec.model.rec_keys_path = Some(dict);

        if engine.global.use_cls {
            let cls = self.planned_path(ModelRole::Classifier)?;
            engine.cls.model_path = Some(cls);
            engine.cls.model_store_dir = Some(self.model_dir.clone());
            engine.cls.allow_download = false;
        } else {
            // 方向分类关闭时不留任何陈旧路径：`use_cls` 打开后必须仍走模型集。
            engine.cls.model_path = None;
            engine.cls.model_store_dir = Some(self.model_dir.clone());
            engine.cls.allow_download = false;
        }
        Ok(())
    }

    /// 计划里某个 role 的绝对路径（加载路径的唯一取值点）。
    fn planned_path(&self, role: ModelRole) -> Result<PathBuf, ModelPlanError> {
        self.plan_file(role)
            .map(|file| file.path.clone())
            .ok_or(ModelPlanError::MissingRole { role })
    }

    /// 构造**运行计划**（唯一实现，[`Self::resolve`] 调用一次）。
    ///
    /// 文本管线恒在计划里；公式管线（识别 + 检测）只在解析出检测模型时加入。
    /// 被选中的检测模型如果**不是**集合声明的那个文件（CLI 覆盖到别处），集合里那份
    /// 就不会被加载，因此它**不进计划**——计划必须精确等于"这次会打开的文件"。
    fn build_plan(&self, use_cls: bool) -> Result<Vec<PlanFile>, ModelPlanError> {
        let formula = self.detector.is_some();
        let mut roles = vec![ModelRole::Detector];
        if use_cls {
            roles.push(ModelRole::Classifier);
        }
        roles.push(ModelRole::Recognizer);
        roles.push(ModelRole::Dictionary);
        if formula {
            roles.push(ModelRole::FormulaRecognizer);
            if self.detector_is_declared() {
                roles.push(ModelRole::FormulaDetector);
            }
        }

        let mut out: Vec<PlanFile> = Vec::new();
        for set in &self.sets {
            for file in &set.files {
                if !roles.contains(&file.role) {
                    continue;
                }
                if out.iter().any(|planned| planned.name == file.name) {
                    continue;
                }
                out.push(PlanFile {
                    name: file.name.clone(),
                    role: file.role,
                    path: self.model_dir.join(&file.name),
                    declared_sha256: file.has_hash().then(|| file.sha256.clone()),
                    pipeline: Pipeline::of(file.role),
                });
            }
        }
        if !formula {
            return Ok(out);
        }
        // 公式识别模型是公式队列的必需文件（库的 `ModelRequest::formula_roles`）；
        // 缺失在这里是可定位的启动期错误（带管线信息），而不是第一次公式请求才失败。
        if !out
            .iter()
            .any(|planned| planned.role == ModelRole::FormulaRecognizer)
        {
            return Err(ModelPlanError::MissingRole {
                role: ModelRole::FormulaRecognizer,
            });
        }
        let spec = self
            .detector
            .as_ref()
            .expect("the formula pipeline is only planned when a detector was resolved");
        if !out
            .iter()
            .any(|planned| same_file(&planned.path, &spec.path))
        {
            out.push(PlanFile {
                name: file_name(&spec.path),
                role: ModelRole::FormulaDetector,
                path: spec.path.clone(),
                declared_sha256: spec.expected_sha256.clone(),
                pipeline: Pipeline::Formula,
            });
        }
        Ok(out)
    }

    /// 被选中的检测模型是否**就是**集合声明的那个文件（决定它是否已随集合进计划）。
    fn detector_is_declared(&self) -> bool {
        let Some(spec) = self.detector.as_ref() else {
            return false;
        };
        self.spec_for(ModelRole::FormulaDetector)
            .ok()
            .flatten()
            .is_some_and(|declared| same_file(&self.model_dir.join(&declared.name), &spec.path))
    }

    /// 某个 role 在模型集里的文件描述（`None` = 该 role 没有被任何集合声明）。
    ///
    /// 重复声明（两个集合声明同一 role 的不同文件，或同一个清单里声明两次）会返回
    /// [`ModelPlanError::AmbiguousRole`]，而不是"悄悄用第一个"。这是**唯一**的 role →
    /// 文件解析实现（计划、检测模型解析与结构检查都经它）。
    fn spec_for(
        &self,
        role: ModelRole,
    ) -> Result<Option<&rapid_ocr_rs::ModelFileSpec>, ModelPlanError> {
        let mut found: Option<&rapid_ocr_rs::ModelFileSpec> = None;
        for set in &self.sets {
            for file in &set.files {
                if file.role != role {
                    continue;
                }
                match found {
                    None => found = Some(file),
                    Some(existing) if existing.name == file.name => {}
                    Some(existing) => {
                        return Err(ModelPlanError::AmbiguousRole {
                            role,
                            first: existing.name.clone(),
                            second: file.name.clone(),
                        });
                    }
                }
            }
        }
        Ok(found)
    }

    /// 解析公式检测模型：CLI（`--formula-detector`）优先，其次是模型集声明的
    /// `formula_detector` role。
    ///
    /// **哈希跟随被选中的那个文件**——完整性规则与识别模型**完全对称**：
    ///
    /// - 选中的路径正是模型集声明过的那一个文件（无论它来自 CLI 覆盖还是 role 本身），
    ///   且集合声明了 SHA-256 → 摘要随路径一起交给 `FormulaPolicy`，库在加载检测器时校验；
    /// - CLI 给了一个集合没有声明过的路径 → 没有可信摘要，`expected_sha256 = None`，
    ///   如实表示"这个文件无法校验内容"，而不是挑一个别的哈希来"看起来校验过"
    ///   （此时"验证"的含义见模块文档的文档化规则）。
    fn resolve_formula_detector(
        &self,
        cli: Option<&Path>,
    ) -> Result<Option<FormulaDetectorSpec>, ModelPlanError> {
        let declared = self.spec_for(ModelRole::FormulaDetector)?;
        let Some(cli) = cli else {
            return Ok(declared.map(|spec| FormulaDetectorSpec {
                path: self.model_dir.join(&spec.name),
                expected_sha256: spec.has_hash().then(|| spec.sha256.clone()),
            }));
        };
        let expected_sha256 = declared
            .filter(|spec| same_file(&self.model_dir.join(&spec.name), cli))
            .filter(|spec| spec.has_hash())
            .map(|spec| spec.sha256.clone());
        Ok(Some(FormulaDetectorSpec {
            path: cli.to_path_buf(),
            expected_sha256,
        }))
    }

    /// **冷验证**运行计划里的每一个文件，忽略身份键控缓存（`--reverify-models` 与
    /// `POST /api/models/reverify` 的唯一实现）。
    ///
    /// 每个文件都调用库的 [`rapid_ocr_rs::force_verify_file`]：它现在就读盘重算 SHA-256，
    /// 并如实回答"这一次算了没有、为什么算"（`cause`）。命中路径**不存在**，
    /// 因此调用方拿到的 `digests_computed` 就是"这次真的读了多少个文件"。
    ///
    /// 状态判定与 [`PlanFile::state`] 是**同一条规则**：有声明摘要就比哈希，没有就按
    /// 文档化的"存在 + 可读 + 像 ONNX"（这次已经读盘算过摘要，可读性因此已被证明，
    /// 只需再核对序言）。摘要无论有没有声明值都会被如实报告。
    pub fn reverify(&self) -> PlanReverification {
        let mut files = Vec::new();
        let mut digests_computed = 0_usize;
        let mut content_changed = Vec::new();
        for planned in &self.planned {
            let expected = planned.expected();
            // "文件不在"与"文件在但读不出来/内容不对"必须是**两种**结论（`/api/models`
            // 的 `missing` 与 `corrupt` 就是靠它们区分，前端给出的建议也不同：
            // 缺失 → 下载，损坏 → 重新下载并以原子方式替换）。因此先用一次 `is_file`
            // 把"不在"分离出来，`force_verify_file` 的错误随后只表示"在但读不出来"。
            let outcome = if planned.path.is_file() {
                Some(rapid_ocr_rs::force_verify_file(&planned.path))
            } else {
                None
            };
            let (state, digest, cause) = match outcome {
                None => (
                    ModelFileState::Missing,
                    None,
                    rapid_ocr_rs::ReverifyCause::FirstSight,
                ),
                Some(Ok(outcome)) => {
                    digests_computed += 1;
                    if outcome.cause == rapid_ocr_rs::ReverifyCause::ContentChanged {
                        content_changed.push(planned.name.clone());
                    }
                    let state = if planned.has_declared_digest() {
                        if outcome.sha256.eq_ignore_ascii_case(&expected) {
                            ModelFileState::Present
                        } else {
                            ModelFileState::Corrupt {
                                expected: expected.clone(),
                                actual: outcome.sha256.clone(),
                            }
                        }
                    } else {
                        // 没有可信摘要：这次读盘已经证明"可读"，再核对序言是否像 ONNX。
                        match undeclared_digest_failure(&planned.path) {
                            None => ModelFileState::Present,
                            Some(actual) => ModelFileState::Corrupt {
                                expected: String::new(),
                                actual,
                            },
                        }
                    };
                    (state, Some(outcome.sha256), outcome.cause)
                }
                Some(Err(error)) => (
                    ModelFileState::Corrupt {
                        expected: expected.clone(),
                        actual: format!("unreadable: {error}"),
                    },
                    None,
                    rapid_ocr_rs::ReverifyCause::FirstSight,
                ),
            };
            files.push(VerifiedPlanFile {
                name: planned.name.clone(),
                role: planned.role,
                pipeline: planned.pipeline,
                declared_sha256: planned.declared_sha256.clone(),
                sha256: digest,
                state,
                cause,
            });
        }
        PlanReverification {
            files,
            digests_computed,
            content_changed,
        }
    }
}

/// 一次"冷验证运行计划"的逐文件结论（启动期与 `POST /api/models/reverify` 共用）。
///
/// `pub(crate)` 是因为它出现在 [`crate::serve::run::ServeStartError::ModelsUnusable`] 里
/// （启动期拒绝的错误载荷）；字段仍然私有，外部只能经下面的访问器读。
#[derive(Debug, Clone)]
pub(crate) struct PlanReverification {
    /// 与 [`ModelPlan::plan_files`] **同序**（因此也按管线分组）。
    files: Vec<VerifiedPlanFile>,
    /// 这一轮真的重算了多少个完整摘要（`force_verify_file` 从不命中缓存，因此它等于
    /// 计划里的文件数；例外是读不出来的文件——它连摘要都没有）。
    digests_computed: usize,
    /// 局部摘要发现"stat 身份相同、内容不同"的文件名（局部摘要抓住的替换）。
    content_changed: Vec<String>,
}

impl PlanReverification {
    pub fn files(&self) -> &[VerifiedPlanFile] {
        &self.files
    }

    /// 这一轮算出的完整摘要个数（进响应的 `computed`，也是测试断言的对象）。
    pub fn digests_computed(&self) -> usize {
        self.digests_computed
    }

    /// 内容被换过的文件名（进响应的 `content_changed` 与启动日志）。
    pub fn content_changed(&self) -> &[String] {
        &self.content_changed
    }

    /// 缺失 ∪ 损坏的**全部**文件（不区分管线；启动期 fail-fast 的判据）。
    pub fn blocking(&self) -> Vec<&VerifiedPlanFile> {
        self.files
            .iter()
            .filter(|file| !file.state.is_present())
            .collect()
    }

    /// 某条管线里缺失 ∪ 损坏的文件（分组报告与错误文案用它）。
    pub fn blocking_in(&self, pipeline: Pipeline) -> Vec<&VerifiedPlanFile> {
        self.files
            .iter()
            .filter(|file| file.pipeline == pipeline && !file.state.is_present())
            .collect()
    }

    /// 计划里某条管线的全部文件（分组报告用）。
    pub fn files_in(&self, pipeline: Pipeline) -> Vec<&VerifiedPlanFile> {
        self.files
            .iter()
            .filter(|file| file.pipeline == pipeline)
            .collect()
    }

    /// 拒绝启动时的**分管线**清单（错误文案与日志共用，唯一实现）。
    ///
    /// 只写一行一句是不够的：调用方必须能一眼看出是**哪条管线**的**哪个文件**，
    /// 因此每条管线一行，行内是该管线里缺失/损坏文件的逐文件结论。
    pub fn blocking_summary(&self) -> String {
        let mut lines = Vec::new();
        for pipeline in Pipeline::ALL {
            let files = self.blocking_in(pipeline);
            if files.is_empty() {
                continue;
            }
            lines.push(format!(
                "{} pipeline: {}",
                pipeline.as_str(),
                files
                    .iter()
                    .map(|file| file.describe())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        lines.join("\n  ")
    }
}

/// 一个"这次运行会加载的文件"的冷验证结论。
#[derive(Debug, Clone)]
pub(crate) struct VerifiedPlanFile {
    /// 文件名（不泄露绝对路径；与 `/api/models` 的 `files[].name` 同值）。
    pub name: String,
    pub role: ModelRole,
    pub pipeline: Pipeline,
    /// 模型集声明的 SHA-256；`None` = 没有可信摘要（只能按文档化规则证明"可用"）。
    pub declared_sha256: Option<String>,
    /// 这一轮真正算出来的摘要（读不出来时为 `None`；没有声明值时**照样**报告它）。
    pub sha256: Option<String>,
    pub state: ModelFileState,
    /// 这一轮为什么算了摘要（`force_verify_file` 从不返回 `cache_hit`）。
    pub cause: rapid_ocr_rs::ReverifyCause,
}

impl VerifiedPlanFile {
    /// 启动日志/响应里的一行结论。
    pub fn describe(&self) -> String {
        let digest = match (&self.declared_sha256, &self.sha256) {
            (Some(_), Some(value)) => format!("sha256={}", short_digest(value)),
            (None, Some(value)) => format!(
                "sha256={} (computed by this call; no digest was declared)",
                short_digest(value)
            ),
            _ => "sha256=<none>".to_string(),
        };
        match &self.state {
            ModelFileState::Present => format!("{} ({}) present {digest}", self.name, self.role),
            ModelFileState::Missing => format!("{} ({}) missing", self.name, self.role),
            ModelFileState::Corrupt { actual, .. } => {
                format!("{} ({}) corrupt (actual: {actual})", self.name, self.role)
            }
        }
    }
}

/// 摘要的前 16 个字符（日志里的一行结论不需要全量摘要）。
fn short_digest(value: &str) -> &str {
    &value[..16.min(value.len())]
}

/// 启动期解析出的公式检测模型：路径 + **集合声明的** SHA-256（`None` = 没有可信摘要）。
///
/// 两者必须一起传递：只有路径的旧形状正是"检测模型从不校验"的根因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FormulaDetectorSpec {
    pub path: PathBuf,
    pub expected_sha256: Option<String>,
}

/// 没有声明摘要时"验证"的失败原因（`None` = 通过）。
///
/// **文档化规则**（不变量，见模块文档）：文件必须
///
/// 1. 非空；
/// 2. 打得开、读得出头几个字节；
/// 3. 看起来是一份 ONNX `ModelProto`：protobuf 的第一个字段是 `ir_version`
///    （tag = `0x08`，varint 取值 1..=64）。
///
/// 这是**启发式**：它拦得住"随便一个文件冒充模型"，拦不住"能写文件的人构造一个
/// 看起来像 ONNX 的坏文件"。没有可信摘要时，服务如实把 `sha256` 报成 `null`，
/// 不声称内容被校验过；A1/A2 仍然会算一次完整摘要并报告它。
fn undeclared_digest_failure(path: &Path) -> Option<String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return Some(format!("unreadable: {error}")),
    };
    let mut head = [0_u8; 8];
    let read = match file.read(&mut head) {
        Ok(read) => read,
        Err(error) => return Some(format!("unreadable: {error}")),
    };
    if read == 0 {
        return Some("the file is empty (0 bytes), so it cannot be an ONNX model".to_string());
    }
    if read < 2 || head[0] != 0x08 {
        return Some(format!(
            "not a plausible ONNX model: the file must start with the protobuf `ir_version` field \
             (0x08 + varint 1..=64), found {:02x?}",
            &head[..read]
        ));
    }
    if !(1..=64).contains(&head[1]) {
        return Some(format!(
            "not a plausible ONNX model: `ir_version` is {} (expected 1..=64)",
            head[1]
        ));
    }
    None
}

/// 两个路径是否指向同一个文件（先规范化；规范化失败时退回字面比较）。
///
/// `--formula-detector` 与模型集里的声明可能一个带 `\\?\` 前缀、一个不带，
/// 也可能一个相对一个绝对；用规范化后的路径比较，CLI 与 role 指向同一个文件时
/// 声明的哈希才不会被丢掉，计划里也不会重复列出同一个文件。
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// 路径的最后一段（响应用的文件名；§7.4 路径脱敏的唯一实现）。
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// 某个管线里缺失/损坏文件的有序清单（**库存**口径；`/api/models` 的 `formula` 块用它）。
///
/// 按 `files[].role` 分组（[`Pipeline::of`]），不按集合：默认表把两条管线放在两个集合里，
/// 本地清单把两条管线放在**一个**集合里，两种来源下结论必须一样。
/// 运行期判定（准入、引擎状态机、A1/A2）用的是**运行计划**口径（[`ModelPlan::plan_blocking`]），
/// 两者都由本模块给出，区别只是"目录里有什么"与"这次会加载什么"。
pub(super) fn blocking_files(statuses: &[ModelSetStatus], pipeline: Pipeline) -> Vec<BlockingFile> {
    let mut out = Vec::new();
    for status in statuses {
        for (file, state) in &status.files {
            if Pipeline::of(file.role) != pipeline {
                continue;
            }
            match state {
                ModelFileState::Present => {}
                ModelFileState::Missing => out.push(BlockingFile {
                    name: file.name.clone(),
                    pipeline,
                    corrupt: false,
                }),
                ModelFileState::Corrupt { .. } => out.push(BlockingFile {
                    name: file.name.clone(),
                    pipeline,
                    corrupt: true,
                }),
            }
        }
    }
    out
}

/// 阻塞清单里缺失文件的文件名（`BlockingFile` → 名字的唯一实现）。
pub(super) fn missing_names(files: &[BlockingFile]) -> Vec<String> {
    files
        .iter()
        .filter(|file| !file.corrupt)
        .map(|file| file.name.clone())
        .collect()
}

/// 阻塞清单里损坏文件的文件名。
pub(super) fn corrupt_names(files: &[BlockingFile]) -> Vec<String> {
    files
        .iter()
        .filter(|file| file.corrupt)
        .map(|file| file.name.clone())
        .collect()
}

/// 阻塞清单里的全部文件名（缺失 ∪ 损坏）。
pub(super) fn blocked_names(files: &[BlockingFile]) -> Vec<String> {
    files.iter().map(|file| file.name.clone()).collect()
}

/// 一个集合里待下载文件的规模（**唯一实现**：磁盘核算、预算预检与下载进度共用它）。
pub(super) fn pending_download(status: &ModelSetStatus) -> PendingDownload {
    let mut files = 0_usize;
    let mut bytes = Some(0_u64);
    for (file, state) in &status.files {
        if state.is_present() {
            continue;
        }
        files += 1;
        bytes = match (bytes, file.size_bytes) {
            (Some(total), Some(size)) => total.checked_add(size),
            _ => None,
        };
    }
    PendingDownload { files, bytes }
}

/// 一次请求上的模型状态报告（**库存**口径；`/api/models` 与 OCR 409 的同一份数据）。
#[derive(Debug, Clone)]
pub(super) struct ModelReport {
    statuses: Vec<ModelSetStatus>,
    /// 这一份报告里**真的**重算了摘要的文件数（其余命中身份键控的校验缓存）。
    ///
    /// 它是"轮询不再重新哈希 566 MB"的可断言证据：页面每 8 s 拉一次 `/api/models`，
    /// 第二次开始这个数字必须是 0。
    cold_this_call: usize,
}

/// 一个集合里"需要下载"的文件规模（§6.5 的核算口径）。
///
/// 与 [`ModelSetStatus::download_bytes_total`] 的区别是**损坏文件也要计入**：损坏的文件会被
/// 重新下载（§6.3），因此它同样需要网络与磁盘空间。`bytes` 为 `None` 表示至少有一个待下载
/// 文件没有声明体积（此时磁盘核算按 `--max-download-mb` 计入，见 §6.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingDownload {
    pub files: usize,
    pub bytes: Option<u64>,
}

impl ModelReport {
    pub fn statuses(&self) -> &[ModelSetStatus] {
        &self.statuses
    }

    /// 这一份报告里真的重算了摘要的文件数（`/api/models.verification.cold_this_call`）。
    pub fn cold_this_call(&self) -> usize {
        self.cold_this_call
    }

    /// 某个集合里待下载文件的规模；集合不存在时为 `None`。
    pub fn pending_download(&self, set_id: &str) -> Option<PendingDownload> {
        self.statuses
            .iter()
            .find(|status| status.set_id == set_id)
            .map(pending_download)
    }

    /// **库存**口径：公式管线缺失文件的文件名（页面用它告诉用户"要下哪个文件"）。
    ///
    /// 运行期判定（公式队列的 409 与 `formula` 的 `pipelines` 块）用运行计划口径
    /// （[`ModelPlan::plan_blocking`]），它还会把 `--formula-detector` 指向的集合外文件算进去。
    pub fn formula_inventory_blocking(&self) -> Vec<BlockingFile> {
        blocking_files(&self.statuses, Pipeline::Formula)
    }

    /// **库存**口径：公式管线是否齐备（每个文件都 `Present` 且有声明哈希，§5.2）。
    pub fn formula_inventory_complete(&self) -> bool {
        let mut seen = false;
        for status in &self.statuses {
            for (file, state) in &status.files {
                if Pipeline::of(file.role) != Pipeline::Formula {
                    continue;
                }
                seen = true;
                if !state.is_present() || !file.has_hash() {
                    return false;
                }
            }
        }
        seen
    }
}

/// 启动期冻结的模型快照（引擎状态机与启动日志的输入）。**按运行计划**分组。
#[derive(Debug, Clone)]
pub(super) struct ModelSnapshot {
    model_dir: PathBuf,
    source: ModelSourceKind,
    /// 文本管线（引擎）的缺失/损坏文件。
    blocking: Vec<BlockingFile>,
    /// 公式管线的缺失/损坏文件（公式不在计划里时为空——那时公式模型不会被加载，
    /// 也不会阻塞任何东西）。
    formula_blocking: Vec<BlockingFile>,
    /// 公式管线是否属于这次运行（启动日志用）。
    formula_in_plan: bool,
}

impl ModelSnapshot {
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// 引擎状态机的输入（§7.6 第 3 步）：**文本管线**的缺失与损坏都要进
    /// `BlockedModelsMissing` 的清单——引擎对两者都不可用，区别只在响应的 `code`
    /// （`models_missing` / `models_corrupt`）与文案上。
    pub fn readiness(&self) -> ModelReadiness {
        if self.blocking.is_empty() {
            ModelReadiness::Complete
        } else {
            ModelReadiness::Incomplete {
                missing: self.blocking_names(),
            }
        }
    }

    pub fn blocking_names(&self) -> Vec<String> {
        blocked_names(&self.blocking)
    }

    /// 公式管线在**这次运行的计划**里缺失/损坏的文件名（日志与诊断用）。
    pub fn formula_blocking_names(&self) -> Vec<String> {
        blocked_names(&self.formula_blocking)
    }

    /// 公式管线是否属于这次运行（= 解析出了公式检测模型）。
    pub fn formula_in_plan(&self) -> bool {
        self.formula_in_plan
    }

    /// 启动日志用的一行摘要（避免把绝对路径写进响应）。
    ///
    /// 两条管线分别给出结论：只报一个总数会让"公式模型没下载"看起来像"引擎起不来"。
    pub fn summary(&self) -> String {
        format!(
            "text pipeline: {} model file(s) missing or corrupt; formula pipeline: {}",
            self.blocking.len(),
            if self.formula_in_plan {
                format!(
                    "{} model file(s) missing or corrupt",
                    self.formula_blocking.len()
                )
            } else {
                "not part of this run (no formula detector configured)".to_string()
            }
        )
    }

    /// `/api/models` 与 409 的 `source` 字段文本（与库的枚举同值）。
    pub fn source_label(&self) -> &'static str {
        source_label(self.source)
    }
}

/// `ModelSourceKind` 的展示文本（唯一实现）。
pub(super) fn source_label(kind: ModelSourceKind) -> &'static str {
    match kind {
        ModelSourceKind::DefaultTable => "default_table",
        ModelSourceKind::LocalManifest => "local_manifest",
    }
}

/// 与 `--config` 一致的文本管线选择（§5.4 的集合正是由它决定）。
fn selection(engine: &EngineConfig) -> DefaultModelSelection {
    DefaultModelSelection {
        ocr_version: engine.rec.model.ocr_version,
        det_lang: engine.det.lang,
        rec_lang: engine.rec.model.lang,
        cls_lang: engine.cls.lang,
        model_type: engine.rec.model.model_type,
        include_classifier: engine.global.use_cls,
    }
}

/// 模型集解析失败（启动期错误，带可定位原因）。
#[derive(Debug)]
pub(crate) enum ModelPlanError {
    /// 清单/默认表本身的问题（旧 schema、缺 role、未知版本…）。
    Source(RapidOcrError),
    /// 默认表只有一个 `model_type`，det/rec 冲突时无法表达。
    ModelTypeConflict {
        det: &'static str,
        rec: &'static str,
    },
    /// 解析出的集合为空（理论上不可达，保留为可定位错误而不是空转）。
    NoSets,
    /// 必需的 role 在模型集里不存在（`require_roles` 已在库侧拦过一次）。
    MissingRole { role: ModelRole },
    /// 同一个 role 出现在两个集合里且文件名不同。
    AmbiguousRole {
        role: ModelRole,
        first: String,
        second: String,
    },
}

impl std::fmt::Display for ModelPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(f, "{error}"),
            Self::ModelTypeConflict { det, rec } => write!(
                f,
                "the default model table selects one `model_type` for the whole pipeline, but the \
                 configuration uses det.model_type={det} and rec.model.model_type={rec}; make them \
                 equal, or describe this directory with a local manifest.json (docs/05 §5.3)"
            ),
            Self::NoSets => write!(
                f,
                "the selected model source produced no model set for the text pipeline"
            ),
            Self::MissingRole { role } => write!(
                f,
                "the model set does not declare the required role `{role}`"
            ),
            Self::AmbiguousRole {
                role,
                first,
                second,
            } => write!(
                f,
                "the role `{role}` is declared twice with different files (`{first}` and \
                 `{second}`); serve cannot tell which one the engine must load"
            ),
        }
    }
}

impl std::error::Error for ModelPlanError {}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rapid_ocr_rs::{EngineConfig, ModelFileState, ModelRole, sha256_file};

    use super::{ModelPlan, ModelPlanError, Pipeline, undeclared_digest_failure};

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m1-plan-fixture")
    }

    /// 一份"看起来像 ONNX"的夹具内容：protobuf 序言 = 字段 1（`ir_version`）= 7。
    ///
    /// 没有声明摘要的文件按模块文档的文档化规则判定，因此夹具必须过这一关；
    /// 反过来说，任何"随便写点字节当模型"的夹具都必须显式声明摘要。
    fn plausible_onnx_bytes(tag: &str) -> Vec<u8> {
        let mut bytes = vec![0x08, 0x07];
        bytes.extend_from_slice(tag.as_bytes());
        bytes
    }

    fn resolve(dir: &Path) -> ModelPlan {
        ModelPlan::resolve(dir, &EngineConfig::default(), None).expect("model plan")
    }

    /// 写一份本地清单目录：文件内容由 `files` 给出，清单里的 `sha256` 是**真实**摘要。
    ///
    /// 用得着它的地方是"某个文件必须是健康的、另一个必须不是"——只写占位内容 + 假摘要会让
    /// 每个文件都变成 `corrupt`，那样断言就测不到想测的东西。
    fn write_manifest(dir: &Path, files: &[(&str, &str, Vec<u8>)]) {
        std::fs::create_dir_all(dir).expect("fixture dir");
        let mut manifest = String::from(
            "{\"schema_version\":1,\"id\":\"t\",\"family\":\"PP-OCR\",\"version\":\"v6\",\
             \"files\":[",
        );
        for (index, (name, role, body)) in files.iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(&path, body).expect("fixture file");
            let sha = sha256_file(&path).expect("hash the fixture");
            if index > 0 {
                manifest.push(',');
            }
            manifest.push_str(&format!(
                "{{\"name\":\"{name}\",\"role\":\"{role}\",\"sha256\":\"{sha}\"}}"
            ));
        }
        manifest.push_str("]}");
        std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
    }

    /// 一份健康的本地清单（det/rec/dict + formula_recognizer）。
    fn healthy_files() -> Vec<(&'static str, &'static str, Vec<u8>)> {
        vec![
            (
                "det.onnx",
                "detector",
                plausible_onnx_bytes("healthy detector"),
            ),
            (
                "rec.onnx",
                "recognizer",
                plausible_onnx_bytes("healthy recognizer"),
            ),
            (
                "dict.txt",
                "dictionary",
                plausible_onnx_bytes("healthy dictionary"),
            ),
            (
                "fx.onnx",
                "formula_recognizer",
                plausible_onnx_bytes("healthy formula recognizer"),
            ),
        ]
    }

    /// 默认表（无 `manifest.json`）下解析出的**两条**管线：文本集合 + 公式集合。
    #[test]
    fn the_default_table_describes_the_configured_pipeline() {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let plan = resolve(&dir);
        let report = plan.report();
        assert_eq!(report.statuses().len(), 2, "text set + formula set (M4)");
        let set = &report.statuses()[0];
        let roles: Vec<&str> = set
            .files
            .iter()
            .map(|(file, _)| file.role.as_str())
            .collect();
        assert!(roles.contains(&"detector"), "{roles:?}");
        assert!(roles.contains(&"recognizer"), "{roles:?}");
        assert!(roles.contains(&"dictionary"), "{roles:?}");
        // 第二个集合是公式集合：只有 `formula_recognizer`（公式检测模型是可选的，
        // 默认表不登记它，见 assets/default_models.yaml 的 `formula:` 段注释）。
        let formula = &report.statuses()[1];
        let formula_roles: Vec<&str> = formula
            .files
            .iter()
            .map(|(file, _)| file.role.as_str())
            .collect();
        assert_eq!(formula_roles, vec!["formula_recognizer"]);
        assert_eq!(formula.set_id, "PP-FormulaNet_plus-M");
        let (name, size) = (
            formula.files[0].0.name.clone(),
            formula.files[0].0.size_bytes,
        );
        assert_eq!(name, "pp_formulanet_plus_m.onnx");
        assert!(
            size.is_some_and(|bytes| bytes > 500_000_000),
            "the page must be able to show the ~566 MB size before downloading: {size:?}"
        );
        assert!(formula.files[0].0.has_hash(), "the set declares a SHA-256");
        assert!(formula.files[0].0.has_source_url(), "and a trusted source");
        assert!(!formula.complete, "the fixture dir has no formula model");

        // 空目录 → 文本管线全部缺失，且缺失清单顺序与声明顺序一致。
        let snapshot = plan.snapshot();
        let declared: Vec<String> = set
            .files
            .iter()
            .map(|(file, _)| file.name.clone())
            .collect();
        assert_eq!(snapshot.blocking_names(), declared);
        assert_eq!(plan.plan_blocking(Pipeline::Text).len(), declared.len());
        assert_eq!(
            super::blocked_names(&plan.plan_blocking(Pipeline::Text)),
            declared
        );
        assert!(!plan.plan_complete(Pipeline::Text));
        assert!(matches!(
            snapshot.readiness(),
            super::ModelReadiness::Incomplete { .. }
        ));

        // 公式管线单独报告（**不影响**上面的引擎结论），但这次运行**没有**公式管线：
        // 没有检测模型 ⇒ 566 MB 的识别模型不在计划里、A1/A2 一个字节都不读它。
        assert_eq!(
            super::blocked_names(&report.formula_inventory_blocking()),
            vec![name.clone()],
            "the inventory reports the formula model as missing (the page offers the download)"
        );
        assert!(!report.formula_inventory_complete());
        assert!(!plan.formula_in_plan());
        assert!(plan.plan_files_in(Pipeline::Formula).is_empty());
        assert!(
            snapshot.formula_blocking_names().is_empty(),
            "the run plan has no formula pipeline, so nothing formula-related blocks"
        );
        assert!(!snapshot.formula_in_plan());
        assert_eq!(plan.plan_files().len(), declared.len());
        // 默认表不登记公式检测模型：解析结果是 `None`（正常），不是错误。
        assert!(plan.formula_detector().is_none());
    }

    /// **M4 的核心不变量**：公式模型缺失绝不能让文本引擎被阻塞。
    ///
    /// 模型目录里只有文本管线需要的文件时（内容故意是占位字节 → 哈希不匹配）：
    /// 文本管线的清单与公式管线的清单**互不混入**，公式的缺失不进引擎的 409 清单。
    #[test]
    fn a_missing_formula_model_never_blocks_the_text_pipeline() {
        let dir = fixture_dir().join("text-only-complete");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let plan = resolve(&dir);

        // 引擎要加载的文本文件（名字由模型表决定，测试不写死）：把它们以**占位内容**写到
        // 磁盘上——存在但不是真模型（哈希必然不匹配）。
        let text_files: Vec<String> = plan
            .plan_files_in(Pipeline::Text)
            .into_iter()
            .map(|file| file.name.clone())
            .collect();
        assert_eq!(text_files.len(), 3, "{text_files:?}");
        for name in &text_files {
            std::fs::write(dir.join(name), b"placeholder").expect("fixture file");
        }

        let plan = resolve(&dir);
        // 文本文件都在磁盘上、哈希全部不匹配 → 三个都进 corrupt 清单（而不是 missing）。
        let blocking = plan.plan_blocking(Pipeline::Text);
        assert_eq!(super::missing_names(&blocking), Vec::<String>::new());
        assert_eq!(super::corrupt_names(&blocking), text_files);
        assert!(!plan.plan_complete(Pipeline::Text));
        // 公式管线：不在计划里，也**不混进**文本管线的清单。
        assert!(plan.plan_blocking(Pipeline::Formula).is_empty());
        for name in super::blocked_names(&blocking) {
            assert!(
                !name.contains("formula"),
                "the engine list must stay text-scoped: {name}"
            );
        }
        // 库存仍然如实报告公式模型缺失（页面据此提供下载），但它与引擎结论无关。
        assert_eq!(
            super::blocked_names(&plan.report().formula_inventory_blocking()),
            vec!["pp_formulanet_plus_m.onnx"]
        );
        // 引擎状态机因此只看文本清单：公式缺失不进 `BlockedModelsMissing` 的清单。
        let snapshot = plan.snapshot();
        assert_eq!(snapshot.blocking_names(), text_files);
        assert_eq!(
            super::corrupt_names(&plan.plan_blocking(Pipeline::Text)),
            text_files,
            "the text files exist but do not match their declared digests"
        );
        assert!(snapshot.formula_blocking_names().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 公式检测模型在场时，公式管线（识别 + 检测）才进入运行计划。
    #[test]
    fn the_formula_pipeline_is_planned_only_when_a_detector_is_resolved() {
        let dir = fixture_dir().join("formula-plan");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let detector = dir.join("mfd.onnx");
        std::fs::write(&detector, plausible_onnx_bytes("detector fixture"))
            .expect("fixture detector");

        // 没有检测模型：计划里只有文本三个文件（默认表不登记公式检测 role）。
        let without = resolve(&dir);
        assert!(!without.formula_in_plan());
        let names: Vec<&str> = without
            .plan_files()
            .iter()
            .map(|file| file.name.as_str())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(!names.contains(&"pp_formulanet_plus_m.onnx"));

        // 给出 `--formula-detector`：公式识别 + 检测都进计划，且检测模型带**它自己的**路径。
        let with = ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&detector))
            .expect("model plan");
        assert!(with.formula_in_plan());
        assert_eq!(with.plan_files_in(Pipeline::Formula).len(), 2);
        let planned = with
            .plan_file(ModelRole::FormulaDetector)
            .expect("the detector is planned");
        assert_eq!(planned.path, detector);
        assert_eq!(planned.name, "mfd.onnx");
        assert!(
            !planned.has_declared_digest(),
            "an external detector has no declared digest"
        );
        // 检测模型在模型目录之外（这里只是名字不同）也不影响计划。
        let recognizer = with
            .plan_file(ModelRole::FormulaRecognizer)
            .expect("the formula recognizer is planned with a detector");
        assert_eq!(recognizer.path, dir.join("pp_formulanet_plus_m.onnx"));
        assert_eq!(
            recognizer.declared_sha256.as_deref().map(str::len),
            Some(64),
            "the default table's declared SHA-256 travels with the planned file"
        );
        assert!(
            !with.plan_complete(Pipeline::Formula),
            "the 566 MB model is absent"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 公式识别模型与检测模型都来自集合时，两者都在计划里，且计划**不重复**列出同一个文件。
    #[test]
    fn a_set_declared_detector_is_planned_once_with_its_declared_digest() {
        let dir = fixture_dir().join("set-detector-plan");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let declared = "a".repeat(64);
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                    "files":[{{"name":"det.onnx","role":"detector","sha256":"aa"}},
                             {{"name":"rec.onnx","role":"recognizer","sha256":"bb"}},
                             {{"name":"dict.txt","role":"dictionary","sha256":"cc"}},
                             {{"name":"fx.onnx","role":"formula_recognizer","sha256":"dd"}},
                             {{"name":"mfd.onnx","role":"formula_detector","sha256":"{declared}"}}]}}"#
            ),
        )
        .expect("manifest");
        std::fs::write(dir.join("mfd.onnx"), plausible_onnx_bytes("set detector"))
            .expect("fixture file");

        // 集合声明了检测模型 → 公式路由启用（CLI 不参与）。
        let plan = resolve(&dir);
        assert!(plan.formula_in_plan());
        let detectors: Vec<&str> = plan
            .plan_files_in(Pipeline::Formula)
            .into_iter()
            .filter(|file| file.role == ModelRole::FormulaDetector)
            .map(|file| file.name.as_str())
            .collect();
        assert_eq!(detectors, vec!["mfd.onnx"], "planned exactly once");
        let planned = plan.plan_file(ModelRole::FormulaDetector).expect("planned");
        assert_eq!(planned.declared_sha256.as_deref(), Some(declared.as_str()));
        assert_eq!(plan.plan_files_in(Pipeline::Formula).len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 评审 P1-1 的根因回归：检测模型的 SHA-256 必须与识别模型**同一条规则**地
    /// 跟着"被选中的那个文件"走，而不是在解析时被丢掉。
    #[test]
    fn the_formula_detector_hash_travels_with_the_selected_file() {
        let dir = fixture_dir().join("detector-hash");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let declared = "a".repeat(64);
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                    "files":[{{"name":"det.onnx","role":"detector","sha256":"aa"}},
                             {{"name":"rec.onnx","role":"recognizer","sha256":"bb"}},
                             {{"name":"dict.txt","role":"dictionary","sha256":"cc"}},
                             {{"name":"fx.onnx","role":"formula_recognizer","sha256":"dd"}},
                             {{"name":"mfd.onnx","role":"formula_detector","sha256":"{declared}"}}]}}"#
            ),
        )
        .expect("manifest");
        std::fs::write(dir.join("mfd.onnx"), b"placeholder").expect("fixture file");

        // role 本身：路径 + 集合声明的哈希一起进计划。
        let plan = ModelPlan::resolve(&dir, &EngineConfig::default(), None).expect("manifest");
        let spec = plan
            .formula_detector()
            .expect("the role is declared")
            .clone();
        assert_eq!(spec.path, dir.join("mfd.onnx"));
        assert_eq!(
            spec.expected_sha256.as_deref(),
            Some(declared.as_str()),
            "the declared hash must not be dropped"
        );
        assert_eq!(
            plan.plan_file(ModelRole::FormulaDetector)
                .expect("planned")
                .declared_sha256
                .as_deref(),
            Some(declared.as_str())
        );

        // CLI 指向**同一个**文件（写法不同）→ 声明的哈希仍然跟随，且计划里只有它一个。
        let indirect = dir.join(".").join("mfd.onnx");
        let plan =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&indirect)).expect("manifest");
        let spec = plan.formula_detector().expect("cli override").clone();
        assert_eq!(
            spec.expected_sha256.as_deref(),
            Some(declared.as_str()),
            "a CLI path that resolves to the declared file keeps its hash"
        );
        let detectors: Vec<&Path> = plan
            .plan_files_in(Pipeline::Formula)
            .into_iter()
            .filter(|file| file.role == ModelRole::FormulaDetector)
            .map(|file| file.path.as_path())
            .collect();
        assert_eq!(
            detectors.len(),
            1,
            "the same file must not be planned twice"
        );

        // CLI 指向集合没有声明过的文件 → 没有可信摘要（如实为 `None`，
        // 而不是借用另一个文件的哈希来"看起来校验过"），且集合里那份**不进计划**
        // （它不会被加载）。
        let other = dir.join("other-mfd.onnx");
        std::fs::write(&other, b"placeholder").expect("fixture file");
        let plan =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&other)).expect("manifest");
        assert_eq!(plan.formula_detector().expect("cli").path, other);
        assert_eq!(plan.formula_detector().expect("cli").expected_sha256, None);
        let planned = plan.plan_file(ModelRole::FormulaDetector).expect("planned");
        assert_eq!(planned.path, other);
        assert!(!planned.has_declared_digest());
        for file in plan.plan_files() {
            assert_ne!(
                file.path,
                dir.join("mfd.onnx"),
                "a file that will not be loaded must not be planned"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 需求 4：集合之外的 `--formula-detector` 没有声明摘要时，仍然**在计划里**、
    /// 被冷验证，并按文档化规则（存在 + 可读 + 像 ONNX）如实报告状态。
    #[test]
    fn an_external_detector_without_a_declared_digest_follows_the_documented_rule() {
        let dir = fixture_dir().join("external-detector");
        write_manifest(&dir, &healthy_files());
        // 检测模型在模型目录**之外**：不属于任何集合，因此没有可信摘要。
        let outside = fixture_dir().join("external-detector-outside.onnx");
        std::fs::write(&outside, plausible_onnx_bytes("external detector")).expect("detector");

        let plan =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&outside)).expect("model plan");
        let planned = plan.plan_file(ModelRole::FormulaDetector).expect("planned");
        assert_eq!(planned.path, outside);
        assert!(!planned.has_declared_digest());
        assert_eq!(
            planned.state(),
            ModelFileState::Present,
            "exists + readable + plausible ONNX"
        );
        assert_eq!(
            super::blocked_names(&plan.plan_blocking(Pipeline::Formula)),
            Vec::<String>::new(),
            "everything in the formula plan is usable"
        );

        // 内容不是 ONNX → 文档化规则必须把它判成 corrupt（而不是"没有摘要所以算了"）。
        std::fs::write(&outside, b"not a model at all").expect("corrupt the detector");
        let planned = plan.plan_file(ModelRole::FormulaDetector).expect("planned");
        assert!(matches!(planned.state(), ModelFileState::Corrupt { .. }));
        assert_eq!(
            super::blocked_names(&plan.plan_blocking(Pipeline::Formula)),
            vec!["external-detector-outside.onnx"],
            "the external detector blocks the formula pipeline and is named"
        );

        // 空文件同样被拒绝（规则里"非空"那一条）。
        std::fs::write(&outside, b"").expect("empty the detector");
        assert!(undeclared_digest_failure(&outside).is_some());

        // 文件不在 → `Missing`（与"在但不对"是两种结论）。
        std::fs::remove_file(&outside).expect("remove the detector");
        let planned = plan.plan_file(ModelRole::FormulaDetector).expect("planned");
        assert_eq!(planned.state(), ModelFileState::Missing);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&outside).ok();
    }

    /// 声明了摘要时判定完全按库的逐文件校验（哈希是权威，不看序言）。
    #[test]
    fn a_declared_digest_is_enforced_and_is_the_authority() {
        let dir = fixture_dir().join("declared-digest");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let body = plausible_onnx_bytes("detector with a declared digest");
        std::fs::write(dir.join("det.onnx"), &body).expect("fixture file");
        let sha = sha256_file(dir.join("det.onnx")).expect("hash");
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                    "files":[{{"name":"det.onnx","role":"detector","sha256":"{sha}"}},
                             {{"name":"rec.onnx","role":"recognizer","sha256":"bb"}},
                             {{"name":"dict.txt","role":"dictionary","sha256":"cc"}},
                             {{"name":"fx.onnx","role":"formula_recognizer","sha256":"dd"}}]}}"#
            ),
        )
        .expect("manifest");

        let plan = resolve(&dir);
        let planned = plan.plan_file(ModelRole::Detector).expect("planned");
        assert_eq!(planned.state(), ModelFileState::Present);
        // 内容与声明的摘要一致 → 即使它不像 ONNX 也是 `Present`（摘要就是权威；
        // 这里用同一份声明摘要 + 改名的文件证明判定只看摘要）。
        let mut other = plausible_onnx_bytes("x");
        other.clear();
        other.extend_from_slice(b"garbage whose declared digest matches");
        std::fs::write(dir.join("det.onnx"), &other).expect("rewrite");
        let sha = sha256_file(dir.join("det.onnx")).expect("hash");
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                    "files":[{{"name":"det.onnx","role":"detector","sha256":"{sha}"}},
                             {{"name":"rec.onnx","role":"recognizer","sha256":"bb"}},
                             {{"name":"dict.txt","role":"dictionary","sha256":"cc"}},
                             {{"name":"fx.onnx","role":"formula_recognizer","sha256":"dd"}}]}}"#
            ),
        )
        .expect("manifest");
        let plan = resolve(&dir);
        assert_eq!(
            plan.plan_file(ModelRole::Detector)
                .expect("planned")
                .state(),
            ModelFileState::Present,
            "a matching declared digest is the authority (the prologue rule does not apply)"
        );

        // 摘要不匹配 → `Corrupt`（实际摘要必须被报告）。
        std::fs::write(dir.join("det.onnx"), b"different bytes entirely").expect("rewrite");
        let plan = resolve(&dir);
        match plan
            .plan_file(ModelRole::Detector)
            .expect("planned")
            .state()
        {
            ModelFileState::Corrupt { expected, actual } => {
                assert_eq!(expected, sha, "the declared digest must be reported");
                assert_ne!(actual, sha);
            }
            other => panic!("expected corrupt, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **身份断言（单元层）**：计划、冷验证、快照与加载路径读的是**同一份**清单。
    #[test]
    fn one_plan_feeds_reverification_the_snapshot_and_the_loading_paths() {
        let dir = fixture_dir().join("identity");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let detector = dir.join("mfd.onnx");
        std::fs::write(&detector, plausible_onnx_bytes("identity detector")).expect("detector");
        let plan =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&detector)).expect("plan");

        // 1) 计划本身的文件（名字 + role + 管线）。
        let planned: Vec<(String, ModelRole, Pipeline)> = plan
            .plan_files()
            .iter()
            .map(|file| (file.name.clone(), file.role, file.pipeline))
            .collect();
        assert_eq!(planned.len(), 5, "{planned:?}");

        // 把计划里的每个文件都写到磁盘上（内容任意：默认表声明的摘要必然不匹配），
        // 这样冷验证会为**每一个**计划文件真的算一次摘要。
        for file in plan.plan_files() {
            std::fs::write(&file.path, plausible_onnx_bytes(&file.name)).expect("planned file");
        }
        let plan =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&detector)).expect("plan");

        // 2) 冷验证逐文件结论与计划**同序同集**（含集合外的检测模型）。
        let report = plan.reverify();
        let verified: Vec<(String, ModelRole, Pipeline)> = report
            .files()
            .iter()
            .map(|file| (file.name.clone(), file.role, file.pipeline))
            .collect();
        assert_eq!(verified, planned);
        assert_eq!(report.digests_computed(), planned.len());

        // 3) 快照与计划的分管线分组一致。
        let snapshot = plan.snapshot();
        assert_eq!(
            snapshot.blocking_names(),
            super::blocked_names(&plan.plan_blocking(Pipeline::Text))
        );
        assert_eq!(
            snapshot.formula_blocking_names(),
            super::blocked_names(&plan.plan_blocking(Pipeline::Formula))
        );
        assert!(snapshot.formula_in_plan());

        // 4) 加载路径（引擎路径 + 公式策略的输入）取的也是计划里的路径与声明摘要。
        let mut engine = EngineConfig::default();
        plan.pin_engine_paths(&mut engine)
            .expect("all roles planned");
        assert_eq!(
            engine.det.model_path.as_deref(),
            Some(
                plan.plan_file(ModelRole::Detector)
                    .expect("planned")
                    .path
                    .as_path()
            )
        );
        assert_eq!(
            engine.rec.model.model_path.as_deref(),
            Some(
                plan.plan_file(ModelRole::Recognizer)
                    .expect("planned")
                    .path
                    .as_path()
            )
        );
        assert_eq!(
            engine.rec.model.rec_keys_path.as_deref(),
            Some(
                plan.plan_file(ModelRole::Dictionary)
                    .expect("planned")
                    .path
                    .as_path()
            )
        );
        let detector_spec = plan.formula_detector().expect("detector");
        assert_eq!(
            plan.plan_file(ModelRole::FormulaDetector)
                .expect("planned")
                .path,
            detector_spec.path
        );
        assert_eq!(
            plan.plan_file(ModelRole::FormulaRecognizer)
                .expect("planned")
                .declared_sha256,
            plan.plan_files_in(Pipeline::Formula)
                .into_iter()
                .find(|file| file.role == ModelRole::FormulaRecognizer)
                .expect("planned")
                .declared_sha256
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 公式识别模型损坏 + 公式启用 → 计划里如实报阻塞（启动期 fail-fast 的输入）。
    #[test]
    fn a_corrupt_formula_recognizer_blocks_the_formula_pipeline_when_it_is_planned() {
        let dir = fixture_dir().join("corrupt-formula-recognizer");
        write_manifest(&dir, &healthy_files());
        // 公式识别模型存在但内容与声明不符；文本三个文件与检测模型都是健康的。
        std::fs::write(dir.join("fx.onnx"), b"not the declared bytes").expect("corrupt fx");
        let detector = dir.join("mfd.onnx");
        std::fs::write(&detector, plausible_onnx_bytes("detector")).expect("detector");

        let enabled =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&detector)).expect("plan");
        assert_eq!(
            super::blocked_names(&enabled.plan_blocking(Pipeline::Formula)),
            vec!["fx.onnx"]
        );
        assert!(!enabled.plan_complete(Pipeline::Formula));
        assert!(enabled.plan_complete(Pipeline::Text));

        // 公式关闭：识别模型**不在计划里**，同一个损坏不影响任何结论；
        // 冷验证只读文本管线的三个文件（566 MB 的公式识别模型一个字节都不读）。
        let disabled = resolve(&dir);
        assert!(disabled.plan_blocking(Pipeline::Formula).is_empty());
        assert!(disabled.plan_complete(Pipeline::Formula));
        let report = disabled.reverify();
        assert_eq!(
            report.digests_computed(),
            3,
            "the 566 MB formula recognizer must not be hashed when formula is disabled: {:?}",
            report.files().iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert_eq!(
            report.files().len(),
            3,
            "the formula models are not in this run's plan at all"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 启动期冷验证：计划里每个文件都被真的重算一次摘要，且缺失/损坏如实分类。
    #[test]
    fn reverification_covers_exactly_the_plan() {
        let dir = fixture_dir().join("reverify-plan");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let detector = dir.join("mfd.onnx");
        std::fs::write(&detector, plausible_onnx_bytes("reverify detector")).expect("detector");
        let plan =
            ModelPlan::resolve(&dir, &EngineConfig::default(), Some(&detector)).expect("plan");
        let report = plan.reverify();
        assert_eq!(report.files().len(), plan.plan_files().len());
        // 目录里只有那个集合之外的检测模型：它是唯一"算得出摘要"的文件，其余都缺失。
        assert_eq!(report.digests_computed(), 1);
        for file in report.files() {
            assert_eq!(file.cause.as_str(), "first_sight");
            if file.role == ModelRole::FormulaDetector {
                assert_eq!(file.state.as_str(), "present");
                assert!(file.sha256.is_some(), "{}", file.describe());
                assert_eq!(file.declared_sha256, None);
            } else {
                assert_eq!(file.state.as_str(), "missing", "{}", file.describe());
                assert!(file.sha256.is_none(), "a missing file has no digest");
            }
        }
        assert_eq!(report.blocking().len(), plan.plan_files().len() - 1);
        let summary = report.blocking_summary();
        assert!(summary.contains("text pipeline:"), "{summary}");
        // 公式管线**也**有一行——它缺的是识别模型（默认表那个 566 MB 的文件），
        // 而**不是**那个已经写好的外部检测模型。
        assert!(summary.contains("formula pipeline:"), "{summary}");
        assert!(
            !summary.contains("mfd.onnx"),
            "the external detector is present, so it must not be reported as blocking: {summary}"
        );
        assert!(
            report
                .blocking_in(Pipeline::Text)
                .iter()
                .all(|file| file.name != "mfd.onnx"),
            "the detector must not be reported as a text-pipeline file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// det/rec 的 model_type 冲突在默认表下必须报可定位错误，而不是静默选一个。
    #[test]
    fn a_model_type_conflict_is_rejected_with_a_locating_error() {
        let dir = fixture_dir();
        let mut engine = EngineConfig::default();
        engine.det.model_type = rapid_ocr_rs::ModelType::Server;
        engine.rec.model.model_type = rapid_ocr_rs::ModelType::Mobile;
        let error = ModelPlan::resolve(&dir, &engine, None).expect_err("must reject");
        let text = error.to_string();
        assert!(text.contains("det.model_type"), "{text}");
        assert!(text.contains("rec.model.model_type"), "{text}");
        assert!(matches!(error, ModelPlanError::ModelTypeConflict { .. }));
    }

    /// 引擎路径必须全部落在模型目录里，且三个 `allow_download` 都是 `false`。
    #[test]
    fn engine_paths_are_pinned_to_the_model_set_files() {
        let dir = fixture_dir();
        let plan = resolve(&dir);
        let mut engine = EngineConfig::default();
        engine.det.allow_download = true;
        engine.rec.model.allow_download = true;
        engine.cls.allow_download = true;
        plan.pin_engine_paths(&mut engine).expect("all roles exist");

        let det = engine.det.model_path.expect("detector path");
        let rec = engine.rec.model.model_path.expect("recognizer path");
        let dict = engine.rec.model.rec_keys_path.expect("dictionary path");
        for path in [&det, &rec, &dict] {
            assert_eq!(path.parent(), Some(dir.as_path()), "{}", path.display());
        }
        assert_ne!(det, rec);
        assert!(!engine.det.allow_download);
        assert!(!engine.rec.model.allow_download);
        assert!(!engine.cls.allow_download);
        assert!(
            engine.cls.model_path.is_none(),
            "use_cls=false keeps cls off"
        );
        assert_eq!(engine.det.model_store_dir.as_deref(), Some(dir.as_path()));
    }

    /// `use_cls` 打开时 classifier 属于计划（引擎会加载它）。
    #[test]
    fn the_classifier_joins_the_plan_when_use_cls_is_on() {
        let dir = fixture_dir().join("use-cls");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let mut engine = EngineConfig::default();
        engine.global.use_cls = true;
        let plan = ModelPlan::resolve(&dir, &engine, None).expect("default table");
        assert!(
            plan.plan_file(ModelRole::Classifier).is_some(),
            "the engine loads the classifier when use_cls is on"
        );
        assert_eq!(plan.plan_files_in(Pipeline::Text).len(), 4);
        let mut pinned = EngineConfig::default();
        pinned.global.use_cls = true;
        plan.pin_engine_paths(&mut pinned).expect("planned");
        assert!(pinned.cls.model_path.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 缺 role 时给出可定位错误（这里用 manifest 表达"集合里没有 dictionary"）。
    ///
    /// M4 起公式识别模型也是必需 role，因此错误里会**同时**列出两个缺失 role
    /// （`ModelSet::require_roles` 一次列全，而不是只报第一个）。
    #[test]
    fn a_missing_role_is_reported_by_the_shared_rule() {
        let dir = fixture_dir().join("no-dictionary");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                "files":[{"name":"det.onnx","role":"detector","sha256":"aa"},
                         {"name":"rec.onnx","role":"recognizer","sha256":"bb"}]}"#,
        )
        .expect("manifest");
        let error = ModelPlan::resolve(&dir, &EngineConfig::default(), None).expect_err("reject");
        let text = error.to_string();
        assert!(text.contains("dictionary"), "{text}");
        assert!(
            text.contains("formula_recognizer"),
            "the formula pipeline's required role must be named too: {text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 旧四字段清单必须给出可定位错误（§5.3 第 2 条第 4 项）。
    #[test]
    fn a_legacy_manifest_is_rejected_with_a_migration_hint() {
        let dir = fixture_dir().join("legacy");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"detector":{"file_name":"d.onnx","sha256":"a"}}"#,
        )
        .expect("manifest");
        let error = ModelPlan::resolve(&dir, &EngineConfig::default(), None).expect_err("reject");
        let text = error.to_string();
        assert!(text.contains("schema_version"), "{text}");
        assert!(text.contains("migrate"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 模型集里重复声明同一 role 且文件不同 → 可定位错误。
    #[test]
    fn an_ambiguous_role_is_rejected_instead_of_picking_one() {
        let dir = fixture_dir().join("ambiguous");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                "files":[{"name":"det-a.onnx","role":"detector","sha256":"aa"},
                         {"name":"det-b.onnx","role":"detector","sha256":"bb"},
                         {"name":"rec.onnx","role":"recognizer","sha256":"cc"},
                         {"name":"dict.txt","role":"dictionary","sha256":"dd"},
                         {"name":"fx.onnx","role":"formula_recognizer","sha256":"ee"}]}"#,
        )
        .expect("manifest");
        let error = ModelPlan::resolve(&dir, &EngineConfig::default(), None).expect_err("reject");
        assert!(matches!(
            error,
            ModelPlanError::AmbiguousRole {
                role: ModelRole::Detector,
                ..
            }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **M4**：公式识别模型声明了两次（两个文件）时，启动期就拒绝，而不是挑一个。
    ///
    /// 这条锁住"公式路径不是 `sets.last()` 或 `files.find(...)` 的随意取值"：
    /// `spec_for` 是唯一的解析实现，歧义判定因此对公式 role 同样成立——**即使公式路由
    /// 关闭**（歧义是清单的结构错误，不是"这次跑不跑公式"的问题）。
    #[test]
    fn an_ambiguous_formula_recognizer_is_rejected_at_startup() {
        let dir = fixture_dir().join("ambiguous-formula");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schema_version":1,"id":"t","family":"PP-OCR","version":"v6",
                "files":[{"name":"det.onnx","role":"detector","sha256":"aa"},
                         {"name":"rec.onnx","role":"recognizer","sha256":"cc"},
                         {"name":"dict.txt","role":"dictionary","sha256":"dd"},
                         {"name":"fx-a.onnx","role":"formula_recognizer","sha256":"ee"},
                         {"name":"fx-b.onnx","role":"formula_recognizer","sha256":"ff"}]}"#,
        )
        .expect("manifest");
        let error = ModelPlan::resolve(&dir, &EngineConfig::default(), None).expect_err("reject");
        let text = error.to_string();
        assert!(text.contains("formula_recognizer"), "{text}");
        assert!(text.contains("fx-a.onnx"), "{text}");
        assert!(text.contains("fx-b.onnx"), "{text}");
        assert!(matches!(
            error,
            ModelPlanError::AmbiguousRole {
                role: ModelRole::FormulaRecognizer,
                ..
            }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
