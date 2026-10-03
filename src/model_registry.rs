use std::collections::HashMap;

use serde::Deserialize;

use crate::{
    config::{LangCls, LangDet, LangRec, ModelType, OcrVersion},
    error::{RapidOcrError, Result},
    model_set::{ModelFileSpec, ModelRole, ModelSet},
};

const DEFAULT_MODELS_YAML: &str = include_str!("../assets/default_models.yaml");

#[derive(Debug, Clone, Deserialize)]
struct Root {
    onnxruntime: HashMap<String, OcrVersionNode>,
    /// 公式模型：**固定集合**，与 `ocr_version`/语言选择无关，因此不是
    /// `onnxruntime` 选择树的一部分（那棵树按版本+语言+模型规格选模型）。
    #[serde(default)]
    formula: Option<FormulaNode>,
}

#[derive(Debug, Clone, Deserialize)]
struct OcrVersionNode {
    #[serde(default)]
    det: HashMap<String, ModelEntry>,
    #[serde(default)]
    cls: HashMap<String, ModelEntry>,
    #[serde(default)]
    rec: HashMap<String, ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelEntry {
    model_dir: String,
    #[serde(rename = "SHA256")]
    sha256: Option<String>,
    size_bytes: Option<u64>,
    /// 识别字典是**一等文件**（自带 URL 与 SHA-256），不再是裸 `dict_url`：
    /// 字典与权重一样必须做哈希校验。
    dict: Option<DictEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct DictEntry {
    model_dir: String,
    #[serde(rename = "SHA256")]
    sha256: Option<String>,
    size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct FormulaNode {
    id: String,
    family: String,
    version: String,
    files: Vec<FormulaFileNode>,
}

#[derive(Debug, Clone, Deserialize)]
struct FormulaFileNode {
    name: String,
    role: ModelRole,
    model_dir: String,
    #[serde(rename = "SHA256")]
    sha256: Option<String>,
    size_bytes: Option<u64>,
}

/// 默认表里的一次文本管线选择：决定 `ModelSet` 由哪些文件组成。
///
/// `include_classifier` 是**必须显式表达**的：方向分类模型在引擎里默认关闭
/// （`GlobalConfig::use_cls = false`），把它无条件算进集合会让“没有下载 cls”
/// 变成“模型不齐备”，从而挡住本来就跑得通的管线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DefaultModelSelection {
    pub ocr_version: OcrVersion,
    pub det_lang: LangDet,
    pub rec_lang: LangRec,
    pub cls_lang: LangCls,
    pub model_type: ModelType,
    pub include_classifier: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelVariant {
    Server,
    Mobile,
}

#[derive(Debug, Clone, Copy)]
struct ModelCandidate<'a> {
    name: &'a String,
    entry: &'a ModelEntry,
    variant: ModelVariant,
}

/// 默认表里解析出来的单个可下载文件。
#[derive(Debug, Clone)]
pub struct ResolvedModelFile {
    pub url: String,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRecModel {
    #[allow(dead_code)]
    pub model_name: String,
    pub model_url: String,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub dictionary: Option<ResolvedModelFile>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTaskModel {
    #[allow(dead_code)]
    pub model_name: String,
    pub model_url: String,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    root: Root,
}

impl ModelRegistry {
    pub fn from_default_yaml() -> Result<Self> {
        Self::from_yaml_str(DEFAULT_MODELS_YAML)
    }

    pub fn from_yaml_str(yaml: &str) -> Result<Self> {
        let root = serde_yaml::from_str::<Root>(yaml)?;
        Ok(Self { root })
    }

    pub fn resolve_rec(
        &self,
        ocr_version: OcrVersion,
        lang: LangRec,
        model_type: ModelType,
    ) -> Result<ResolvedRecModel> {
        let version_map = self
            .root
            .onnxruntime
            .get(ocr_version.as_str())
            .ok_or_else(|| {
                RapidOcrError::ModelResolve(format!(
                    "unsupported ocr version for onnxruntime: {}",
                    ocr_version.as_str()
                ))
            })?;

        let lang_prefix = lang.as_str();
        let selected = select_model(
            &version_map.rec,
            lang_prefix,
            model_type,
            "rec",
            ocr_version,
        )?;

        Ok(ResolvedRecModel {
            model_name: selected.0.clone(),
            model_url: selected.1.model_dir.clone(),
            sha256: selected.1.sha256.clone(),
            size_bytes: selected.1.size_bytes,
            dictionary: selected.1.dict.as_ref().map(|dict| ResolvedModelFile {
                url: dict.model_dir.clone(),
                sha256: dict.sha256.clone(),
                size_bytes: dict.size_bytes,
            }),
        })
    }

    pub fn resolve_det(
        &self,
        ocr_version: OcrVersion,
        lang: LangDet,
        model_type: ModelType,
    ) -> Result<ResolvedTaskModel> {
        let version_map = self.version_node(ocr_version)?;
        let selected = select_model(
            &version_map.det,
            lang.as_str(),
            model_type,
            "det",
            ocr_version,
        )?;
        Ok(ResolvedTaskModel {
            model_name: selected.0.clone(),
            model_url: selected.1.model_dir.clone(),
            sha256: selected.1.sha256.clone(),
            size_bytes: selected.1.size_bytes,
        })
    }

    pub fn resolve_cls(
        &self,
        ocr_version: OcrVersion,
        lang: LangCls,
        model_type: ModelType,
    ) -> Result<ResolvedTaskModel> {
        let version_map = self.version_node(ocr_version)?;
        let selected = select_model(
            &version_map.cls,
            lang.as_str(),
            model_type,
            "cls",
            ocr_version,
        )?;
        Ok(ResolvedTaskModel {
            model_name: selected.0.clone(),
            model_url: selected.1.model_dir.clone(),
            sha256: selected.1.sha256.clone(),
            size_bytes: selected.1.size_bytes,
        })
    }

    fn version_node(&self, ocr_version: OcrVersion) -> Result<&OcrVersionNode> {
        self.root
            .onnxruntime
            .get(ocr_version.as_str())
            .ok_or_else(|| {
                RapidOcrError::ModelResolve(format!(
                    "unsupported ocr version for onnxruntime: {}",
                    ocr_version.as_str()
                ))
            })
    }

    /// 默认表里的文本管线模型集：detector（+ 可选 classifier）+ recognizer + 字典。
    ///
    /// 文件名**从 URL 推导**（唯一实现是 `model_store::extract_file_name`），因为
    /// 默认表的键不是文件名（例如键 `multi_PP-OCRv6_det_small` 对应
    /// `PP-OCRv6_det_small.onnx`），而 `ensure_downloaded` 落盘时用的正是 URL 的
    /// 最后一段；两处推导一致才能让状态校验命中同一个文件。
    ///
    /// 字典缺失时**不在这里报错**：集合只是缺少 `Dictionary` role，由
    /// `ModelSet::require_roles` 统一报“缺哪些 role”，避免两处各有一套缺失判定。
    pub fn text_model_set(&self, selection: &DefaultModelSelection) -> Result<ModelSet> {
        let det = self.resolve_det(
            selection.ocr_version,
            selection.det_lang,
            selection.model_type,
        )?;
        let rec = self.resolve_rec(
            selection.ocr_version,
            selection.rec_lang,
            selection.model_type,
        )?;

        let mut files = vec![task_file(
            &det.model_url,
            ModelRole::Detector,
            det.sha256,
            det.size_bytes,
        )?];
        if selection.include_classifier {
            let cls = self.resolve_cls(
                selection.ocr_version,
                selection.cls_lang,
                selection.model_type,
            )?;
            files.push(task_file(
                &cls.model_url,
                ModelRole::Classifier,
                cls.sha256,
                cls.size_bytes,
            )?);
        }
        files.push(task_file(
            &rec.model_url,
            ModelRole::Recognizer,
            rec.sha256,
            rec.size_bytes,
        )?);
        if let Some(dictionary) = &rec.dictionary {
            files.push(task_file(
                &dictionary.url,
                ModelRole::Dictionary,
                dictionary.sha256.clone(),
                dictionary.size_bytes,
            )?);
        }

        let set = ModelSet {
            id: format!(
                "{}-{}-{}",
                selection.ocr_version.as_str(),
                selection.model_type.as_str(),
                selection.rec_lang.as_str()
            ),
            family: "PP-OCR".to_string(),
            version: selection.ocr_version.as_str().to_string(),
            files,
        };
        set.validate()?;
        Ok(set)
    }

    /// 默认表里的公式模型集。公式模型与 `ocr_version`/语言无关，因此直接取自
    /// `formula:` 段（该段缺失即报错，不静默返回空集合）。
    pub fn formula_model_set(&self) -> Result<ModelSet> {
        let node = self.root.formula.as_ref().ok_or_else(|| {
            RapidOcrError::ModelResolve(
                "the default model table has no `formula:` section, so formula roles cannot be \
                 resolved from it"
                    .to_string(),
            )
        })?;
        let files = node
            .files
            .iter()
            .map(|file| {
                ModelFileSpec::new(
                    file.name.clone(),
                    file.role,
                    file.size_bytes,
                    file.sha256.clone().unwrap_or_default(),
                    file.model_dir.clone(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let set = ModelSet {
            id: node.id.clone(),
            family: node.family.clone(),
            version: node.version.clone(),
            files,
        };
        set.validate()?;
        Ok(set)
    }
}

/// 由 URL 构造一个 `ModelFileSpec`（文件名取 URL 最后一段）。
fn task_file(
    url: &str,
    role: ModelRole,
    sha256: Option<String>,
    size_bytes: Option<u64>,
) -> Result<ModelFileSpec> {
    let name = crate::model_store::extract_file_name(url)?;
    ModelFileSpec::new(name, role, size_bytes, sha256.unwrap_or_default(), url)
}

fn select_model<'a>(
    model_map: &'a HashMap<String, ModelEntry>,
    lang_prefix: &str,
    model_type: ModelType,
    task: &str,
    ocr_version: OcrVersion,
) -> Result<(&'a String, &'a ModelEntry)> {
    if ocr_version == OcrVersion::PPocrV6 {
        return select_ppocr_v6_model(model_map, lang_prefix, model_type, task);
    }

    let mut candidates: Vec<ModelCandidate<'a>> = model_map
        .iter()
        .filter(|(name, _)| language_tag_matches(name, lang_prefix))
        .map(|(name, entry)| ModelCandidate {
            name,
            entry,
            variant: classify_model_variant(name),
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(b.name));

    if candidates.is_empty() {
        return Err(RapidOcrError::ModelResolve(format!(
            "no {task} model found for lang={lang_prefix}, version={}",
            ocr_version.as_str()
        )));
    }

    let selected = match model_type {
        ModelType::Server => select_unique_variant(
            &candidates,
            ModelVariant::Server,
            task,
            lang_prefix,
            ocr_version,
        )?,
        ModelType::Mobile => select_unique_variant(
            &candidates,
            ModelVariant::Mobile,
            task,
            lang_prefix,
            ocr_version,
        )?,
        ModelType::Tiny | ModelType::Small | ModelType::Medium => {
            return Err(RapidOcrError::ModelResolve(format!(
                "{} {task} does not provide `{}` models; use `mobile` or `server`",
                ocr_version.as_str(),
                model_type.as_str()
            )));
        }
    };
    Ok((selected.name, selected.entry))
}

fn select_ppocr_v6_model<'a>(
    model_map: &'a HashMap<String, ModelEntry>,
    lang_prefix: &str,
    model_type: ModelType,
    task: &str,
) -> Result<(&'a String, &'a ModelEntry)> {
    validate_ppocr_v6_task(task)?;
    validate_ppocr_v6_model_type(model_type, task)?;
    validate_ppocr_v6_lang(lang_prefix, model_type, task)?;

    let model_key = format!("multi_PP-OCRv6_{task}_{}", model_type.as_str());
    model_map.get_key_value(&model_key).ok_or_else(|| {
        RapidOcrError::ModelResolve(format!(
            "missing PP-OCRv6 {task} model registry entry `{model_key}`"
        ))
    })
}

fn validate_ppocr_v6_task(task: &str) -> Result<()> {
    if matches!(task, "det" | "rec") {
        return Ok(());
    }

    Err(RapidOcrError::ModelResolve(format!(
        "PP-OCRv6 {task} models are not available in the default onnxruntime registry"
    )))
}

fn validate_ppocr_v6_model_type(model_type: ModelType, task: &str) -> Result<()> {
    if matches!(
        model_type,
        ModelType::Tiny | ModelType::Small | ModelType::Medium
    ) {
        return Ok(());
    }

    Err(RapidOcrError::ModelResolve(format!(
        "PP-OCRv6 {task} does not provide `{}` models; use `tiny`, `small`, or `medium`",
        model_type.as_str()
    )))
}

fn validate_ppocr_v6_lang(lang_prefix: &str, model_type: ModelType, task: &str) -> Result<()> {
    let supported = (task == "det" && lang_prefix == "multi")
        || match model_type {
            ModelType::Tiny => matches!(lang_prefix, "ch" | "chinese_cht" | "en"),
            ModelType::Small | ModelType::Medium => {
                matches!(lang_prefix, "ch" | "chinese_cht" | "en" | "japan")
            }
            ModelType::Mobile | ModelType::Server => false,
        };

    if supported {
        return Ok(());
    }

    Err(RapidOcrError::ModelResolve(format!(
        "unsupported PP-OCRv6 {task} language `{lang_prefix}` for `{}` model",
        model_type.as_str()
    )))
}

fn select_unique_variant<'a>(
    candidates: &[ModelCandidate<'a>],
    variant: ModelVariant,
    task: &str,
    lang_prefix: &str,
    ocr_version: OcrVersion,
) -> Result<ModelCandidate<'a>> {
    let matched: Vec<ModelCandidate<'a>> = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.variant == variant)
        .collect();
    if matched.is_empty() {
        return Err(RapidOcrError::ModelResolve(format!(
            "no {variant:?} {task} model found for lang={lang_prefix}, version={}",
            ocr_version.as_str()
        )));
    }
    if matched.len() > 1 {
        return Err(RapidOcrError::ModelResolve(format!(
            "ambiguous {variant:?} {task} models for lang={lang_prefix}, version={}: {}",
            ocr_version.as_str(),
            format_candidate_names(&matched)
        )));
    }
    Ok(matched[0])
}

fn language_tag_matches(model_name: &str, lang_tag: &str) -> bool {
    extract_language_tag(model_name).is_some_and(|candidate| candidate == lang_tag)
}

fn extract_language_tag(model_name: &str) -> Option<&str> {
    let markers = ["_PP-", "_PP", "_ppocr_", "_ppocr"];
    let idx = markers
        .iter()
        .filter_map(|marker| model_name.find(marker))
        .min()?;
    if idx == 0 {
        return None;
    }
    Some(&model_name[..idx])
}

fn classify_model_variant(model_name: &str) -> ModelVariant {
    let lower = model_name.to_ascii_lowercase();
    if lower.contains("_server_") || lower.contains("_server.") {
        return ModelVariant::Server;
    }
    ModelVariant::Mobile
}

fn format_candidate_names(candidates: &[ModelCandidate<'_>]) -> String {
    let mut names: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.name.as_str())
        .collect();
    names.sort_unstable();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::ModelRegistry;
    use crate::config::{LangCls, LangDet, LangRec, ModelType, OcrVersion};

    const CUSTOM_YAML: &str = r#"
onnxruntime:
  PP-OCRv4:
    rec:
      ch_PP-OCRv4_rec_infer.onnx:
        model_dir: https://example.com/ch-mobile.onnx
      ch_doc_PP-OCRv4_rec_server_infer.onnx:
        model_dir: https://example.com/ch-doc-server.onnx
      ch_PP-OCRv4_rec_server_infer.onnx:
        model_dir: https://example.com/ch-server.onnx
"#;

    const AMBIGUOUS_MOBILE_YAML: &str = r#"
onnxruntime:
  PP-OCRv4:
    rec:
      en_PP-OCRv4_rec_infer.onnx:
        model_dir: https://example.com/en-mobile-a.onnx
      en_PP-OCRv4_rec_infer_v2.onnx:
        model_dir: https://example.com/en-mobile-b.onnx
"#;

    #[test]
    fn resolve_server_and_mobile() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");

        let mobile = reg
            .resolve_rec(OcrVersion::PPocrV4, LangRec::Ch, ModelType::Mobile)
            .expect("mobile model should resolve");
        assert!(mobile.model_name.contains("ch_PP-OCRv4_rec_infer"));
        assert!(!mobile.model_name.contains("server"));

        let server = reg
            .resolve_rec(OcrVersion::PPocrV4, LangRec::Ch, ModelType::Server)
            .expect("server model should resolve");
        assert!(server.model_name.contains("server"));
    }

    #[test]
    fn resolve_det_and_cls() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");

        let det = reg
            .resolve_det(OcrVersion::PPocrV4, LangDet::Ch, ModelType::Mobile)
            .expect("det model should resolve");
        assert!(det.model_name.contains("det"));

        let cls = reg
            .resolve_cls(OcrVersion::PPocrV4, LangCls::Ch, ModelType::Mobile)
            .expect("cls model should resolve");
        assert!(cls.model_name.contains("cls"));
    }

    #[test]
    fn resolve_ppocr_v6_size_models() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");

        let det = reg
            .resolve_det(OcrVersion::PPocrV6, LangDet::Ch, ModelType::Small)
            .expect("v6 det model should resolve");
        assert_eq!(det.model_name, "multi_PP-OCRv6_det_small");
        assert!(det.model_url.ends_with("PP-OCRv6_det_small.onnx"));

        let rec = reg
            .resolve_rec(OcrVersion::PPocrV6, LangRec::Ch, ModelType::Small)
            .expect("v6 rec model should resolve");
        assert_eq!(rec.model_name, "multi_PP-OCRv6_rec_small");
        assert!(rec.model_url.ends_with("PP-OCRv6_rec_small.onnx"));
        // 字典现在是一等文件（自带 URL 与 SHA-256），不再是裸 `dict_url`。
        let dictionary = rec.dictionary.expect("v6 rec must resolve a dictionary");
        assert!(dictionary.url.ends_with("ppocrv6_dict.txt"));
        assert!(
            dictionary
                .sha256
                .as_deref()
                .is_some_and(|hash| hash.len() == 64),
            "the dictionary must carry a real SHA-256: {dictionary:?}"
        );
        assert_eq!(dictionary.size_bytes, Some(74_947));
    }

    #[test]
    fn resolve_ppocr_v6_rejects_legacy_model_type() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");
        let err = reg
            .resolve_rec(OcrVersion::PPocrV6, LangRec::Ch, ModelType::Mobile)
            .expect_err("v6 should require size-based model type");
        assert!(err.to_string().contains("does not provide `mobile`"));
    }

    #[test]
    fn resolve_ppocr_v6_tiny_rejects_japan_lang() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");
        let err = reg
            .resolve_rec(OcrVersion::PPocrV6, LangRec::Japan, ModelType::Tiny)
            .expect_err("v6 tiny should reject unsupported lang");
        assert!(
            err.to_string()
                .contains("unsupported PP-OCRv6 rec language `japan`")
        );
    }

    #[test]
    fn resolve_lang_prefix_match_is_exact() {
        let reg = ModelRegistry::from_yaml_str(CUSTOM_YAML).expect("registry should parse");
        let mobile = reg
            .resolve_rec(OcrVersion::PPocrV4, LangRec::Ch, ModelType::Mobile)
            .expect("mobile model should resolve");
        assert!(mobile.model_name.starts_with("ch_PP-OCRv4_rec_infer"));
    }

    #[test]
    fn resolve_server_requires_explicit_server_variant() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");
        let err = reg
            .resolve_rec(OcrVersion::PPocrV4, LangRec::En, ModelType::Server)
            .expect_err("server should require explicit server variant");
        assert!(err.to_string().contains("no Server rec model found"));
    }

    #[test]
    fn resolve_mobile_rejects_ambiguous_mobile_models() {
        let reg =
            ModelRegistry::from_yaml_str(AMBIGUOUS_MOBILE_YAML).expect("registry should parse");
        let err = reg
            .resolve_rec(OcrVersion::PPocrV4, LangRec::En, ModelType::Mobile)
            .expect_err("ambiguous mobile models should fail");
        assert!(err.to_string().contains("ambiguous Mobile rec models"));
    }

    fn selection(ocr_version: OcrVersion, model_type: ModelType) -> super::DefaultModelSelection {
        super::DefaultModelSelection {
            ocr_version,
            model_type,
            ..super::DefaultModelSelection::default()
        }
    }

    #[test]
    fn text_model_set_covers_detector_recognizer_and_dictionary() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");
        let set = reg
            .text_model_set(&selection(OcrVersion::PPocrV6, ModelType::Small))
            .expect("v6 text model set should build");

        assert_eq!(set.id, "PP-OCRv6-small-ch");
        assert_eq!(set.family, "PP-OCR");
        assert_eq!(set.version, "PP-OCRv6");
        assert_eq!(
            set.declared_roles(),
            vec![
                crate::model_set::ModelRole::Detector,
                crate::model_set::ModelRole::Recognizer,
                crate::model_set::ModelRole::Dictionary,
            ],
            "the default classifier is off, so it must not be part of the set"
        );
        // 文件名来自 URL 的最后一段，而不是表里的键（键是 `multi_PP-OCRv6_*`）。
        let names: Vec<&str> = set.files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "PP-OCRv6_det_small.onnx",
                "PP-OCRv6_rec_small.onnx",
                "ppocrv6_dict.txt"
            ]
        );
        for file in &set.files {
            assert!(
                file.has_hash(),
                "every default-table file must carry a hash: {file:?}"
            );
        }
        set.require_roles(&[
            crate::model_set::ModelRole::Detector,
            crate::model_set::ModelRole::Recognizer,
            crate::model_set::ModelRole::Dictionary,
        ])
        .expect("the text pipeline roles must all be declared");
    }

    #[test]
    fn text_model_set_includes_the_classifier_only_when_requested() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");

        let without = reg
            .text_model_set(&selection(OcrVersion::PPocrV4, ModelType::Mobile))
            .expect("v4 text model set should build");
        assert!(!without.has_role(crate::model_set::ModelRole::Classifier));

        let with = reg
            .text_model_set(&super::DefaultModelSelection {
                include_classifier: true,
                ..selection(OcrVersion::PPocrV4, ModelType::Mobile)
            })
            .expect("v4 text model set with classifier should build");
        assert!(with.has_role(crate::model_set::ModelRole::Classifier));
        assert_eq!(
            with.files
                .iter()
                .find(|file| file.role == crate::model_set::ModelRole::Classifier)
                .map(|file| file.name.as_str()),
            Some("ch_ppocr_mobile_v2.0_cls_infer.onnx")
        );

        // v6 没有 cls 模型：请求它必须报错，而不是悄悄少一个文件。
        let error = reg
            .text_model_set(&super::DefaultModelSelection {
                include_classifier: true,
                ..selection(OcrVersion::PPocrV6, ModelType::Small)
            })
            .expect_err("v6 has no classifier model");
        assert!(error.to_string().contains("cls"), "{error}");
    }

    #[test]
    fn formula_model_set_covers_the_formula_recognizer() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");
        let set = reg
            .formula_model_set()
            .expect("the default table must carry a formula section");

        assert_eq!(set.id, "PP-FormulaNet_plus-M");
        assert_eq!(set.family, "RapidDoc");
        assert_eq!(set.version, "v1.0.0");
        assert_eq!(set.files.len(), 1);
        let file = &set.files[0];
        assert_eq!(file.role, crate::model_set::ModelRole::FormulaRecognizer);
        assert_eq!(file.name, "pp_formulanet_plus_m.onnx");
        assert_eq!(
            file.sha256,
            "71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b"
        );
        assert_eq!(file.size_bytes, Some(593_915_961));
        // 页面公式检测模型在 FormulaPolicy 里是可选的，且没有可信公开下载来源，
        // 因此它不是集合成员（否则会把可选文件变成“模型不齐备”）。
        assert!(!set.has_role(crate::model_set::ModelRole::FormulaDetector));
    }

    #[test]
    fn a_table_without_a_formula_section_reports_a_locating_error() {
        let reg = ModelRegistry::from_yaml_str(CUSTOM_YAML).expect("registry should parse");
        let error = reg
            .formula_model_set()
            .expect_err("a table without `formula:` must not silently return an empty set");
        assert!(error.to_string().contains("formula"), "{error}");
    }

    /// **缺口不能回归**：默认表里每个权重与每个字典都必须携带 SHA-256。
    ///
    /// 只要有人新增一个 `dict:`（或把它退回裸 `dict_url`），这条测试就会失败，
    /// 而不是让字典静默变回“下载时不校验”。
    #[test]
    fn every_default_table_entry_carries_a_sha256() {
        let reg = ModelRegistry::from_default_yaml().expect("registry should parse");
        let looks_like_sha256 = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|hash| hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        };

        let mut models = 0;
        let mut dictionaries = 0;
        for (version, node) in &reg.root.onnxruntime {
            for (task, entries) in [("det", &node.det), ("cls", &node.cls), ("rec", &node.rec)] {
                for (name, entry) in entries {
                    assert!(
                        looks_like_sha256(&entry.sha256),
                        "{version}/{task}/{name} must carry a SHA-256"
                    );
                    models += 1;
                    if task == "rec" {
                        let dict = entry.dict.as_ref().unwrap_or_else(|| {
                            panic!("{version}/rec/{name} must declare a `dict:` block")
                        });
                        assert!(
                            looks_like_sha256(&dict.sha256),
                            "{version}/rec/{name} dictionary must carry a SHA-256"
                        );
                        dictionaries += 1;
                    }
                }
            }
        }
        assert_eq!(models, 40, "det+cls+rec entries in the default table");
        assert_eq!(dictionaries, 30, "dictionary entries in the default table");
    }
}
