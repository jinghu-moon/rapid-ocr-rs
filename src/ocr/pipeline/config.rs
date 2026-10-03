use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    ocr::cls::classifier::ClassifierConfig,
    ocr::config::RecognizerConfig,
    ocr::det::detector::DetectorConfig,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalConfig {
    pub text_score: f32,
    pub use_det: bool,
    pub use_cls: bool,
    pub use_rec: bool,
    pub min_height: usize,
    pub width_height_ratio: f32,
    pub max_side_len: usize,
    pub min_side_len: usize,
    pub return_word_box: bool,
    pub return_single_char_box: bool,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            text_score: 0.5,
            use_det: true,
            use_cls: false,
            use_rec: true,
            min_height: 30,
            width_height_ratio: 8.0,
            max_side_len: 2000,
            min_side_len: 30,
            return_word_box: false,
            return_single_char_box: false,
        }
    }
}

/// 引擎配置。
///
/// 运行时设置（线程 / provider / arena / 公式批大小）只有一份 [`RuntimeConfig`]，
/// 位于 `runtime`：det/cls/rec 三份总是相同的 `runtime` 段已删除，遗留的
/// `det.runtime` 会因为 `deny_unknown_fields` 直接报错，而不是被静默忽略。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub global: GlobalConfig,
    pub runtime: RuntimeConfig,
    pub det: DetectorConfig,
    pub cls: ClassifierConfig,
    pub rec: RecognizerConfig,
}

impl EngineConfig {
    pub fn from_yaml_str(yaml: &str) -> Result<Self> {
        let root = serde_yaml::from_str::<Value>(yaml)?;
        validate_native_required_sections(&root)?;
        let cfg = serde_yaml::from_value::<Self>(root)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_yaml_file(path: impl AsRef<Path>) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::from_yaml_str(&text)
    }

    pub fn validate(&self) -> Result<()> {
        validate_inclusive_range("global.text_score", self.global.text_score, 0.0, 1.0)?;

        if self.global.min_height == 0 {
            return Err(RapidOcrError::Config(
                "global.min_height must be greater than zero".to_string(),
            ));
        }
        if self.global.min_side_len == 0 {
            return Err(RapidOcrError::Config(
                "global.min_side_len must be greater than zero".to_string(),
            ));
        }
        if self.global.max_side_len == 0 {
            return Err(RapidOcrError::Config(
                "global.max_side_len must be greater than zero".to_string(),
            ));
        }
        if self.global.min_side_len > self.global.max_side_len {
            return Err(RapidOcrError::Config(format!(
                "global.min_side_len ({}) must be <= global.max_side_len ({})",
                self.global.min_side_len, self.global.max_side_len
            )));
        }
        if self.global.width_height_ratio <= 0.0 {
            return Err(RapidOcrError::Config(
                "global.width_height_ratio must be > 0".to_string(),
            ));
        }

        if self.det.limit_side_len == 0 {
            return Err(RapidOcrError::Config(
                "det.limit_side_len must be greater than zero".to_string(),
            ));
        }
        validate_inclusive_range("det.thresh", self.det.thresh, 0.0, 1.0)?;
        validate_inclusive_range("det.box_thresh", self.det.box_thresh, 0.0, 1.0)?;
        if self.det.max_candidates == 0 {
            return Err(RapidOcrError::Config(
                "det.max_candidates must be greater than zero".to_string(),
            ));
        }
        if self.det.unclip_ratio <= 0.0 {
            return Err(RapidOcrError::Config(
                "det.unclip_ratio must be > 0".to_string(),
            ));
        }

        if self.cls.cls_batch_num == 0 {
            return Err(RapidOcrError::Config(
                "cls.cls_batch_num must be greater than zero".to_string(),
            ));
        }
        validate_inclusive_range("cls.cls_thresh", self.cls.cls_thresh, 0.0, 1.0)?;
        if self.cls.cls_image_shape.contains(&0) {
            return Err(RapidOcrError::Config(format!(
                "cls.cls_image_shape must not contain zero values, got {:?}",
                self.cls.cls_image_shape
            )));
        }

        if self.rec.rec_batch_num == 0 {
            return Err(RapidOcrError::Config(
                "rec.rec_batch_num must be greater than zero".to_string(),
            ));
        }
        if self.rec.rec_img_shape[0] != 3 {
            return Err(RapidOcrError::Config(format!(
                "rec.rec_img_shape must start with channel=3, got {:?}",
                self.rec.rec_img_shape
            )));
        }
        if self.rec.rec_img_shape.contains(&0) {
            return Err(RapidOcrError::Config(format!(
                "rec.rec_img_shape must not contain zero values, got {:?}",
                self.rec.rec_img_shape
            )));
        }

        validate_runtime_config("runtime", &self.runtime)?;

        Ok(())
    }
}

fn validate_native_required_sections(root: &Value) -> Result<()> {
    let required_sections = ["global", "det", "cls", "rec"];

    for section in required_sections {
        let Some(value) = mapping_get(root, section) else {
            return Err(RapidOcrError::Config(format!(
                "native config missing required section `{section}`"
            )));
        };
        if !value.is_mapping() {
            return Err(RapidOcrError::Config(format!(
                "native config section `{section}` must be a mapping"
            )));
        }
    }

    Ok(())
}

fn validate_runtime_config(prefix: &str, runtime: &RuntimeConfig) -> Result<()> {
    if runtime.intra_threads.is_some_and(|v| v == 0) {
        return Err(RapidOcrError::Config(format!(
            "{prefix}.intra_threads must be greater than zero when set"
        )));
    }
    if runtime.inter_threads.is_some_and(|v| v == 0) {
        return Err(RapidOcrError::Config(format!(
            "{prefix}.inter_threads must be greater than zero when set"
        )));
    }
    if runtime.rayon_threads.is_some_and(|v| v == 0) {
        return Err(RapidOcrError::Config(format!(
            "{prefix}.rayon_threads must be greater than zero when set"
        )));
    }
    if runtime.formula_batch == 0 {
        return Err(RapidOcrError::Config(format!(
            "{prefix}.formula_batch must be greater than zero"
        )));
    }
    Ok(())
}

fn validate_inclusive_range(name: &str, value: f32, min: f32, max: f32) -> Result<()> {
    if !(min..=max).contains(&value) {
        return Err(RapidOcrError::Config(format!(
            "{name} must be in range [{min}, {max}], got {value}"
        )));
    }
    Ok(())
}

fn mapping_get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::String(key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::EngineConfig;

    #[test]
    fn parse_native_yaml_requires_all_sections() {
        let yaml = "global:\n  text_score: 0.77\n";
        let err = EngineConfig::from_yaml_str(yaml).expect_err("must reject missing sections");
        assert!(
            err.to_string()
                .contains("native config missing required section `det`")
        );
    }

    #[test]
    fn parse_native_yaml_rejects_unknown_top_level_fields() {
        let mut yaml = serde_yaml::to_string(&EngineConfig::default())
            .expect("default config should serialize");
        yaml.push_str("unexpected: true\n");
        let err = EngineConfig::from_yaml_str(&yaml).expect_err("must reject unknown top-level");
        assert!(err.to_string().contains("unknown field `unexpected`"));
    }

    /// 运行时设置只有一份：遗留的 per-stage `runtime:` 必须**报错并定位到字段**，
    /// 而不是被当成未知键静默忽略（那会让用户以为线程设置生效了）。
    #[test]
    fn per_stage_runtime_sections_are_rejected_with_a_locating_error() {
        for section in ["det", "cls", "rec"] {
            let mut yaml = String::from("global: {}\n");
            for name in ["det", "cls", "rec"] {
                if name == section {
                    yaml.push_str(&format!("{name}:\n  runtime:\n    intra_threads: 4\n"));
                } else {
                    yaml.push_str(&format!("{name}: {{}}\n"));
                }
            }
            let err = EngineConfig::from_yaml_str(&yaml)
                .expect_err("a per-stage runtime section must not deserialize");
            let message = err.to_string();
            assert!(
                message.contains("unknown field") && message.contains("runtime"),
                "the error for `{section}.runtime` must locate the removed field: {message}"
            );
        }

        // 单一顶层 `runtime:` 段必须继续可用。
        let config = EngineConfig::from_yaml_str(
            "global: {}\ndet: {}\ncls: {}\nrec: {}\nruntime:\n  intra_threads: 4\n",
        )
        .expect("the single top-level runtime section must deserialize");
        assert_eq!(config.runtime.intra_threads, Some(4));
    }

    /// `formula_batch` 必须像其他 runtime 字段一样被校验。
    #[test]
    fn runtime_formula_batch_must_be_greater_than_zero() {
        let mut config = EngineConfig::default();
        config.runtime.formula_batch = 0;
        let err = config.validate().expect_err("zero formula batch must fail");
        assert!(
            err.to_string().contains("runtime.formula_batch")
                && err.to_string().contains("greater than zero"),
            "error: {err}"
        );

        config.runtime.formula_batch = 1;
        config
            .validate()
            .expect("a positive formula batch is valid");
    }

    /// 线程字段的零值同样必须被拒绝。
    #[test]
    fn runtime_thread_fields_reject_zero() {
        let mut zero_intra = EngineConfig::default();
        zero_intra.runtime.intra_threads = Some(0);
        assert!(
            zero_intra
                .validate()
                .expect_err("zero intra threads must fail")
                .to_string()
                .contains("runtime.intra_threads")
        );

        let mut zero_inter = EngineConfig::default();
        zero_inter.runtime.inter_threads = Some(0);
        assert!(
            zero_inter
                .validate()
                .expect_err("zero inter threads must fail")
                .to_string()
                .contains("runtime.inter_threads")
        );

        let mut zero_rayon = EngineConfig::default();
        zero_rayon.runtime.rayon_threads = Some(0);
        assert!(
            zero_rayon
                .validate()
                .expect_err("zero rayon threads must fail")
                .to_string()
                .contains("runtime.rayon_threads")
        );
    }
}
