//! 模型来源：**单一权威来源**的选择规则，以及本地清单（`manifest.json`）。
//!
//! # 单一来源规则（无合并、无“可选覆盖”）
//!
//! 一个模型目录里的权威描述只有**一个**：
//!
//! 1. 若 `<model_dir>/manifest.json` 存在 → 它是该目录的**唯一来源**，
//!    默认表（`assets/default_models.yaml`）**完全不参与**；
//! 2. 否则 → 默认表是唯一来源。
//!
//! 两条来源都有各自的编码方式，但都不会与对方合并：没有“清单里缺的字段从默认表补”
//! 这种语义（那正是上一版双权威的来源）。若所选来源缺少当前管线需要的 role，
//! [`ModelSet::require_roles`] 会报错并**列出全部缺失 role**，绝不静默降级。
//!
//! # 本地清单格式
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "id": "pp-ocrv6-medium",
//!   "family": "PP-OCR",
//!   "version": "PP-OCRv6",
//!   "languages": ["ch", "en"],
//!   "files": [
//!     { "name": "PP-OCRv6_det_medium.onnx", "role": "detector",
//!       "sha256": "…", "size_bytes": 62119454, "source_url": "https://…" }
//!   ]
//! }
//! ```
//!
//! `schema_version` 是必需的，且必须等于 [`SUPPORTED_MANIFEST_SCHEMA_VERSION`]；
//! 旧的四字段清单（`detector`/`recognizer`/`dictionary`/`classifier`）会被识别出来并给出
//! 迁移提示，而不是被当成“缺少某个字段”这种模糊错误。
//!
//! `files[].sha256` 不能为空：本地清单是**完整**描述，每个文件都必须携带已验证的哈希
//! （默认表反之可以有未哈希条目，但那会让集合不可能是 `complete`，
//! 见 [`crate::model_set`] 的模块文档）。

use std::{fmt::Display, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    error::{RapidOcrError, Result},
    model_registry::{DefaultModelSelection, ModelRegistry},
    model_set::{ModelFileSpec, ModelRole, ModelSet},
};

/// 模型目录里本地清单的固定文件名。
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

/// 本 crate 支持的清单 schema 版本。
pub const SUPPORTED_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// 清单里的单个文件。
///
/// 与 [`ModelFileSpec`] 的唯一结构差别是 `source_url` 为 `Option`：清单允许描述
/// “只能本地放置、没有下载来源”的文件，而 [`ModelFileSpec`] 用空串表达同一件事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    pub name: String,
    pub role: ModelRole,
    pub sha256: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub source_url: Option<String>,
}

/// 通用模型清单。
///
/// 不再是 `detector`/`recognizer`/`dictionary`/`classifier` 四个固定字段：那四个字段
/// 无法表达 tokenizer 与公式模型（公式识别、公式检测），因此改为
/// `files: Vec<ManifestFile>` + `role`。这是**破坏性修改**（crate 未发布，
/// 符合开发期规则）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    pub schema_version: u32,
    pub id: String,
    pub family: String,
    pub version: String,
    #[serde(default)]
    pub languages: Vec<String>,
    pub files: Vec<ManifestFile>,
}

impl ModelManifest {
    /// 解析清单 JSON；schema 相关错误都是可定位错误。
    pub fn from_json_str(json: &str) -> Result<Self> {
        parse_manifest_json(json).map_err(|reason| manifest_error(None, reason))
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|error| manifest_error(None, format!("is not valid UTF-8: {error}")))?;
        Self::from_json_str(text)
    }

    /// 从文件加载；错误信息带上清单路径。
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .map_err(|error| manifest_error(Some(path), format!("cannot be read: {error}")))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| manifest_error(Some(path), format!("is not valid UTF-8: {error}")))?;
        parse_manifest_json(text).map_err(|reason| manifest_error(Some(path), reason))
    }

    /// `<dir>/manifest.json` 存在时加载它，否则返回 `None`。
    ///
    /// 返回 `None` 只表示“没有本地清单”，不表示“清单坏了”：坏清单必须报错，
    /// 否则会静默回落到默认表，用户看到的行为与磁盘上的文件不一致。
    pub fn load_from_dir(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(MANIFEST_FILE_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        Self::load(&path).map(Some)
    }

    /// 转换成模型集合；文件名、哈希与重复项都在这里校验。
    pub fn to_model_set(&self) -> Result<ModelSet> {
        let mut files = Vec::with_capacity(self.files.len());
        for file in &self.files {
            if file.sha256.trim().is_empty() {
                return Err(manifest_error(
                    None,
                    format!(
                        "file `{}` has an empty `sha256`; a local manifest must carry a verified \
                         hash for every file",
                        file.name
                    ),
                ));
            }
            files.push(ModelFileSpec::new(
                file.name.clone(),
                file.role,
                file.size_bytes,
                file.sha256.clone(),
                file.source_url.clone().unwrap_or_default(),
            )?);
        }
        let set = ModelSet {
            id: self.id.clone(),
            family: self.family.clone(),
            version: self.version.clone(),
            files,
        };
        set.validate()?;
        Ok(set)
    }

    /// 逐文件校验：**薄包装**于 [`crate::model_set::validate_model_files`]。
    ///
    /// 遇到第一个非 `Present` 的文件即返回错误（保持原有“首个错误”语义），
    /// 但逐文件实现只有共享函数那一份。
    pub fn validate_files(&self, root: impl AsRef<Path>) -> Result<()> {
        let root = root.as_ref();
        let status = self.to_model_set()?.status(root);
        for (file, state) in &status.files {
            match state {
                crate::model_set::ModelFileState::Present => {}
                crate::model_set::ModelFileState::Missing => {
                    return Err(RapidOcrError::FileNotFound(root.join(&file.name)));
                }
                crate::model_set::ModelFileState::Corrupt { expected, actual } => {
                    return Err(RapidOcrError::HashMismatch {
                        path: root.join(&file.name),
                        expected: expected.clone(),
                        actual: actual.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// 选定来源的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSourceKind {
    DefaultTable,
    LocalManifest,
}

/// 一次模型请求：需要哪些管线，以及（默认表分支下）具体的文本管线选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModelRequest {
    pub text: Option<DefaultModelSelection>,
    pub formula: bool,
}

impl ModelRequest {
    pub fn text_only(text: DefaultModelSelection) -> Self {
        Self {
            text: Some(text),
            formula: false,
        }
    }

    pub fn text_and_formula(text: DefaultModelSelection) -> Self {
        Self {
            text: Some(text),
            formula: true,
        }
    }

    pub fn formula_only() -> Self {
        Self {
            text: None,
            formula: true,
        }
    }

    /// 文本管线**必需**的 role 集合：`detector` + `recognizer` + `dictionary`；
    /// 显式启用方向分类时再加 `classifier`。
    pub fn text_roles(&self) -> Vec<ModelRole> {
        let Some(selection) = &self.text else {
            return Vec::new();
        };
        let mut roles = vec![ModelRole::Detector];
        if selection.include_classifier {
            roles.push(ModelRole::Classifier);
        }
        roles.push(ModelRole::Recognizer);
        roles.push(ModelRole::Dictionary);
        roles
    }

    /// 公式管线**必需**的 role 集合，只有 `formula_recognizer`。
    ///
    /// 页面公式检测模型在 `FormulaPolicy` 里是**可选**的（不给 `detector_path` 时只处理
    /// 调用方显式声明的区域），把它算作必需会让“可选能力”变成“必须先下载 80 MB”。
    pub fn formula_roles(&self) -> Vec<ModelRole> {
        if self.formula {
            vec![ModelRole::FormulaRecognizer]
        } else {
            Vec::new()
        }
    }

    /// 该请求必需的全部 role（并集）。
    ///
    /// 本地清单描述的就是**一个**目录，因此必须由同一个集合满足这个并集；
    /// 默认表按管线产生多个集合，各自满足 [`Self::text_roles`] 或 [`Self::formula_roles`]。
    pub fn required_roles(&self) -> Vec<ModelRole> {
        let mut roles = self.text_roles();
        roles.extend(self.formula_roles());
        roles
    }
}

/// 某个模型目录选定的**唯一**来源。
#[derive(Debug, Clone)]
pub struct ModelSource {
    manifest: Option<ModelManifest>,
}

impl ModelSource {
    /// 应用单一来源规则：`<model_dir>/manifest.json` 存在则只用它，否则只用默认表。
    pub fn select(model_dir: &Path) -> Result<Self> {
        Ok(Self {
            manifest: ModelManifest::load_from_dir(model_dir)?,
        })
    }

    pub const fn kind(&self) -> ModelSourceKind {
        match self.manifest {
            Some(_) => ModelSourceKind::LocalManifest,
            None => ModelSourceKind::DefaultTable,
        }
    }

    pub fn manifest(&self) -> Option<&ModelManifest> {
        self.manifest.as_ref()
    }

    /// 该来源下满足请求所需的模型集。
    ///
    /// 本地清单描述的就是**这一个目录**，因此只产生一个集合（并由它承担全部必需 role
    /// 的并集）；默认表按管线产生文本集与公式集，各自满足自己那一组 role。
    /// 两种分支都调用同一个 [`ModelSet::require_roles`]，
    /// 因此“缺 role 就报错”只有一处实现。
    pub fn model_sets(&self, request: &ModelRequest) -> Result<Vec<ModelSet>> {
        match &self.manifest {
            Some(manifest) => {
                let set = manifest.to_model_set()?;
                set.require_roles(&request.required_roles())?;
                Ok(vec![set])
            }
            None => {
                let registry = ModelRegistry::from_default_yaml()?;
                let mut sets = Vec::new();
                if let Some(selection) = &request.text {
                    let set = registry.text_model_set(selection)?;
                    set.require_roles(&request.text_roles())?;
                    sets.push(set);
                }
                if request.formula {
                    let set = registry.formula_model_set()?;
                    set.require_roles(&request.formula_roles())?;
                    sets.push(set);
                }
                Ok(sets)
            }
        }
    }
}

/// 解析清单 JSON，返回**不含路径前缀**的可定位原因。
fn parse_manifest_json(json: &str) -> std::result::Result<ModelManifest, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("is not valid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "must be a JSON object".to_string())?;

    match object.get("schema_version") {
        None => {
            let legacy_keys = ["detector", "recognizer", "dictionary", "classifier"];
            if legacy_keys.iter().any(|key| object.contains_key(*key)) {
                return Err(format!(
                    "has no `schema_version` and uses the legacy four-field shape \
                     (`detector`/`recognizer`/`dictionary`/`classifier`), which is no longer \
                     supported; migrate to `schema_version: {SUPPORTED_MANIFEST_SCHEMA_VERSION}` \
                     with a `files` array of `{{ name, role, sha256, size_bytes, source_url }}`"
                ));
            }
            Err(format!(
                "has no `schema_version`; every manifest must declare \
                 `schema_version: {SUPPORTED_MANIFEST_SCHEMA_VERSION}`"
            ))
        }
        Some(declared) => {
            let declared = declared
                .as_u64()
                .ok_or_else(|| "`schema_version` must be an unsigned integer".to_string())?;
            if declared != u64::from(SUPPORTED_MANIFEST_SCHEMA_VERSION) {
                return Err(format!(
                    "declares unsupported `schema_version` {declared}; this build supports \
                     {SUPPORTED_MANIFEST_SCHEMA_VERSION}"
                ));
            }
            serde_json::from_value::<ModelManifest>(value).map_err(|error| {
                format!(
                    "does not match `schema_version` {SUPPORTED_MANIFEST_SCHEMA_VERSION}: {error}"
                )
            })
        }
    }
}

fn manifest_error(path: Option<&Path>, reason: impl Display) -> RapidOcrError {
    match path {
        Some(path) => {
            RapidOcrError::ModelResolve(format!("model manifest {}: {reason}", path.display()))
        }
        None => RapidOcrError::ModelResolve(format!("model manifest: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{LangDet, LangRec, ModelType, OcrVersion},
        model_registry::DefaultModelSelection,
        model_set::{ModelFileState, ModelRole},
        test_support::TempDir,
    };

    /// 旧的四字段清单（与 `assets/manifest.example.json` 的上一版形状一致）。
    const LEGACY_MANIFEST: &str = r#"{
      "id": "pp-ocrv6-medium-multilingual",
      "family": "PP-OCR",
      "version": "v6",
      "languages": ["ch", "en"],
      "detector": {
        "file_name": "PP-OCRv6_det_medium.onnx",
        "sha256": "92078b7355007ccfffcd4c8cd441a3afd4538904d06881b29a155e1e679907c2",
        "source_url": "https://www.modelscope.cn/models/RapidAI/RapidOCR"
      },
      "recognizer": {
        "file_name": "PP-OCRv6_rec_medium.onnx",
        "sha256": "eef444829dbbe18d7fea59a3f6eb75647518d2b3a9568d27c92e42940204894b",
        "source_url": "https://www.modelscope.cn/models/RapidAI/RapidOCR"
      },
      "dictionary": {
        "file_name": "ppocrv6_dict.txt",
        "sha256": "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
        "source_url": "https://www.modelscope.cn/models/RapidAI/RapidOCR"
      },
      "classifier": null
    }"#;

    fn current_manifest() -> String {
        r#"{
          "schema_version": 1,
          "id": "pp-ocrv6-medium",
          "family": "PP-OCR",
          "version": "PP-OCRv6",
          "languages": ["ch"],
          "files": [
            { "name": "PP-OCRv6_det_medium.onnx", "role": "detector",
              "sha256": "92078b7355007ccfffcd4c8cd441a3afd4538904d06881b29a155e1e679907c2",
              "size_bytes": 62119454,
              "source_url": "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.1/onnx/PP-OCRv6/det/PP-OCRv6_det_medium.onnx" },
            { "name": "PP-OCRv6_rec_medium.onnx", "role": "recognizer",
              "sha256": "eef444829dbbe18d7fea59a3f6eb75647518d2b3a9568d27c92e42940204894b",
              "size_bytes": 76629984,
              "source_url": "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.1/onnx/PP-OCRv6/rec/PP-OCRv6_rec_medium.onnx" },
            { "name": "ppocrv6_dict.txt", "role": "dictionary",
              "sha256": "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d",
              "size_bytes": 74947 }
          ]
        }"#
        .to_string()
    }

    fn text_selection() -> DefaultModelSelection {
        DefaultModelSelection {
            ocr_version: OcrVersion::PPocrV6,
            det_lang: LangDet::Ch,
            rec_lang: LangRec::Ch,
            model_type: ModelType::Medium,
            ..DefaultModelSelection::default()
        }
    }

    #[test]
    fn legacy_four_field_manifest_is_rejected_with_a_migration_hint() {
        let error = ModelManifest::from_json_str(LEGACY_MANIFEST)
            .expect_err("the legacy four-field shape must be rejected");
        let message = error.to_string();
        assert!(message.contains("schema_version"), "{message}");
        assert!(message.contains("legacy"), "{message}");
        assert!(message.contains("files"), "{message}");
    }

    #[test]
    fn an_unknown_schema_version_is_rejected() {
        let json = current_manifest().replace("\"schema_version\": 1", "\"schema_version\": 2");
        let error = ModelManifest::from_json_str(&json)
            .expect_err("an unknown schema version must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("unsupported `schema_version` 2"),
            "{message}"
        );
        assert!(message.contains("supports 1"), "{message}");

        // 非整数版本同样必须是可定位错误，而不是被当成未知版本静默放行。
        let json = current_manifest().replace("\"schema_version\": 1", "\"schema_version\": \"1\"");
        let error =
            ModelManifest::from_json_str(&json).expect_err("a non-integer version must fail");
        assert!(error.to_string().contains("unsigned integer"), "{error}");

        // 缺失 schema_version 但也不是旧四字段形状：报缺失，而不是报旧格式。
        let error = ModelManifest::from_json_str(r#"{ "id": "x", "family": "f", "version": "v" }"#)
            .expect_err("a manifest without schema_version must fail");
        assert!(
            error.to_string().contains("has no `schema_version`"),
            "{error}"
        );
        assert!(!error.to_string().contains("legacy"), "{error}");
    }

    #[test]
    fn the_current_schema_converts_to_a_model_set() {
        let manifest = ModelManifest::from_json_str(&current_manifest())
            .expect("the current schema must parse");
        assert_eq!(manifest.schema_version, SUPPORTED_MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.languages, vec!["ch"]);

        let set = manifest.to_model_set().expect("conversion must succeed");
        assert_eq!(set.id, "pp-ocrv6-medium");
        assert_eq!(
            set.declared_roles(),
            vec![
                ModelRole::Detector,
                ModelRole::Recognizer,
                ModelRole::Dictionary
            ]
        );
        // 没有 source_url 的文件用空串表示“没有可信下载来源”，且仍然有哈希。
        let dictionary = set
            .files
            .iter()
            .find(|file| file.role == ModelRole::Dictionary)
            .expect("dictionary must be present");
        assert!(!dictionary.has_source_url());
        assert!(dictionary.has_hash());

        // 空目录下三个文件都缺失，集合不齐备；`validate_files` 报第一个缺失文件。
        let dir = TempDir::new("manifest-validate-missing");
        let status = set.status(dir.path());
        assert!(
            status
                .files
                .iter()
                .all(|(_, state)| *state == ModelFileState::Missing)
        );
        assert!(!status.complete);
        assert_eq!(
            status.download_bytes_total,
            Some(62_119_454 + 76_629_984 + 74_947)
        );
        let error = manifest
            .validate_files(dir.path())
            .expect_err("missing files must fail");
        let message = error.to_string();
        assert!(message.contains("file not found"), "{message}");
        assert!(
            message.contains("PP-OCRv6_det_medium.onnx"),
            "the error must locate the first missing file: {message}"
        );

        // 哈希不匹配：报 `HashMismatch`，且同样定位到具体文件。
        dir.write("PP-OCRv6_det_medium.onnx", b"not the real model");
        let error = manifest
            .validate_files(dir.path())
            .expect_err("a hash mismatch must fail");
        match error {
            RapidOcrError::HashMismatch {
                path,
                expected,
                actual,
            } => {
                assert!(path.ends_with("PP-OCRv6_det_medium.onnx"));
                assert_eq!(
                    expected,
                    "92078b7355007ccfffcd4c8cd441a3afd4538904d06881b29a155e1e679907c2"
                );
                assert_ne!(actual, expected);
            }
            other => panic!("expected HashMismatch, got {other}"),
        }
    }

    #[test]
    fn a_manifest_file_must_carry_a_hash_and_a_bare_name() {
        let no_hash = current_manifest().replace(
            "92078b7355007ccfffcd4c8cd441a3afd4538904d06881b29a155e1e679907c2",
            "",
        );
        let manifest = ModelManifest::from_json_str(&no_hash).expect("json must still parse");
        let error = manifest
            .to_model_set()
            .expect_err("an empty hash must be rejected");
        assert!(error.to_string().contains("empty `sha256`"), "{error}");

        let escaped = current_manifest()
            .replace("PP-OCRv6_det_medium.onnx", "../../PP-OCRv6_det_medium.onnx");
        let manifest = ModelManifest::from_json_str(&escaped).expect("json must still parse");
        let error = manifest
            .to_model_set()
            .expect_err("a path-escaping name must be rejected");
        assert!(
            error.to_string().contains("bare relative file name"),
            "{error}"
        );
    }

    #[test]
    fn unknown_manifest_fields_are_rejected() {
        let json = current_manifest().replace("\"files\":", "\"classifier\": null,\n\"files\":");
        let error =
            ModelManifest::from_json_str(&json).expect_err("unknown fields must not be ignored");
        assert!(error.to_string().contains("classifier"), "{error}");
    }

    /// 随仓库提交的示例清单必须能被当前的加载器接受。
    ///
    /// 示例文件与本 crate 的加载器是同一个契约的两份拷贝；示例过时会让使用者
    /// 复制出一个必然加载失败的清单，因此这里把它当作契约的一部分来断言。
    #[test]
    fn the_shipped_manifest_example_matches_the_current_schema() {
        let manifest =
            ModelManifest::from_json_str(include_str!("../assets/manifest.example.json"))
                .expect("the shipped manifest example must load");
        assert_eq!(manifest.schema_version, SUPPORTED_MANIFEST_SCHEMA_VERSION);
        let set = manifest
            .to_model_set()
            .expect("the shipped manifest example must convert");
        assert!(set.has_role(ModelRole::Detector));
        assert!(set.has_role(ModelRole::Recognizer));
        assert!(set.has_role(ModelRole::Dictionary));
        for file in &set.files {
            assert!(file.has_hash(), "{file:?}");
            assert!(file.has_source_url(), "{file:?}");
        }
    }

    #[test]
    fn the_selected_source_is_the_local_manifest_when_present() {
        let empty = TempDir::new("source-empty");
        let source = ModelSource::select(empty.path()).expect("selection must succeed");
        assert_eq!(source.kind(), ModelSourceKind::DefaultTable);
        assert!(source.manifest().is_none());

        let with_manifest = TempDir::new("source-manifest");
        with_manifest.write(MANIFEST_FILE_NAME, current_manifest().as_bytes());
        let source = ModelSource::select(with_manifest.path()).expect("selection must succeed");
        assert_eq!(source.kind(), ModelSourceKind::LocalManifest);
        assert_eq!(
            source.manifest().map(|manifest| manifest.id.as_str()),
            Some("pp-ocrv6-medium")
        );
    }

    /// 坏清单必须报错，**不得**静默回落到默认表。
    #[test]
    fn a_broken_manifest_is_an_error_not_a_fallback_to_the_default_table() {
        let dir = TempDir::new("source-broken");
        dir.write(MANIFEST_FILE_NAME, b"{ not json");
        let error = ModelSource::select(dir.path()).expect_err("a broken manifest must fail");
        assert!(error.to_string().contains(MANIFEST_FILE_NAME), "{error}");

        dir.write(MANIFEST_FILE_NAME, LEGACY_MANIFEST.as_bytes());
        let error =
            ModelSource::select(dir.path()).expect_err("a legacy manifest must fail, not degrade");
        assert!(error.to_string().contains("legacy"), "{error}");
    }

    /// 单一来源：本地清单存在时默认表**完全不参与**，即使清单只描述了公式模型。
    #[test]
    fn a_local_manifest_is_never_merged_with_the_default_table() {
        let dir = TempDir::new("source-no-merge");
        let manifest = r#"{
          "schema_version": 1,
          "id": "formula-only",
          "family": "RapidDoc",
          "version": "v1.0.0",
          "files": [
            { "name": "pp_formulanet_plus_m.onnx", "role": "formula_recognizer",
              "sha256": "71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b",
              "size_bytes": 593915961 }
          ]
        }"#;
        dir.write(MANIFEST_FILE_NAME, manifest.as_bytes());
        let source = ModelSource::select(dir.path()).expect("selection must succeed");

        let sets = source
            .model_sets(&ModelRequest::formula_only())
            .expect("the formula role is declared by the manifest");
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].id, "formula-only");
        assert_eq!(sets[0].files.len(), 1);

        // 文本管线所需 role 不在清单里 → 报错并列出**全部**缺失 role，
        // 而不是从默认表补一个文本集合（那就是双权威）。
        let error = source
            .model_sets(&ModelRequest::text_only(text_selection()))
            .expect_err("the manifest cannot satisfy the text pipeline");
        let message = error.to_string();
        assert!(message.contains("detector"), "{message}");
        assert!(message.contains("recognizer"), "{message}");
        assert!(message.contains("dictionary"), "{message}");
    }

    #[test]
    fn the_default_table_source_builds_the_text_and_formula_sets() {
        let dir = TempDir::new("source-default");
        let source = ModelSource::select(dir.path()).expect("selection must succeed");

        let sets = source
            .model_sets(&ModelRequest::text_and_formula(text_selection()))
            .expect("the default table satisfies both pipelines");
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].id, "PP-OCRv6-medium-ch");
        assert_eq!(sets[1].id, "PP-FormulaNet_plus-M");

        let statuses: Vec<_> = sets.iter().map(|set| set.status(dir.path())).collect();
        for status in &statuses {
            assert!(!status.complete, "no file exists in an empty model dir");
            assert!(
                status.files.iter().all(|(file, _)| file.has_hash()),
                "every default-table file must carry a hash: {status:?}"
            );
        }
        // 公式集合只有一个 594 MB 的文件，其大小已知。
        assert_eq!(statuses[1].download_bytes_total, Some(593_915_961));
        // v6 的模型与字典在默认表里都记录了大小，因此文本集合的总量可核算。
        assert_eq!(statuses[0].download_bytes_total, Some(138_824_385));

        // v4 的模型大小在默认表里未知 → 总量是 `None`，而不是“已知部分之和”。
        let registry = ModelRegistry::from_default_yaml().expect("registry should parse");
        let v4 = registry
            .text_model_set(&DefaultModelSelection {
                ocr_version: OcrVersion::PPocrV4,
                model_type: ModelType::Mobile,
                ..text_selection()
            })
            .expect("v4 text model set should build");
        assert_eq!(v4.status(dir.path()).download_bytes_total, None);

        // 请求文本管线但默认表里 rec 的字典缺失时才可能缺 role；v6 有字典，
        // 因此这里必须成功。
        source
            .model_sets(&ModelRequest::text_only(text_selection()))
            .expect("the text roles must all be declared");
    }

    /// **真实模型目录**上的端到端校验：默认表里的哈希与逐文件状态逻辑对真实文件成立。
    ///
    /// 缺 `RAPID_OCR_MODEL_ROOT`（或该目录下没有 `medium/`）时显式 skip，
    /// 与套件里其他真实资产测试一致。
    #[test]
    fn the_default_table_set_is_complete_against_real_v6_medium_models() {
        let Some(root) = crate::test_support::ocr_model_root() else {
            return;
        };
        let dir = root.join("medium");
        if !dir.is_dir() {
            eprintln!(
                "skipping test: {} does not contain the v6 medium models",
                dir.display()
            );
            return;
        }

        let registry = ModelRegistry::from_default_yaml().expect("registry should parse");
        let set = registry
            .text_model_set(&DefaultModelSelection {
                ocr_version: OcrVersion::PPocrV6,
                model_type: ModelType::Medium,
                ..text_selection()
            })
            .expect("the v6 medium text model set should build");
        let status = set.status(&dir);

        let names: Vec<&str> = status
            .files
            .iter()
            .map(|(file, _)| file.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "PP-OCRv6_det_medium.onnx",
                "PP-OCRv6_rec_medium.onnx",
                "ppocrv6_dict.txt"
            ],
            "the set names must be the on-disk names derived from the URLs"
        );
        assert!(
            status
                .files
                .iter()
                .all(|(_, state)| *state == ModelFileState::Present),
            "every real v6 medium file must hash-match the default table: {status:?}"
        );
        assert!(status.complete);
        assert_eq!(status.download_bytes_total, Some(0));
    }
}
