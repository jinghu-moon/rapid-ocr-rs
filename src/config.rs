use std::{borrow::Cow, path::Path};

use image::ImageReader;
use serde::{Deserialize, Serialize};

use crate::error::{RapidOcrError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorOrder {
    Bgr,
    Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    #[default]
    Mobile,
    Server,
    Tiny,
    Small,
    Medium,
}

impl ModelType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mobile => "mobile",
            Self::Server => "server",
            Self::Tiny => "tiny",
            Self::Small => "small",
            Self::Medium => "medium",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum OcrVersion {
    #[default]
    #[serde(rename = "PP-OCRv4")]
    PPocrV4,
    #[serde(rename = "PP-OCRv5")]
    PPocrV5,
    #[serde(rename = "PP-OCRv6")]
    PPocrV6,
}

impl OcrVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PPocrV4 => "PP-OCRv4",
            Self::PPocrV5 => "PP-OCRv5",
            Self::PPocrV6 => "PP-OCRv6",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LangDet {
    #[default]
    Ch,
    En,
    Multi,
}

impl LangDet {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ch => "ch",
            Self::En => "en",
            Self::Multi => "multi",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LangCls {
    #[default]
    Ch,
}

impl LangCls {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ch => "ch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LangRec {
    #[default]
    Ch,
    ChDoc,
    En,
    Arabic,
    ChineseCht,
    Cyrillic,
    Devanagari,
    Japan,
    Korean,
    Ka,
    Latin,
    Ta,
    Te,
    Eslav,
    Th,
    El,
}

impl LangRec {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ch => "ch",
            Self::ChDoc => "ch_doc",
            Self::En => "en",
            Self::Arabic => "arabic",
            Self::ChineseCht => "chinese_cht",
            Self::Cyrillic => "cyrillic",
            Self::Devanagari => "devanagari",
            Self::Japan => "japan",
            Self::Korean => "korean",
            Self::Ka => "ka",
            Self::Latin => "latin",
            Self::Ta => "ta",
            Self::Te => "te",
            Self::Eslav => "eslav",
            Self::Th => "th",
            Self::El => "el",
        }
    }
}

/// 执行提供者偏好。
///
/// Windows x64 收窄之后只剩三个：CPU（默认）、CUDA、DirectML。
/// CANN 不是 Windows x64 目标，已整体删除（feature、枚举变体、序列化与测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPreference {
    #[default]
    Cpu,
    Cuda {
        device_id: usize,
    },
    DirectMl {
        device_id: usize,
    },
}

/// 单个 ONNX Runtime 会话的运行时配置。
///
/// 这里**没有** `backend` 字段：本 crate 只使用 ONNX Runtime，曾经的单变体枚举
/// `RuntimeBackend::OnnxCpu` 是伪抽象（只有一个取值，却要检查、序列化并出现在 YAML 里），
/// 已删除。provider 选择由 [`RuntimeConfig::provider_preference`] 表达。
///
/// 这份配置在引擎里**只有一份**（`EngineConfig.runtime`），不再是 det/cls/rec 各一份：
/// 三份总是相同的配置没有任何单一解释处，也无法回答“这个进程到底用了多少线程”。
///
/// **线程字段的语义只有一个**：[`RuntimeConfig::effective_session_threads`] 是唯一的
/// 解析实现，`OrtSession` 与 `RuntimeProfile::plan` 都调用它，因此把这份配置直接交给
/// `FormulaSession`（`formula_bench` / `formula_eval`）与交给 `RapidOcrEngine` 得到的是
/// 同一套线程行为。Rayon 份额与批大小的完整解析见 `runtime::profile`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// ORT 每个会话的 intra 线程数；`None`/`0` = 未显式设置（见
    /// [`RuntimeConfig::effective_session_threads`]）。
    pub intra_threads: Option<usize>,
    /// ORT 每个会话的 inter 线程数；`None`/`0` = 未显式设置。
    pub inter_threads: Option<usize>,
    /// 是否允许从未显式设置的字段推导线程数（`false` 表示“不要自动配置”）。
    pub auto_tune_threads: bool,
    /// Rayon 全局线程池的线程数；`None`/`0` = 不配置。显式设置时它是**显式请求**，
    /// 无法生效（进程里已有不同大小的池）会返回错误而不是被静默替换。
    pub rayon_threads: Option<usize>,
    pub enable_cpu_mem_arena: bool,
    pub fail_if_provider_unavailable: bool,
    pub provider_preference: ProviderPreference,
    /// 单次公式识别的批大小上限（必须 > 0）。
    pub formula_batch: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            intra_threads: None,
            inter_threads: None,
            auto_tune_threads: true,
            rayon_threads: None,
            enable_cpu_mem_arena: true,
            fail_if_provider_unavailable: false,
            provider_preference: ProviderPreference::default(),
            formula_batch: 16,
        }
    }
}

impl RuntimeConfig {
    /// 这份配置**实际**要求 ORT 会话使用的 `(intra_threads, inter_threads)`。
    ///
    /// 这是线程策略的**唯一实现**：引擎路径（[`crate::runtime::profile::RuntimeProfile::plan`]）
    /// 与直接构造会话的路径（`FormulaSession` / `formula_bench` / `formula_eval` /
    /// [`crate::runtime::session::OrtSession::open_unchecked`]）都调用这一个函数，
    /// 因此同一个公开字段在所有入口处只有一种含义。
    ///
    /// 语义：
    ///
    /// - 显式值永远优先：`intra_threads` / `inter_threads` 里 `Some(n)` 且 `n > 0` 时原样生效
    ///   （与 `auto_tune_threads` 无关）；`Some(0)` 与 `None` 一样表示“未设置”；
    /// - 未显式设置的字段在该字段没有被显式给出**且** `auto_tune_threads = true` 时，
    ///   从共享预算推导：`intra = runtime::profile::auto_tuned_thread_budget()`
    ///   （`min(逻辑核数, 物理核数)`，至少 1），`inter = 1`（ORT 的 inter 并行对本 workload
    ///   无益）。预算只有那一份定义，这里不重复实现任何算术；
    /// - `auto_tune_threads = false` 且未显式设置的字段返回 `None`：`None` 的语义是
    ///   **不配置**，`OrtSession` 不会调用 `with_intra_threads` / `with_inter_threads`，
    ///   ONNX Runtime 保留自己的默认值。
    ///
    /// 返回 `(None, None)` 因此只表示“两个线程数都不配置”，不表示 0 线程。
    ///
    /// | `intra_threads` | `inter_threads` | `auto_tune_threads` | 结果 |
    /// | --- | --- | --- | --- |
    /// | `Some(a > 0)` | `Some(b > 0)` | 任意 | `(Some(a), Some(b))` |
    /// | `Some(a > 0)` | 未设置 | `true` | `(Some(a), Some(1))` |
    /// | `Some(a > 0)` | 未设置 | `false` | `(Some(a), None)` |
    /// | 未设置 | `Some(b > 0)` | `true` | `(Some(budget), Some(b))` |
    /// | 未设置 | `Some(b > 0)` | `false` | `(None, Some(b))` |
    /// | 未设置 | 未设置 | `true` | `(Some(budget), Some(1))` |
    /// | 未设置 | 未设置 | `false` | `(None, None)` |
    pub fn effective_session_threads(&self) -> (Option<usize>, Option<usize>) {
        let explicit_intra = self.intra_threads.filter(|value| *value > 0);
        let explicit_inter = self.inter_threads.filter(|value| *value > 0);
        if !self.auto_tune_threads {
            return (explicit_intra, explicit_inter);
        }
        let budget = crate::runtime::profile::auto_tuned_thread_budget();
        (explicit_intra.or(Some(budget)), explicit_inter.or(Some(1)))
    }

    /// 是否显式请求过线程数：`intra_threads` / `inter_threads` / `rayon_threads` 里
    /// **任意**一个给了 `Some(n)` 且 `n > 0`。
    ///
    /// `Some(0)` 与 `None` 一样表示“未设置”。这个判断是 [`crate::runtime::profile::ThreadSource`]
    /// 的依据：显式请求过的配置不能被静默替换成另一个数字（例如已经存在的 Rayon 全局池），
    /// 而没有显式请求的配置可以（并会报告实际生效值）。
    pub fn has_explicit_thread_request(&self) -> bool {
        let set = |value: Option<usize>| value.filter(|value| *value > 0).is_some();
        set(self.intra_threads) || set(self.inter_threads) || set(self.rayon_threads)
    }
}

#[derive(Debug, Clone)]
pub struct RecImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
    color_order: ColorOrder,
}

impl RecImage {
    pub fn from_bgr_u8(width: usize, height: usize, data: Vec<u8>) -> Result<Self> {
        Self::new(width, height, data, ColorOrder::Bgr)
    }

    pub fn from_rgb_u8(width: usize, height: usize, data: Vec<u8>) -> Result<Self> {
        Self::new(width, height, data, ColorOrder::Rgb)
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let image = ImageReader::open(path.as_ref())
            .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?
            .decode()
            .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?
            .to_rgb8();

        let (width, height) = image.dimensions();
        let raw = image.into_raw();

        let mut bgr = vec![0_u8; raw.len()];
        for (src, dst) in raw
            .as_chunks::<3>()
            .0
            .iter()
            .zip(bgr.as_chunks_mut::<3>().0.iter_mut())
        {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
        }

        Self::new(width as usize, height as usize, bgr, ColorOrder::Bgr)
    }

    pub fn new(
        width: usize,
        height: usize,
        data: Vec<u8>,
        color_order: ColorOrder,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(RapidOcrError::InvalidImage(
                "image width and height must be greater than zero".to_string(),
            ));
        }

        let expected = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(3))
            .ok_or_else(|| RapidOcrError::InvalidImage("image dimensions overflow".to_string()))?;

        if data.len() != expected {
            return Err(RapidOcrError::InvalidImage(format!(
                "image data size mismatch: expected {expected}, got {}",
                data.len()
            )));
        }

        Ok(Self {
            width,
            height,
            data,
            color_order,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn color_order(&self) -> ColorOrder {
        self.color_order
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
    pub fn as_bgr_cow(&self) -> Cow<'_, [u8]> {
        match self.color_order {
            ColorOrder::Bgr => Cow::Borrowed(&self.data),
            ColorOrder::Rgb => {
                let mut out = vec![0_u8; self.data.len()];
                for (src, dst) in self
                    .data
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .zip(out.as_chunks_mut::<3>().0.iter_mut())
                {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                }
                Cow::Owned(out)
            }
        }
    }

    pub fn as_bgr_bytes(&self) -> Vec<u8> {
        self.as_bgr_cow().into_owned()
    }

    pub fn wh_ratio(&self) -> f32 {
        self.width() as f32 / self.height() as f32
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{ColorOrder, ProviderPreference, RecImage, RuntimeConfig};

    #[test]
    fn rec_image_rejects_zero_dimension() {
        let err = RecImage::from_bgr_u8(0, 10, vec![]).expect_err("must reject zero width");
        assert!(
            err.to_string()
                .contains("image width and height must be greater than zero")
        );
    }

    #[test]
    fn runtime_config_defaults_are_explicit() {
        let cfg = RuntimeConfig::default();
        assert!(cfg.auto_tune_threads);
        assert_eq!(cfg.rayon_threads, None);
        assert!(cfg.enable_cpu_mem_arena);
        assert!(!cfg.fail_if_provider_unavailable);
        assert_eq!(cfg.provider_preference, ProviderPreference::Cpu);
        assert_eq!(cfg.formula_batch, 16);
    }

    /// YAML 里已经删除的字段必须被拒绝，并且错误要能定位到字段名。
    #[test]
    fn removed_runtime_fields_are_rejected_with_a_locating_error() {
        // `backend`（单变体伪抽象）、`provider_preference: cann` 与 `vision_backend`
        // （OpenCV 后端删除后不再有后端可选）都已删除。
        let error = serde_yaml::from_str::<RuntimeConfig>("backend: onnx_cpu")
            .expect_err("the removed `backend` field must not deserialize");
        assert!(
            error.to_string().contains("unknown field"),
            "error must locate the removed field: {error}"
        );

        let error = serde_yaml::from_str::<RuntimeConfig>("vision_backend: pure_rust")
            .expect_err("the removed `vision_backend` field must not deserialize");
        assert!(
            error.to_string().contains("unknown field"),
            "error must locate the removed backend field: {error}"
        );

        let error = serde_yaml::from_str::<RuntimeConfig>("provider_preference: cann")
            .expect_err("the removed CANN provider must not deserialize");
        let message = error.to_string();
        assert!(
            message.contains("unknown variant") || message.contains("did not match any variant"),
            "error must locate the removed provider: {message}"
        );

        // 仍然合法的取值必须继续工作。
        let config: RuntimeConfig = serde_yaml::from_str("provider_preference: cpu")
            .expect("the supported CPU preference must deserialize");
        assert_eq!(config.provider_preference, ProviderPreference::Cpu);
    }

    #[test]
    fn as_bgr_cow_borrows_for_bgr_images() {
        let image =
            RecImage::new(2, 1, vec![1, 2, 3, 4, 5, 6], ColorOrder::Bgr).expect("valid image");
        let bgr = image.as_bgr_cow();
        assert!(matches!(bgr, Cow::Borrowed(_)));
    }

    #[test]
    fn as_bgr_cow_allocates_for_rgb_images() {
        let image = RecImage::new(1, 1, vec![10, 20, 30], ColorOrder::Rgb).expect("valid image");
        let bgr = image.as_bgr_cow();
        assert!(matches!(bgr, Cow::Owned(_)));
        assert_eq!(bgr.as_ref(), &[30, 20, 10]);
    }
}
