//! OCR 后端：真实引擎，以及供测试替换的构造工厂。
//!
//! # 为什么要在这里留一个工厂
//!
//! §12 要求验证的东西里有相当一部分**不是**"真实模型能不能识别"，而是运行期行为：
//! `202 → queued → running → succeeded`、双队列各自 503、`--max-queue-*` 与
//! `--max-consecutive-*` 的双向公平性、`/result` 的体积上限。这些必须能在几秒内跑完，
//! 而真实引擎每张图要几百毫秒到数秒、还要 30 MB 模型在场。
//!
//! 因此 worker 只依赖 [`OcrBackend`] 这一层薄抽象；生产路径的工厂是
//! [`real_engine_factory`]（包住 `RapidOcrEngine`），测试路径注入一个可脚本化的后端
//! （见 `serve/tests.rs` 的 `ScriptedBackend`）。**公平性测试用慢速后端驱动队列**，
//! 报告里写明了这一点与它的实际参数。
//!
//! 这层抽象只有两个方法，且引擎本身是 `&mut self`（§8.2 的"固定 1 个 worker"由此而来）。

use std::sync::Arc;

use rapid_ocr_rs::{
    EngineConfig, OcrEngine, OcrOutput, OcrRequest, PipelineProviderResolutions, RapidOcrEngine,
    RapidOcrError, ResolvedExecutionProvider,
};

/// 后端实测的 provider 事实（进 `/api/status` 的 `selected_ep` / `fallback_to_cpu`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BackendProvider {
    pub selected_ep: String,
    pub fallback_to_cpu: bool,
}

/// 一次识别的唯一入口。
pub(super) trait OcrBackend: Send {
    fn recognize(&mut self, request: OcrRequest) -> Result<OcrOutput, RapidOcrError>;
    /// 会话建立后**实测**到的 provider（§7.5：只在 `Loading → Ready` 处可判定）。
    fn provider(&self) -> BackendProvider;
}

/// 引擎构造工厂：签名与 [`RapidOcrEngine::new`] 一致，只是返回装箱的后端。
pub(super) type EngineFactory =
    Arc<dyn Fn(&EngineConfig) -> Result<Box<dyn OcrBackend>, RapidOcrError> + Send + Sync>;

/// 生产路径的工厂。
pub(super) fn real_engine_factory() -> EngineFactory {
    Arc::new(|config| Ok(Box::new(RealEngine::new(config)?)))
}

/// `RapidOcrEngine` 的薄包装：把公开的 `OcrEngine` 实现接到 [`OcrBackend`] 上。
struct RealEngine(RapidOcrEngine);

impl RealEngine {
    fn new(config: &EngineConfig) -> Result<Self, RapidOcrError> {
        Ok(Self(RapidOcrEngine::new(config.clone())?))
    }

    /// 三个会话由**同一份**运行时档案建立（`RuntimeProfile::session_runtime`），
    /// 因此解析结果必然一致；这里仍然逐项核对，避免将来出现"两个会话用了不同的 EP"
    /// 却只报告其中一个。
    fn resolutions(&self) -> PipelineProviderResolutions {
        self.0.provider_resolutions()
    }
}

impl OcrBackend for RealEngine {
    fn recognize(&mut self, request: OcrRequest) -> Result<OcrOutput, RapidOcrError> {
        OcrEngine::recognize(&mut self.0, request)
    }

    fn provider(&self) -> BackendProvider {
        let resolutions = self.resolutions();
        let primary = resolutions.rec;
        debug_assert_eq!(
            resolutions.det.selected_ep, primary.selected_ep,
            "detector and recognizer must resolve to the same execution provider"
        );
        debug_assert_eq!(
            resolutions.det.fallback_used, primary.fallback_used,
            "detector and recognizer must agree on the fallback decision"
        );
        BackendProvider {
            selected_ep: provider_label(primary.selected_ep),
            fallback_to_cpu: primary.fallback_used,
        }
    }
}

/// EP 的唯一展示文本（与 `format_provider_preference` 的取值同名）。
pub(super) fn provider_label(provider: ResolvedExecutionProvider) -> String {
    match provider {
        ResolvedExecutionProvider::Cpu => "cpu".to_string(),
        ResolvedExecutionProvider::Cuda => "cuda".to_string(),
        ResolvedExecutionProvider::DirectMl => "directml".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::provider_label;
    use rapid_ocr_rs::ResolvedExecutionProvider;

    #[test]
    fn every_execution_provider_has_a_stable_label() {
        assert_eq!(provider_label(ResolvedExecutionProvider::Cpu), "cpu");
        assert_eq!(provider_label(ResolvedExecutionProvider::Cuda), "cuda");
        assert_eq!(
            provider_label(ResolvedExecutionProvider::DirectMl),
            "directml"
        );
    }
}
