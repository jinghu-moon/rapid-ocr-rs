//! 模型清单的唯一抽象（[`ModelSet`]）与**唯一**的逐文件校验实现。
//!
//! 这个模块回答两个问题，且只在这里回答：
//!
//! 1. **管线需要哪些文件**：由 role（[`ModelRole`]）表达，而不是由 det/cls/rec 三个
//!    固定字段表达——固定字段无法表示 tokenizer 与公式模型（公式识别、公式检测），
//!    这正是旧 `ModelManifest` 的结构性缺口；
//! 2. **这些文件现在处于什么状态**：[`validate_model_files`] 一次返回**每个**文件的
//!    状态，不在第一个错误处提前返回。[`crate::model_source::ModelManifest::validate_files`]
//!    只是它的薄包装，因此不存在第二套逐文件逻辑。
//!
//! # 两个哨兵值（不是 `Option`，因此必须显式判断）
//!
//! [`ModelFileSpec`] 的两个字段在协议里是 `String` 而不是 `Option<String>`，因此
//! “不知道”由**空字符串**表达，并且只有本模块定义它们的含义：
//!
//! - `sha256` 为空 → **该文件没有哈希**：无法校验完整性，因此
//!   [`ModelSetStatus::complete`] 永远不可能为 `true`（见 [`ModelFileSpec::has_hash`]）。
//!   空哈希**不允许**被下载器当作“无需校验”；下载入口只接受具体哈希。
//! - `source_url` 为空 → **没有可信下载来源**（例如只能本地放置的文件），
//!   见 [`ModelFileSpec::has_source_url`]。
//!
//! 调用方一律通过 `has_hash()` / `has_source_url()` 判断，不要自己比较空串。
//!
//! # 状态语义
//!
//! - [`ModelFileState::Missing`]：`<root>/<name>` 不存在；
//! - [`ModelFileState::Present`]：文件存在；**记录了哈希时**哈希必须匹配，没有记录
//!   哈希时只能证明“存在”（这正是它不参与 `complete` 的原因）；
//! - [`ModelFileState::Corrupt`]：文件存在但不可用——哈希不匹配、读不出来，
//!   或者 `name` 本身越出了模型目录（见下）。
//!
//! # 路径规则（唯一定义处）
//!
//! [`validate_model_file_name`] 是路径安全的唯一实现：`name` 必须是**裸相对文件名**，
//! 拒绝绝对路径、路径分隔符、`.`/`..` 与盘符前缀。模型目录是**扁平**的
//! （`model_store::download_verified` 只按 URL 的最后一个路径段落盘），因此带分隔符的
//! 名字在磁盘上不可能与下载产物对应，与其让它静默不匹配，不如直接拒绝。

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::error::{RapidOcrError, Result};

/// 模型文件在管线中的角色。
///
/// 序列化为 `snake_case`（`detector` / `formula_recognizer` / …），因为被服务的页面
/// 以 `files[].role` 为键判断就绪状态；解析失败必须是可定位错误，不能静默变成未知角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Detector,
    Classifier,
    Recognizer,
    Dictionary,
    Tokenizer,
    FormulaDetector,
    FormulaRecognizer,
}

impl ModelRole {
    /// 全部角色，顺序即错误信息里“缺失 role”列表的稳定顺序。
    pub const ALL: [Self; 7] = [
        Self::Detector,
        Self::Classifier,
        Self::Recognizer,
        Self::Dictionary,
        Self::Tokenizer,
        Self::FormulaDetector,
        Self::FormulaRecognizer,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Detector => "detector",
            Self::Classifier => "classifier",
            Self::Recognizer => "recognizer",
            Self::Dictionary => "dictionary",
            Self::Tokenizer => "tokenizer",
            Self::FormulaDetector => "formula_detector",
            Self::FormulaRecognizer => "formula_recognizer",
        }
    }
}

impl std::fmt::Display for ModelRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 模型集合里的单个文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFileSpec {
    /// 相对文件名；必须是裸文件名（见模块文档的路径规则）。
    pub name: String,
    pub role: ModelRole,
    /// 用于下载前的空间核算；`None` 表示未知（跳过预算检查，但仍受流式上限保护）。
    pub size_bytes: Option<u64>,
    /// 期望的 SHA-256；空串表示**没有哈希**（该集合不可能 `complete`）。
    pub sha256: String,
    /// 可信下载来源；空串表示没有来源（只能本地放置）。
    pub source_url: String,
}

impl ModelFileSpec {
    /// 构造并立即校验文件名。
    pub fn new(
        name: impl Into<String>,
        role: ModelRole,
        size_bytes: Option<u64>,
        sha256: impl Into<String>,
        source_url: impl Into<String>,
    ) -> Result<Self> {
        let spec = Self {
            name: name.into(),
            role,
            size_bytes,
            sha256: sha256.into(),
            source_url: source_url.into(),
        };
        spec.validate_name()?;
        Ok(spec)
    }

    /// 是否记录了哈希。`false` 时该文件无法校验，因此集合不可能 `complete`。
    pub fn has_hash(&self) -> bool {
        !self.sha256.trim().is_empty()
    }

    /// 是否有可信下载来源。
    pub fn has_source_url(&self) -> bool {
        !self.source_url.trim().is_empty()
    }

    /// 校验文件名；调用 [`validate_model_file_name`]（唯一实现）。
    pub fn validate_name(&self) -> Result<()> {
        validate_model_file_name(&self.name)
    }

    /// 该文件在 `root` 下的状态。
    ///
    /// 越界的 `name` 不会去读磁盘（那正是要避免的事），直接报 `Corrupt`：
    /// 它既不是 `Present`（没有证据），也不能是 `Missing`（`Missing` 会被前端
    /// 解读为“请下载它”，而这个文件按定义不属于本集合）。
    pub fn state_in(&self, root: &Path) -> ModelFileState {
        self.state_in_probed(root).0
    }

    /// 与 [`Self::state_in`] 相同，但额外回答"**这一次**是否真的读了盘算摘要"。
    ///
    /// 这个布尔量是"身份键控的校验缓存确实命中了"的唯一证据来源：它是**单次调用**的
    /// 属性，不受同一进程里其它线程的影响（见 [`crate::model_verify`] 的模块文档）。
    /// 它也是 `/api/models` 的 `verification.cold_this_call` 的来源：页面每 8 s 轮询一次
    /// 就绪状态时，这个数字必须是 0，而不是"我们相信它命中了"。
    pub fn state_in_probed(&self, root: &Path) -> (ModelFileState, bool) {
        self.state_in_digested(root, &mut String::new())
    }

    /// 与 [`Self::state_in_probed`] 相同，但把**这一次真正的**实际摘要写进 `digest`
    /// （冷验证时是这次算出来的，命中时是缓存里的那一份）。
    ///
    /// 存在的理由：`--reverify-models` 与 `POST /api/models/reverify` 必须同时报告
    /// "状态"与"实际摘要"，而判定规则（`expected` vs `actual`、缺哈希、文件名越界、
    /// 读盘失败）**只能有一份实现**。让调用方就地取走那次验证已经算出的摘要，
    /// 就不会出现"为了拿摘要再哈希一遍"或者"另写一套比较逻辑"的分叉。
    /// 没有可信摘要可言时（文件名越界 / 读不出来 / 缓存里没有）`digest` 被清空。
    pub fn state_in_digested(&self, root: &Path, digest: &mut String) -> (ModelFileState, bool) {
        digest.clear();
        if self.validate_name().is_err() {
            return (
                ModelFileState::Corrupt {
                    expected: self.sha256.clone(),
                    actual: format!(
                        "invalid file name `{}` escapes the model directory",
                        self.name
                    ),
                },
                false,
            );
        }

        let path = root.join(&self.name);
        if !path.is_file() {
            return (ModelFileState::Missing, false);
        }
        if !self.has_hash() {
            // 存在但无哈希：只能证明“存在”，无法证明“正确”，因此不需要读盘。
            return (ModelFileState::Present, false);
        }

        match crate::model_verify::verify_file(&path) {
            Ok(outcome) => {
                let state = if outcome.sha256.eq_ignore_ascii_case(&self.sha256) {
                    ModelFileState::Present
                } else {
                    ModelFileState::Corrupt {
                        expected: self.sha256.clone(),
                        actual: outcome.sha256.clone(),
                    }
                };
                *digest = outcome.sha256;
                (state, outcome.computed)
            }
            Err(error) => (
                ModelFileState::Corrupt {
                    expected: self.sha256.clone(),
                    actual: format!("unreadable: {error}"),
                },
                false,
            ),
        }
    }
}

/// 单个文件的状态。
///
/// 只有三种取值：无法校验完整性**不是**第四种状态，而是 `complete` 的判定条件
/// （没有哈希 → 不可能是 `Present` 意义上的“可用”，见 [`ModelSetStatus::complete`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFileState {
    Missing,
    Present,
    Corrupt { expected: String, actual: String },
}

impl ModelFileState {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Present => "present",
            Self::Corrupt { .. } => "corrupt",
        }
    }

    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present)
    }
}

/// 一个模型集合：`id`/`family`/`version` 描述它是什么，`files` 描述它由哪些文件组成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSet {
    pub id: String,
    pub family: String,
    pub version: String,
    pub files: Vec<ModelFileSpec>,
}

impl ModelSet {
    /// 结构校验：文件名合法且集合内不重名。
    ///
    /// 重名必须拒绝：两个同名文件在同一个扁平目录里是同一个路径，
    /// 状态与下载都会互相覆盖。
    pub fn validate(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.files.len());
        for file in &self.files {
            file.validate_name()?;
            if seen.contains(&file.name.as_str()) {
                return Err(RapidOcrError::ModelResolve(format!(
                    "model set `{}` lists `{}` more than once",
                    self.id, file.name
                )));
            }
            seen.push(&file.name);
        }
        Ok(())
    }

    pub fn has_role(&self, role: ModelRole) -> bool {
        self.files.iter().any(|file| file.role == role)
    }

    /// 集合声明的 role（去重，按 [`ModelRole::ALL`] 的稳定顺序）。
    pub fn declared_roles(&self) -> Vec<ModelRole> {
        ModelRole::ALL
            .into_iter()
            .filter(|role| self.has_role(*role))
            .collect()
    }

    /// 声明的 role 里缺失的那些（按 `required` 的顺序去重）。
    pub fn missing_roles(&self, required: &[ModelRole]) -> Vec<ModelRole> {
        let mut missing = Vec::new();
        for role in required {
            if !self.has_role(*role) && !missing.contains(role) {
                missing.push(*role);
            }
        }
        missing
    }

    /// 缺少必需 role 时返回可定位错误，**列出全部缺失 role**。
    ///
    /// 绝不静默降级：某个来源缺少当前管线需要的 role 时必须报错，
    /// 由调用方决定是换来源还是补齐文件。
    pub fn require_roles(&self, required: &[ModelRole]) -> Result<()> {
        let missing = self.missing_roles(required);
        if missing.is_empty() {
            return Ok(());
        }
        let missing: Vec<&str> = missing.iter().map(|role| role.as_str()).collect();
        let declared: Vec<&str> = self
            .declared_roles()
            .iter()
            .map(|role| role.as_str())
            .collect();
        Err(RapidOcrError::ModelResolve(format!(
            "model set `{}` is missing required roles: {} (declared roles: {})",
            self.id,
            missing.join(", "),
            if declared.is_empty() {
                "<none>".to_string()
            } else {
                declared.join(", ")
            }
        )))
    }

    /// 见 [`model_set_status`]。
    pub fn status(&self, root: &Path) -> ModelSetStatus {
        model_set_status(self, root)
    }

    /// 见 [`model_set_status_probed`]。
    pub fn status_probed(&self, root: &Path) -> (ModelSetStatus, usize) {
        model_set_status_probed(self, root)
    }
}

/// 一个模型集合在某个目录下的完整状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSetStatus {
    pub set_id: String,
    /// 与 `ModelSet::files` **同序**，包含每个文件的状态。
    pub files: Vec<(ModelFileSpec, ModelFileState)>,
    /// 全部文件 `Present` **且**每个文件都有哈希。
    pub complete: bool,
    /// 缺失文件大小之和；有任何缺失文件大小未知（或求和溢出）时为 `None`。
    pub download_bytes_total: Option<u64>,
}

/// 校验模型文件名：必须是裸相对文件名。
pub fn validate_model_file_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    let rejects = |reason: &str| {
        RapidOcrError::ModelResolve(format!(
            "model file name `{name}` is not a bare relative file name ({reason})"
        ))
    };

    if name.trim().is_empty() {
        return Err(rejects("it is empty"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(rejects("it contains a path separator"));
    }
    if path.is_absolute() {
        return Err(rejects("it is an absolute path"));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(rejects(
            "it contains a drive prefix, a root, `.` or `..` component",
        ));
    }
    Ok(())
}

/// 共享实现：一次返回**每个**文件的状态，不在第一个错误处提前返回。
///
/// 返回值与 `files` 同序、等长，因此调用方可以一次把整张表交给前端，
/// 而不是逐个文件重复探测。
pub fn validate_model_files(
    files: &[ModelFileSpec],
    root: &Path,
) -> Vec<(ModelFileSpec, ModelFileState)> {
    validate_model_files_probed(files, root).0
}

/// 与 [`validate_model_files`] 相同，但额外回答"这一轮里有几个文件真的重新算过摘要"。
///
/// 成本账：命中身份键控缓存的文件不计数，因此这个数字就是"这次调用实际读了多少盘"。
pub fn validate_model_files_probed(
    files: &[ModelFileSpec],
    root: &Path,
) -> (Vec<(ModelFileSpec, ModelFileState)>, usize) {
    let mut computed = 0_usize;
    let statuses = files
        .iter()
        .map(|file| {
            let (state, did_compute) = file.state_in_probed(root);
            if did_compute {
                computed += 1;
            }
            (file.clone(), state)
        })
        .collect();
    (statuses, computed)
}

/// 见 [`ModelSet::status`]。
pub fn model_set_status(set: &ModelSet, root: &Path) -> ModelSetStatus {
    model_set_status_probed(set, root).0
}

/// 见 [`ModelSet::status_probed`]：状态 + "这一轮真的算了几次摘要"。
pub fn model_set_status_probed(set: &ModelSet, root: &Path) -> (ModelSetStatus, usize) {
    let (files, computed) = validate_model_files_probed(&set.files, root);
    let complete = !files.is_empty()
        && files
            .iter()
            .all(|(file, state)| state.is_present() && file.has_hash());
    let download_bytes_total = missing_download_bytes(&files);
    (
        ModelSetStatus {
            set_id: set.id.clone(),
            files,
            complete,
            download_bytes_total,
        },
        computed,
    )
}

/// 缺失文件大小之和；有未知大小或求和溢出时为 `None`。
fn missing_download_bytes(files: &[(ModelFileSpec, ModelFileState)]) -> Option<u64> {
    let mut total: u64 = 0;
    for (file, state) in files {
        if matches!(state, ModelFileState::Missing) {
            total = total.checked_add(file.size_bytes?)?;
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::test_support::TempDir;

    /// "hello" 的 SHA-256（与 `model_store` 的既有测试同值）。
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn spec(name: &str, role: ModelRole, sha256: &str, size_bytes: Option<u64>) -> ModelFileSpec {
        ModelFileSpec::new(name, role, size_bytes, sha256, "https://example.com/x").expect("spec")
    }

    fn set(files: Vec<ModelFileSpec>) -> ModelSet {
        ModelSet {
            id: "test-set".to_string(),
            family: "test".to_string(),
            version: "v1".to_string(),
            files,
        }
    }

    #[test]
    fn missing_present_and_corrupt_are_reported_per_file() {
        let dir = TempDir::new("states");
        dir.write("present.onnx", b"hello");
        dir.write("corrupt.onnx", b"not-hello");

        let files = vec![
            spec("present.onnx", ModelRole::Detector, HELLO_SHA256, Some(5)),
            spec("missing.onnx", ModelRole::Recognizer, HELLO_SHA256, None),
            spec("corrupt.onnx", ModelRole::Dictionary, HELLO_SHA256, Some(9)),
        ];
        let status = set(files).status(dir.path());

        assert_eq!(status.set_id, "test-set");
        assert_eq!(status.files.len(), 3);
        assert_eq!(status.files[0].1, ModelFileState::Present);
        assert_eq!(status.files[1].1, ModelFileState::Missing);
        match &status.files[2].1 {
            ModelFileState::Corrupt { expected, actual } => {
                assert_eq!(expected, HELLO_SHA256);
                assert_ne!(actual, HELLO_SHA256);
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
        assert!(!status.complete);
    }

    /// 无哈希的文件即使存在（状态为 `Present`）也不得让集合 `complete`：
    /// “存在”不等于“已校验”。
    #[test]
    fn a_set_with_an_unhashed_file_can_never_be_complete() {
        let dir = TempDir::new("unhashed");
        dir.write("unhashed.txt", b"hello");
        dir.write("hashed.onnx", b"hello");

        let files = vec![
            spec("unhashed.txt", ModelRole::Dictionary, "", Some(5)),
            spec("hashed.onnx", ModelRole::Recognizer, HELLO_SHA256, Some(5)),
        ];
        let status = set(files).status(dir.path());

        assert_eq!(status.files[0].1, ModelFileState::Present);
        assert!(!status.files[0].0.has_hash());
        assert_eq!(status.files[1].1, ModelFileState::Present);
        assert!(
            !status.complete,
            "a set containing an unhashed file must never report complete"
        );

        // 一旦补上哈希，同一个集合就可以 `complete`（证明上面的 false 来自哈希缺口）。
        let files = vec![
            spec("unhashed.txt", ModelRole::Dictionary, HELLO_SHA256, Some(5)),
            spec("hashed.onnx", ModelRole::Recognizer, HELLO_SHA256, Some(5)),
        ];
        assert!(set(files).status(dir.path()).complete);
    }

    #[test]
    fn an_empty_set_is_never_complete() {
        assert!(!set(Vec::new()).status(Path::new(".")).complete);
    }

    #[test]
    fn path_escape_is_rejected_by_the_shared_rule() {
        for name in [
            "../escape.onnx",
            "..\\escape.onnx",
            "sub/model.onnx",
            "/abs/model.onnx",
            "C:/abs/model.onnx",
            "C:\\abs\\model.onnx",
            "./model.onnx",
            "",
        ] {
            assert!(
                validate_model_file_name(name).is_err(),
                "`{name}` must be rejected as a model file name"
            );
            assert!(
                ModelFileSpec::new(name, ModelRole::Detector, None, HELLO_SHA256, "").is_err(),
                "`{name}` must be rejected when constructing a spec"
            );
        }
        for name in [
            "model.onnx",
            "ppocr_keys_v1.txt",
            "PP-OCRv6_det_medium.onnx",
        ] {
            validate_model_file_name(name).expect("bare file names must be accepted");
        }

        // 结构体字面量可以绕过构造器，因此校验函数必须独立生效：
        // 越界名字既不是 `Missing`（不能提示下载），也不是 `Present`。
        let escaped = ModelFileSpec {
            name: "../escape.onnx".to_string(),
            role: ModelRole::Detector,
            size_bytes: Some(7),
            sha256: HELLO_SHA256.to_string(),
            source_url: String::new(),
        };
        let status = set(vec![escaped]).status(Path::new("."));
        assert!(matches!(status.files[0].1, ModelFileState::Corrupt { .. }));
        assert!(!status.complete);
    }

    /// 第一个文件就失败时，**后面每个文件的状态仍然必须返回**。
    #[test]
    fn every_file_is_reported_in_order_and_not_stopped_at_the_first_error() {
        let dir = TempDir::new("order");
        dir.write("c.txt", b"hello");

        let files = vec![
            spec("a.onnx", ModelRole::Detector, HELLO_SHA256, None),
            // 越界名字只能由结构体字面量构造（`ModelFileSpec::new` 直接拒绝），
            // 这正是共享校验函数必须独立生效的原因。
            ModelFileSpec {
                name: "../b.onnx".to_string(),
                role: ModelRole::Classifier,
                size_bytes: None,
                sha256: HELLO_SHA256.to_string(),
                source_url: String::new(),
            },
            spec("c.txt", ModelRole::Dictionary, HELLO_SHA256, None),
            spec("d.onnx", ModelRole::Recognizer, HELLO_SHA256, None),
        ];
        let status = set(files).status(dir.path());

        let order: Vec<&str> = status
            .files
            .iter()
            .map(|(file, _)| file.name.as_str())
            .collect();
        assert_eq!(order, vec!["a.onnx", "../b.onnx", "c.txt", "d.onnx"]);
        assert_eq!(status.files[0].1, ModelFileState::Missing);
        assert!(matches!(status.files[1].1, ModelFileState::Corrupt { .. }));
        assert_eq!(status.files[2].1, ModelFileState::Present);
        assert_eq!(status.files[3].1, ModelFileState::Missing);
    }

    #[test]
    fn download_bytes_total_sums_only_missing_files_with_known_sizes() {
        let dir = TempDir::new("bytes");
        dir.write("present.onnx", b"hello");

        let present = spec("present.onnx", ModelRole::Detector, HELLO_SHA256, Some(5));
        let missing_known = spec("a.onnx", ModelRole::Recognizer, HELLO_SHA256, Some(100));
        let missing_known_2 = spec("b.onnx", ModelRole::Dictionary, HELLO_SHA256, Some(23));
        let missing_unknown = spec("c.onnx", ModelRole::Classifier, HELLO_SHA256, None);

        let total = |files: Vec<ModelFileSpec>| set(files).status(dir.path()).download_bytes_total;

        // 已存在的文件不计入下载量；没有缺失文件时为 Some(0)。
        assert_eq!(total(vec![present.clone()]), Some(0));
        assert_eq!(
            total(vec![
                present.clone(),
                missing_known.clone(),
                missing_known_2.clone()
            ]),
            Some(123)
        );
        // 任一缺失文件大小未知 → 整体未知（`None`），而不是把已知部分当答案。
        assert_eq!(
            total(vec![
                present.clone(),
                missing_known.clone(),
                missing_unknown.clone()
            ]),
            None
        );
        // 未知大小的文件已经存在时不参与求和，因此总和无未知项。
        dir.write("c.onnx", b"hello");
        assert_eq!(
            total(vec![present, missing_known, missing_unknown]),
            Some(100)
        );
    }

    #[test]
    fn require_roles_lists_every_missing_role() {
        let collection = set(vec![
            spec("d.onnx", ModelRole::Detector, HELLO_SHA256, None),
            spec("r.onnx", ModelRole::Recognizer, HELLO_SHA256, None),
        ]);

        collection
            .require_roles(&[ModelRole::Detector, ModelRole::Recognizer])
            .expect("declared roles must satisfy the request");

        let error = collection
            .require_roles(&[
                ModelRole::Detector,
                ModelRole::Dictionary,
                ModelRole::FormulaRecognizer,
            ])
            .expect_err("missing roles must fail");
        let message = error.to_string();
        assert!(message.contains("missing required roles"), "{message}");
        assert!(message.contains("dictionary"), "{message}");
        assert!(message.contains("formula_recognizer"), "{message}");
        assert!(
            message.contains("declared roles: detector, recognizer"),
            "{message}"
        );
    }

    #[test]
    fn duplicate_file_names_are_rejected() {
        let collection = set(vec![
            spec("same.onnx", ModelRole::Detector, HELLO_SHA256, None),
            spec("same.onnx", ModelRole::Recognizer, HELLO_SHA256, None),
        ]);
        let error = collection.validate().expect_err("duplicates must fail");
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn roles_round_trip_through_snake_case_json() {
        for role in ModelRole::ALL {
            let json = serde_json::to_string(&role).expect("role should serialize");
            assert_eq!(json, format!("\"{}\"", role.as_str()));
            let parsed: ModelRole = serde_json::from_str(&json).expect("role should deserialize");
            assert_eq!(parsed, role);
            assert_eq!(role.to_string(), role.as_str());
        }

        let error = serde_json::from_str::<ModelRole>("\"unknown_role\"")
            .expect_err("an unknown role must not parse");
        assert!(error.to_string().contains("unknown_role"), "{error}");
    }

    #[test]
    fn state_labels_are_stable_for_the_frontend() {
        assert_eq!(ModelFileState::Missing.as_str(), "missing");
        assert_eq!(ModelFileState::Present.as_str(), "present");
        assert_eq!(
            ModelFileState::Corrupt {
                expected: "a".into(),
                actual: "b".into()
            }
            .as_str(),
            "corrupt"
        );
        assert!(ModelFileState::Present.is_present());
        assert!(!ModelFileState::Missing.is_present());
    }
}
