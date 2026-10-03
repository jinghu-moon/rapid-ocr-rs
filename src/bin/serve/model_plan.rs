//! 模型集的解析（§5.3 的单一来源规则）、就绪判定（§7.6 第 3 步）与引擎路径绑定。
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
//! # 两条管线的 role 组（M4）
//!
//! 本模块请求 `ModelRequest::text_and_formula`：文本管线的 role（detector + recognizer +
//! dictionary，`global.use_cls` 为真时再加 classifier）**与**公式管线的 role
//! （`formula_recognizer`）。这与库的 `ModelRequest::required_roles()` 是**同一份**定义，
//! 因此"哪些 role 属于哪条管线"只有一处实现。
//!
//! 关键推论（M4 的核心修复）：就绪判定必须**按 role 组**进行，而不是"所有集合的并集"。
//! 公式模型文件（566 MB，默认不下载）缺失绝不能让文本引擎进入
//! `BlockedModelsMissing`——那会让普通 OCR 返回 409。因此本模块提供两组独立结论：
//! 文本组（引擎状态机与 `/api/ocr` 的 409 用它）与公式组（公式队列与 `/api/models`
//! 的 `formula` 块用它）。页面也按 `files[].role` 分组，两边判据一致。

use std::path::{Path, PathBuf};

use rapid_ocr_rs::{
    DefaultModelSelection, EngineConfig, ModelFileState, ModelRequest, ModelRole, ModelSet,
    ModelSetStatus, ModelSource, ModelSourceKind, RapidOcrError,
};

use super::state::ModelReadiness;

/// 一条管线（§8.1 的双队列各自需要哪些 role）。
///
/// 判定**只按 role**，不按"集合 id"或集合在响应里的顺序：本地清单只产生一个集合、
/// 默认表产生两个，同一条规则（见 [`Pipeline::of`]）在两种来源下都必须成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pipeline {
    Text,
    Formula,
}

impl Pipeline {
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

/// 启动期解析某个目录得到的结果（本次进程**唯一**的模型清单）。
#[derive(Debug, Clone)]
pub(super) struct ModelPlan {
    model_dir: PathBuf,
    source: ModelSourceKind,
    sets: Vec<ModelSet>,
}

impl ModelPlan {
    /// §5.3 的单一来源选择 + 与 `--config` 一致的管线选择。
    pub fn resolve(model_dir: &Path, engine: &EngineConfig) -> Result<Self, ModelPlanError> {
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

        // M4：清单必须同时覆盖文本与公式两条管线（`/api/models` 要把公式集合报告出来，
        // 页面才能显示它的体积并按用户点击下载）。缺少 role 时由库的
        // `ModelSet::require_roles` 报"缺哪些 role"，不静默降级（§5.3）。
        let request = ModelRequest::text_and_formula(selection(engine));
        let sets = source
            .model_sets(&request)
            .map_err(ModelPlanError::Source)?;
        if sets.is_empty() {
            return Err(ModelPlanError::NoSets);
        }
        let plan = Self {
            model_dir: model_dir.to_path_buf(),
            source: source.kind(),
            sets,
        };
        // 公式识别模型是公式队列的**必需**文件（`ModelRequest::formula_roles`），
        // 这里立刻解析一次，让"公式集合声明了同一 role 的两个不同文件"这类
        // 结构性错误在**启动期**就带上管线信息报出来，而不是等到第一次公式请求。
        plan.formula_recognizer()?;
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

    /// 每个集合的逐文件状态（§5.2 的唯一实现是库里的 `validate_model_files`）。
    ///
    /// **每次调用都会重新读盘并重新哈希**（文本模型 10–30 MB，公式模型约 566 MB）：
    /// `/api/models` 与 `/api/ocr` 的 409 因此总是报告磁盘上的**当前**事实，而不是启动时的
    /// 快照（M2 下载完成后页面必须看到 `present`），两者也就必然一致。请求路径上**不**调它
    /// （公式队列的准入用 [`Self::missing_on_disk`] 的廉价存在性检查，哈希在识别器加载时
    /// 由库用 `expected_model_sha256` 完成）。
    pub fn report(&self) -> ModelReport {
        ModelReport {
            statuses: self
                .sets
                .iter()
                .map(|set| set.status(&self.model_dir))
                .collect(),
        }
    }

    /// 某个管线里**磁盘上不存在**的文件名（只 `stat`，**不哈希**）。
    ///
    /// 这是"请求路径上的廉价预检"：真正的权威判定是库在加载文件时用集合声明的
    /// SHA-256 做的校验（`FormulaRecognizer::from_model_with_hash`）。两者分工明确：
    /// 这里回答"要不要现在就去下载"，那里回答"下到的东西对不对"。
    pub fn missing_on_disk(&self, pipeline: Pipeline) -> Vec<String> {
        let mut out = Vec::new();
        for set in &self.sets {
            for file in &set.files {
                if Pipeline::of(file.role) != pipeline {
                    continue;
                }
                if !self.model_dir.join(&file.name).is_file() {
                    out.push(file.name.clone());
                }
            }
        }
        out
    }

    /// 启动期快照：引擎状态机的输入（§7.6 第 3 步）。**引擎只看文本管线**。
    pub fn snapshot(&self) -> ModelSnapshot {
        // `report()` 会读盘并哈希（公式模型约 566 MB），因此只算一次。
        let report = self.report();
        ModelSnapshot {
            model_dir: self.model_dir.clone(),
            source: self.source,
            blocking: blocking_files(report.statuses(), Pipeline::Text),
            formula_blocking: blocking_files(report.statuses(), Pipeline::Formula),
        }
    }

    /// 把引擎配置的三个模型路径钉在模型集的文件上（见模块文档）。
    pub fn pin_engine_paths(&self, engine: &mut EngineConfig) -> Result<(), ModelPlanError> {
        let det = self.file_for(ModelRole::Detector)?;
        engine.det.model_path = Some(det);
        engine.det.model_store_dir = Some(self.model_dir.clone());
        engine.det.allow_download = false;

        let rec = self.file_for(ModelRole::Recognizer)?;
        engine.rec.model.model_path = Some(rec);
        engine.rec.model_store_dir = Some(self.model_dir.clone());
        engine.rec.model.allow_download = false;

        let dict = self.file_for(ModelRole::Dictionary)?;
        engine.rec.model.rec_keys_path = Some(dict);

        if engine.global.use_cls {
            let cls = self.file_for(ModelRole::Classifier)?;
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

    /// 某个 role 在模型集里的绝对路径。
    ///
    /// 每个 role 必须**唯一**地映射到一个文件：重复出现（两个集合声明同一 role 的不同文件，
    /// 或同一个清单里声明两次）会返回 [`ModelPlanError::AmbiguousRole`]，而不是"悄悄用第一个"。
    /// 公式识别模型走的是**同一个**函数，因此歧义判定只有一份实现。
    fn file_for(&self, role: ModelRole) -> Result<PathBuf, ModelPlanError> {
        let spec = self
            .spec_for(role)?
            .ok_or(ModelPlanError::MissingRole { role })?;
        Ok(self.model_dir.join(&spec.name))
    }

    /// 某个 role 在模型集里的文件描述（`None` = 该 role 没有被任何集合声明）。
    ///
    /// [`Self::file_for`] 的共享实现：两者都经这里做"重复声明 → `AmbiguousRole`"的判定。
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

    /// 公式识别模型（路径 + 集合声明的 SHA-256）。
    ///
    /// 它是公式队列的必需文件（`ModelRequest::formula_roles`），因此缺失是可定位的启动期
    /// 错误；`expected_model_sha256` 由集合给出，加载时由库校验——这就是"下到的东西对不对"
    /// 的权威判定。歧义（同一 role 两个文件）在这里同样被拒绝。
    pub fn formula_recognizer(&self) -> Result<(PathBuf, String), ModelPlanError> {
        let role = ModelRole::FormulaRecognizer;
        let spec = self
            .spec_for(role)?
            .ok_or(ModelPlanError::MissingRole { role })?;
        Ok((self.model_dir.join(&spec.name), spec.sha256.clone()))
    }

    /// 公式检测模型（`formula_detector` role）。
    ///
    /// 它在 `FormulaPolicy` 里是**可选**的（默认表不把它登记为集合成员：没有可信的公开
    /// 下载来源），因此 `None` 是正常结果，不是错误；但一旦集合声明了它，就只认集合里的
    /// 那一个文件（不静默回落到别的路径）。
    pub fn formula_detector(&self) -> Result<Option<PathBuf>, ModelPlanError> {
        Ok(self
            .spec_for(ModelRole::FormulaDetector)?
            .map(|spec| self.model_dir.join(&spec.name)))
    }
}

/// 某个管线里缺失/损坏文件的有序清单（启动期快照与 `/api/models` 的**同一实现**）。
///
/// 按 `files[].role` 分组（[`Pipeline::of`]），不按集合：默认表把两条管线放在两个集合里，
/// 本地清单把两条管线放在**一个**集合里，两种来源下结论必须一样。
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

/// 一次请求上的模型状态报告（`/api/models` 与 OCR 409 的**同一份**数据）。
#[derive(Debug, Clone)]
pub(super) struct ModelReport {
    statuses: Vec<ModelSetStatus>,
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

    /// 某个集合里待下载文件的规模；集合不存在时为 `None`。
    pub fn pending_download(&self, set_id: &str) -> Option<PendingDownload> {
        self.statuses
            .iter()
            .find(|status| status.set_id == set_id)
            .map(pending_download)
    }

    /// 文本管线缺失文件的文件名（按集合与声明顺序）。这是**引擎**看的那一份：
    /// `/api/ocr` 的 409 与 `EngineState::BlockedModelsMissing` 都用它。
    pub fn missing_names(&self) -> Vec<String> {
        self.blocking(Pipeline::Text)
            .into_iter()
            .filter(|file| !file.corrupt)
            .map(|file| file.name)
            .collect()
    }

    /// 文本管线损坏文件的文件名（顺序同上）。
    pub fn corrupt_names(&self) -> Vec<String> {
        self.blocking(Pipeline::Text)
            .into_iter()
            .filter(|file| file.corrupt)
            .map(|file| file.name)
            .collect()
    }

    /// 文本管线的缺失 ∪ 损坏（引擎看两者都不可用）。
    pub fn blocking_names(&self) -> Vec<String> {
        self.blocking(Pipeline::Text)
            .into_iter()
            .map(|file| file.name)
            .collect()
    }

    /// 文本管线是否齐备（**不是**"所有集合都齐备"：公式集合的 566 MB 模型缺失
    /// 绝不能让普通 OCR 变成 409，见模块文档）。
    ///
    /// 齐备 = 文本管线的**每个**文件都 `Present` **且**声明了哈希（§5.2：没有哈希的文件
    /// 只能证明"存在"，因此该集合永远不得报 `complete`）。
    pub fn is_complete(&self) -> bool {
        self.pipeline_complete(Pipeline::Text)
    }

    /// 公式管线缺失文件的文件名。
    pub fn formula_missing_names(&self) -> Vec<String> {
        self.blocking(Pipeline::Formula)
            .into_iter()
            .filter(|file| !file.corrupt)
            .map(|file| file.name)
            .collect()
    }

    /// 公式管线损坏文件的文件名。
    pub fn formula_corrupt_names(&self) -> Vec<String> {
        self.blocking(Pipeline::Formula)
            .into_iter()
            .filter(|file| file.corrupt)
            .map(|file| file.name)
            .collect()
    }

    /// 公式管线的缺失 ∪ 损坏。
    pub fn formula_blocking_names(&self) -> Vec<String> {
        self.blocking(Pipeline::Formula)
            .into_iter()
            .map(|file| file.name)
            .collect()
    }

    /// 公式管线是否齐备（页面用它决定公式开关是否可勾选）。
    pub fn formula_complete(&self) -> bool {
        self.pipeline_complete(Pipeline::Formula)
    }

    /// 某条管线是否齐备：文件都在、且都声明了哈希（§5.2）。
    ///
    /// 与 [`Self::blocking`] 的分工：`blocking` 回答"缺哪些/坏哪些"（给用户看的清单），
    /// 这里回答"够不够用"（给门禁用的结论）。没有哈希的文件不会出现在 `blocking` 里，
    /// 但它让结论永远不是"齐备"——这正是 §5.2 那条规则的落点。
    fn pipeline_complete(&self, pipeline: Pipeline) -> bool {
        let mut seen = false;
        for status in &self.statuses {
            for (file, state) in &status.files {
                if Pipeline::of(file.role) != pipeline {
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

    fn blocking(&self, pipeline: Pipeline) -> Vec<BlockingFile> {
        blocking_files(&self.statuses, pipeline)
    }
}

/// 启动期冻结的模型快照（引擎状态机与启动日志的输入）。
#[derive(Debug, Clone)]
pub(super) struct ModelSnapshot {
    model_dir: PathBuf,
    source: ModelSourceKind,
    /// 文本管线（引擎）的缺失/损坏文件。
    blocking: Vec<BlockingFile>,
    /// 公式管线的缺失/损坏文件（启动日志用；路由开关不依赖它——它只影响
    /// `queue=formula` 的 409，不影响服务可用性）。
    formula_blocking: Vec<BlockingFile>,
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
        self.blocking.iter().map(|file| file.name.clone()).collect()
    }

    /// 公式管线启动期缺失/损坏的文件名（日志与诊断用）。
    pub fn formula_blocking_names(&self) -> Vec<String> {
        self.formula_blocking
            .iter()
            .map(|file| file.name.clone())
            .collect()
    }

    /// 启动日志用的一行摘要（避免把绝对路径写进响应）。
    ///
    /// 两条管线分别给出结论：只报一个总数会让"公式模型没下载"看起来像"引擎起不来"。
    pub fn summary(&self) -> String {
        format!(
            "text pipeline: {} model file(s) missing or corrupt; formula pipeline: {}",
            self.blocking.len(),
            self.formula_blocking.len()
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
    use std::path::PathBuf;

    use rapid_ocr_rs::{EngineConfig, ModelRole};

    use super::{ModelPlan, ModelPlanError, Pipeline};

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m1-plan-fixture")
    }

    /// 默认表（无 `manifest.json`）下解析出的**两条**管线：文本集合 + 公式集合。
    #[test]
    fn the_default_table_describes_the_configured_pipeline() {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let plan = ModelPlan::resolve(&dir, &EngineConfig::default()).expect("default table");
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
        assert_eq!(report.missing_names(), declared);
        assert_eq!(report.corrupt_names(), Vec::<String>::new());
        assert!(!report.is_complete());
        assert!(matches!(
            snapshot.readiness(),
            super::ModelReadiness::Incomplete { .. }
        ));

        // 公式管线单独报告（**不影响**上面的引擎结论）。
        assert_eq!(report.formula_missing_names(), vec![name.clone()]);
        assert!(!report.formula_complete());
        assert_eq!(snapshot.formula_blocking_names(), vec![name.clone()]);
        // 廉价存在性检查与哈希结论在这一场景下一致（空目录）。
        assert_eq!(plan.missing_on_disk(Pipeline::Formula), vec![name]);
        assert_eq!(
            plan.missing_on_disk(Pipeline::Text),
            set.files
                .iter()
                .map(|(file, _)| file.name.clone())
                .collect::<Vec<_>>()
        );
    }

    /// **M4 的核心不变量**：公式模型缺失绝不能让文本引擎被阻塞。
    ///
    /// 模型目录里只有文本管线需要的文件时（内容故意是占位字节 → 哈希不匹配）：
    /// 文本管线的清单与公式管线的清单**互不混入**，公式的缺失不进引擎的 409 清单。
    #[test]
    fn a_missing_formula_model_never_blocks_the_text_pipeline() {
        let dir = fixture_dir().join("text-only-complete");
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let plan = ModelPlan::resolve(&dir, &EngineConfig::default()).expect("default table");

        // 引擎要加载的文本文件（名字由模型表决定，测试不写死）：把它们以**占位内容**写到
        // 磁盘上——存在但不是真模型（哈希必然不匹配）。
        let empty = plan.report();
        let text_files: Vec<String> = empty
            .statuses()
            .iter()
            .flat_map(|status| status.files.iter())
            .filter(|(file, _)| Pipeline::of(file.role) == Pipeline::Text)
            .map(|(file, _)| file.name.clone())
            .collect();
        assert_eq!(text_files.len(), 3, "{text_files:?}");
        for name in &text_files {
            std::fs::write(dir.join(name), b"placeholder").expect("fixture file");
        }

        let plan = ModelPlan::resolve(&dir, &EngineConfig::default()).expect("default table");
        let report = plan.report();
        // 文本文件都在磁盘上、哈希全部不匹配 → 三个都进 corrupt 清单（而不是 missing）。
        assert_eq!(report.missing_names(), Vec::<String>::new());
        assert_eq!(report.corrupt_names(), text_files);
        assert!(!report.is_complete());
        // 公式管线：缺失，且**不混进**文本管线的清单。
        assert_eq!(
            report.formula_missing_names(),
            vec!["pp_formulanet_plus_m.onnx"]
        );
        assert!(!report.formula_complete());
        for name in report.blocking_names() {
            assert!(
                !name.contains("formula"),
                "the engine list must stay text-scoped: {name}"
            );
        }
        // 存在性检查（请求路径上的廉价预检）只看磁盘。
        assert!(plan.missing_on_disk(Pipeline::Text).is_empty());
        assert_eq!(
            plan.missing_on_disk(Pipeline::Formula),
            vec!["pp_formulanet_plus_m.onnx"]
        );
        // 引擎状态机因此只看文本清单：公式缺失不进 `BlockedModelsMissing` 的清单。
        let snapshot = plan.snapshot();
        assert_eq!(snapshot.blocking_names(), text_files);
        assert_eq!(
            snapshot.formula_blocking_names(),
            vec!["pp_formulanet_plus_m.onnx"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 公式识别模型（路径 + 集合声明的哈希）与可选的公式检测模型。
    #[test]
    fn the_formula_roles_resolve_through_the_same_rule() {
        let dir = fixture_dir();
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let plan = ModelPlan::resolve(&dir, &EngineConfig::default()).expect("default table");
        let (path, sha256) = plan.formula_recognizer().expect("formula recognizer");
        assert_eq!(path, dir.join("pp_formulanet_plus_m.onnx"));
        assert_eq!(sha256.len(), 64, "the set's SHA-256 travels with the path");
        // 默认表不登记公式检测模型：`None` 是正常结果，不是错误。
        assert_eq!(plan.formula_detector().expect("optional"), None);
    }

    /// det/rec 的 model_type 冲突在默认表下必须报可定位错误，而不是静默选一个。
    #[test]
    fn a_model_type_conflict_is_rejected_with_a_locating_error() {
        let dir = fixture_dir();
        let mut engine = EngineConfig::default();
        engine.det.model_type = rapid_ocr_rs::ModelType::Server;
        engine.rec.model.model_type = rapid_ocr_rs::ModelType::Mobile;
        let error = ModelPlan::resolve(&dir, &engine).expect_err("must reject");
        let text = error.to_string();
        assert!(text.contains("det.model_type"), "{text}");
        assert!(text.contains("rec.model.model_type"), "{text}");
        assert!(matches!(error, ModelPlanError::ModelTypeConflict { .. }));
    }

    /// 引擎路径必须全部落在模型目录里，且三个 `allow_download` 都是 `false`。
    #[test]
    fn engine_paths_are_pinned_to_the_model_set_files() {
        let dir = fixture_dir();
        let plan = ModelPlan::resolve(&dir, &EngineConfig::default()).expect("default table");
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
        let error = ModelPlan::resolve(&dir, &EngineConfig::default()).expect_err("must reject");
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
        let error = ModelPlan::resolve(&dir, &EngineConfig::default()).expect_err("must reject");
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
        let plan = ModelPlan::resolve(&dir, &EngineConfig::default()).expect("manifest source");
        let mut engine = EngineConfig::default();
        let error = plan
            .pin_engine_paths(&mut engine)
            .expect_err("ambiguous detector");
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
    /// `file_for` / `spec_for` 是唯一的解析实现，歧义判定因此对公式 role 同样成立。
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
        let error = ModelPlan::resolve(&dir, &EngineConfig::default()).expect_err("must reject");
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
