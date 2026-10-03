//! Provider 解析：请求哪个执行提供者、实际是否可用、失败时给出什么错误。
//!
//! # 支持范围
//!
//! crate 只支持 Windows x64，因此公开的 provider 集合只有三个：
//!
//! | provider | feature | 说明 |
//! | --- | --- | --- |
//! | CPU | 默认（`ort-runtime`） | 基础路径，始终可用 |
//! | DirectML | `directml-provider` | Windows GPU 加速 |
//! | CUDA | `cuda-provider` | NVIDIA GPU 加速 |
//!
//! CANN 不是 Windows 目标，已从 feature、枚举、序列化和测试中删除；
//! 历史文档里的 CANN 记录只作为历史，不再代表当前支持面。
//!
//! # 错误语义（三类必须可区分）
//!
//! 1. **feature 未编译进来**：`UnsupportedProvider`，提示重新构建时加 feature；
//! 2. **运行库不可用**：`is_available()` 报告 false 或报错 —— 严格模式返回
//!    `Config` 错误，非严格模式回退 CPU 并置 `fallback_used = true`；
//! 3. **严格模式拒绝回退**：`require_requested_provider` 在 `fallback_used` 时返回
//!    `UnsupportedProvider`，与进程级 `fail_if_provider_unavailable` 无关。
//!
//! # 关于“已解析”的准确含义
//!
//! [`ProviderResolution::resolved`] 表示**交给 ONNX Runtime 的 EP 链**以及
//! `is_available()` 的自报结果，它**不等于**“模型真的在该 EP 上执行”：
//! ORT 会把单个节点回退到下一个 EP（通常是 CPU），这个逐节点分配不通过本 API 暴露。
//!
//! 阶段 0/2 在本机复现了这种现象，并定位到根因：
//!
//! | 现象 | 实测 |
//! | --- | --- |
//! | CPU | p50 1039.5 ms |
//! | DirectML | p50 508.7 ms（约 2×，`DirectML.dll` 已被加载） |
//! | CUDA | p50 1034.2 ms（与 CPU 相同，即使 `onnxruntime_providers_cuda.dll` 就在 exe 旁边） |
//!
//! | 条件 | 本机状态 |
//! | --- | --- |
//! | 加载的 ONNX Runtime | `ort_runtime_version()` = `1.28.0` |
//! | CUDA provider 库 | 存在（ort 缓存，62 MB） |
//! | CUDA 工具链 | 已安装，`cudart64_12.dll`/`cublas64_12.dll` 且在 PATH 上 |
//! | **cuDNN** | **缺失**（`cudnn*.dll` 不存在） |
//!
//! 也就是：CUDA EP 的 provider 库能被加载、`is_available()` 返回 true，但缺少
//! cuDNN 时它无法真正执行模型，节点全部落在 CPU 上，而本 API 无法察觉。
//!
//! 因此本 crate 的规则是：**任何加速结论都必须来自实测（P50/P90 对比），
//! 不得仅凭 `resolved` 宣称加速**；benchmark 与评测报告都会同时记录 provider 解析结果、
//! 实测耗时与 `ort_runtime_version()`。在装上与 ORT 版本匹配的 cuDNN 之前，
//! 本机的 CUDA 一律记为“未验证”，不作为可用 provider 声明。

#[cfg(feature = "cuda-provider")]
use ort::ep::CUDA;
#[cfg(feature = "directml-provider")]
use ort::ep::DirectML;
#[cfg(any(feature = "cuda-provider", feature = "directml-provider"))]
use ort::ep::ExecutionProvider;
use ort::ep::{CPU, ExecutionProviderDispatch};

use crate::{
    config::ProviderPreference,
    error::{RapidOcrError, Result},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedExecutionProvider {
    Cpu,
    Cuda,
    DirectMl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderResolution {
    pub requested: ProviderPreference,
    /// 交给 ONNX Runtime 的加速 EP；发生回退时为 `Cpu`。
    ///
    /// 注意：这**不代表**模型真的在该 EP 上逐节点执行，见模块文档。
    pub resolved: ResolvedExecutionProvider,
    /// 是否因为 `is_available()` 报告不可用而替换成 CPU。
    pub fallback_used: bool,
}

#[derive(Debug)]
pub struct ProviderChain {
    pub providers: Vec<ExecutionProviderDispatch>,
    pub resolution: ProviderResolution,
}

/// 当前进程实际加载的 ONNX Runtime 版本字符串。
///
/// 直接查询运行库自己（`OrtGetApiBase()->GetVersionString()`），而不是读 Cargo
/// 依赖版本：本 crate 通过导入库链接，运行时可能加载 Windows 内置的
/// `onnxruntime.dll`，两者版本并不相同，而这个事实决定了哪些 EP 真正可用。
pub fn ort_runtime_version() -> Option<String> {
    // SAFETY: `OrtGetApiBase` 由链接的 onnxruntime 导入库提供；
    // 返回的指针在进程生命周期内有效，字符串也由运行库持有。
    unsafe {
        let base = ort::sys::OrtGetApiBase();
        if base.is_null() {
            return None;
        }
        let raw = ((*base).GetVersionString)();
        if raw.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned())
    }
}

/// “feature 未编译进来”的统一错误文本。
///
/// 两个加速 feature 都被打开时，所有 `#[cfg(not(feature = ...))]` 分支都不存在，
/// 于是这里没有调用点；文本定义仍然保留一处，避免两套措辞。
#[cfg_attr(
    all(feature = "cuda-provider", feature = "directml-provider"),
    allow(dead_code)
)]
fn feature_disabled(provider: &str, feature: &str) -> RapidOcrError {
    RapidOcrError::UnsupportedProvider(format!(
        "{provider} provider support is not compiled in; rebuild with `--features {feature}`"
    ))
}

/// “运行库不可用”的统一错误文本。
///
/// 只有编译进加速 provider 时才会调用；没有加速 feature 的构建里保留它，
/// 是为了让“运行库不可用”这句话只有一份定义（测试也会直接断言两类错误可区分）。
#[cfg_attr(
    not(any(feature = "cuda-provider", feature = "directml-provider")),
    allow(dead_code)
)]
fn runtime_unavailable(provider: &str, detail: &str) -> String {
    format!(
        "{provider} is unavailable in the loaded ONNX Runtime ({detail}); install a \
         {provider}-enabled ONNX Runtime runtime"
    )
}

pub fn resolve_execution_providers(
    preference: &ProviderPreference,
    enable_cpu_mem_arena: bool,
    fail_if_provider_unavailable: bool,
) -> Result<ProviderChain> {
    let cpu_provider = || {
        CPU::default()
            .with_arena_allocator(enable_cpu_mem_arena)
            .build()
    };

    match preference {
        ProviderPreference::Cpu => Ok(ProviderChain {
            providers: vec![cpu_provider()],
            resolution: ProviderResolution {
                requested: ProviderPreference::Cpu,
                resolved: ResolvedExecutionProvider::Cpu,
                fallback_used: false,
            },
        }),
        ProviderPreference::Cuda { device_id } => {
            #[cfg(feature = "cuda-provider")]
            {
                resolve_accelerator_execution_providers(
                    "CUDA",
                    "cuda-provider",
                    ProviderPreference::Cuda {
                        device_id: *device_id,
                    },
                    ResolvedExecutionProvider::Cuda,
                    *device_id,
                    cpu_provider(),
                    fail_if_provider_unavailable,
                    |id| {
                        let provider = CUDA::default().with_device_id(id);
                        let available = provider.is_available().map_err(|e| {
                            RapidOcrError::Config(runtime_unavailable("CUDA", &e.to_string()))
                        })?;
                        Ok((available, provider.build()))
                    },
                )
            }
            #[cfg(not(feature = "cuda-provider"))]
            {
                let _ = (device_id, fail_if_provider_unavailable);
                Err(feature_disabled("CUDA", "cuda-provider"))
            }
        }
        ProviderPreference::DirectMl { device_id } => {
            #[cfg(feature = "directml-provider")]
            {
                resolve_accelerator_execution_providers(
                    "DirectML",
                    "directml-provider",
                    ProviderPreference::DirectMl {
                        device_id: *device_id,
                    },
                    ResolvedExecutionProvider::DirectMl,
                    *device_id,
                    cpu_provider(),
                    fail_if_provider_unavailable,
                    |id| {
                        let provider = DirectML::default().with_device_id(id);
                        let available = provider.is_available().map_err(|e| {
                            RapidOcrError::Config(runtime_unavailable("DirectML", &e.to_string()))
                        })?;
                        Ok((available, provider.build()))
                    },
                )
            }
            #[cfg(not(feature = "directml-provider"))]
            {
                let _ = (device_id, fail_if_provider_unavailable);
                Err(feature_disabled("DirectML", "directml-provider"))
            }
        }
    }
}

#[cfg(any(feature = "cuda-provider", feature = "directml-provider"))]
fn device_id_to_i32(provider_name: &str, device_id: usize) -> Result<i32> {
    i32::try_from(device_id).map_err(|_| {
        RapidOcrError::Config(format!(
            "invalid {provider_name} device_id {device_id}: value exceeds i32 range"
        ))
    })
}

#[cfg(any(feature = "cuda-provider", feature = "directml-provider"))]
#[allow(clippy::too_many_arguments)]
fn resolve_accelerator_execution_providers<F>(
    provider_name: &str,
    _feature: &str,
    requested: ProviderPreference,
    preferred: ResolvedExecutionProvider,
    device_id: usize,
    cpu_provider: ExecutionProviderDispatch,
    fail_if_provider_unavailable: bool,
    prepare_provider: F,
) -> Result<ProviderChain>
where
    F: FnOnce(i32) -> Result<(bool, ExecutionProviderDispatch)>,
{
    let device_id_i32 = device_id_to_i32(provider_name, device_id)?;
    let (is_available, provider_dispatch) = prepare_provider(device_id_i32)?;
    let resolution = decide_provider_resolution(
        requested,
        preferred,
        is_available,
        fail_if_provider_unavailable,
    )?;

    if resolution.fallback_used {
        Ok(ProviderChain {
            providers: vec![cpu_provider],
            resolution,
        })
    } else {
        Ok(ProviderChain {
            providers: vec![provider_dispatch, cpu_provider],
            resolution,
        })
    }
}

#[cfg(any(feature = "cuda-provider", feature = "directml-provider"))]
fn decide_provider_resolution(
    requested: ProviderPreference,
    preferred: ResolvedExecutionProvider,
    preferred_is_available: bool,
    fail_if_provider_unavailable: bool,
) -> Result<ProviderResolution> {
    if preferred_is_available {
        return Ok(ProviderResolution {
            requested,
            resolved: preferred,
            fallback_used: false,
        });
    }

    if fail_if_provider_unavailable {
        return Err(RapidOcrError::Config(strict_mode_message(requested)));
    }

    Ok(ProviderResolution {
        requested,
        resolved: ResolvedExecutionProvider::Cpu,
        fallback_used: true,
    })
}

/// 严格模式（`fail_if_provider_unavailable = true`）下的统一错误文本。
///
/// 同上：只有加速 provider 构建会调用，但文本定义只保留一处。
#[cfg_attr(
    not(any(feature = "cuda-provider", feature = "directml-provider")),
    allow(dead_code)
)]
fn strict_mode_message(requested: ProviderPreference) -> String {
    format!(
        "requested execution provider {} is unavailable and fail_if_provider_unavailable=true",
        format_provider_preference(requested)
    )
}

/// Rejects a provider resolution that silently fell back to CPU.
///
/// `RuntimeConfig::fail_if_provider_unavailable` is a process-wide policy: by
/// default an unavailable accelerator degrades to CPU. Domains whose contract
/// requires the requested accelerator (the formula token session) must call
/// this instead of trusting that flag, so a caller can never observe a
/// different execution provider than the one it asked for.
pub fn require_requested_provider(resolution: ProviderResolution) -> Result<ProviderResolution> {
    if resolution.fallback_used {
        return Err(RapidOcrError::UnsupportedProvider(format!(
            "requested execution provider {} is unavailable and this domain rejects silent CPU \
             fallback; enable the provider feature, install its runtime, or request \
             `ProviderPreference::Cpu` explicitly",
            format_provider_preference(resolution.requested)
        )));
    }
    Ok(resolution)
}

fn format_provider_preference(preference: ProviderPreference) -> String {
    match preference {
        ProviderPreference::Cpu => "cpu".to_string(),
        ProviderPreference::Cuda { device_id } => format!("cuda(device_id={device_id})"),
        ProviderPreference::DirectMl { device_id } => {
            format!("directml(device_id={device_id})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderResolution, ResolvedExecutionProvider, feature_disabled,
        format_provider_preference, ort_runtime_version, require_requested_provider,
        runtime_unavailable, strict_mode_message,
    };
    use crate::config::ProviderPreference;
    use crate::error::RapidOcrError;

    #[test]
    fn every_remaining_provider_is_named_and_formatted() {
        // 穷举三个 provider：删除 CANN 后这里的 match 必须仍然完整，
        // 新增/遗留变体会在编译期被抓住。
        assert_eq!(format_provider_preference(ProviderPreference::Cpu), "cpu");
        assert_eq!(
            format_provider_preference(ProviderPreference::Cuda { device_id: 0 }),
            "cuda(device_id=0)"
        );
        assert_eq!(
            format_provider_preference(ProviderPreference::DirectMl { device_id: 1 }),
            "directml(device_id=1)"
        );
    }

    #[test]
    fn feature_disabled_and_runtime_unavailable_are_distinguishable() {
        let disabled = feature_disabled("CUDA", "cuda-provider").to_string();
        assert!(disabled.contains("not compiled in"), "{disabled}");
        assert!(disabled.contains("--features cuda-provider"), "{disabled}");

        let unavailable = runtime_unavailable("DirectML", "provider library missing");
        assert!(unavailable.contains("unavailable in the loaded ONNX Runtime"));
        assert!(unavailable.contains("provider library missing"));
        assert_ne!(disabled, unavailable);
    }

    #[test]
    fn strict_mode_message_names_the_requested_provider() {
        let message = strict_mode_message(ProviderPreference::Cuda { device_id: 3 });
        assert!(message.contains("cuda(device_id=3)"), "{message}");
        assert!(
            message.contains("fail_if_provider_unavailable=true"),
            "{message}"
        );
    }

    #[test]
    fn require_requested_provider_rejects_silent_cpu_fallback() {
        let resolution = ProviderResolution {
            requested: ProviderPreference::Cuda { device_id: 0 },
            resolved: ResolvedExecutionProvider::Cpu,
            fallback_used: true,
        };
        let error = require_requested_provider(resolution)
            .expect_err("fallback must be rejected regardless of process-wide policy");
        assert!(
            matches!(error, RapidOcrError::UnsupportedProvider(_)),
            "error: {error}"
        );
        assert!(
            error.to_string().contains("cuda(device_id=0)"),
            "error: {error}"
        );
    }

    #[test]
    fn require_requested_provider_accepts_resolved_accelerator_and_cpu() {
        let accelerated = ProviderResolution {
            requested: ProviderPreference::DirectMl { device_id: 1 },
            resolved: ResolvedExecutionProvider::DirectMl,
            fallback_used: false,
        };
        assert_eq!(
            require_requested_provider(accelerated)
                .expect("resolved provider must pass")
                .resolved,
            ResolvedExecutionProvider::DirectMl
        );

        let cpu = ProviderResolution {
            requested: ProviderPreference::Cpu,
            resolved: ResolvedExecutionProvider::Cpu,
            fallback_used: false,
        };
        assert!(
            require_requested_provider(cpu).is_ok(),
            "explicit CPU must always pass"
        );
    }

    /// 报告里必须能写清“实际加载的是哪个 ONNX Runtime”，
    /// 因为本 crate 链接导入库、运行时可能加载系统自带的 `onnxruntime.dll`。
    #[test]
    fn ort_runtime_version_is_reported() {
        let version = ort_runtime_version().expect("the linked ONNX Runtime must report a version");
        assert!(
            version.split('.').count() >= 2,
            "version string should look like `1.17.0`, got {version:?}"
        );
    }

    /// 未启用 feature 时，请求该 provider 必须返回明确的“未编译进来”错误，
    /// 而不是“仅 Windows 可用”这类与本机事实矛盾的信息。
    #[test]
    #[cfg(not(feature = "directml-provider"))]
    fn directml_without_feature_reports_the_missing_feature() {
        let error = super::resolve_execution_providers(
            &ProviderPreference::DirectMl { device_id: 0 },
            false,
            false,
        )
        .expect_err("DirectML must not resolve without the feature");
        let message = error.to_string();
        assert!(message.contains("not compiled in"), "{message}");
        assert!(message.contains("directml-provider"), "{message}");
    }

    #[test]
    #[cfg(not(feature = "cuda-provider"))]
    fn cuda_without_feature_reports_the_missing_feature() {
        let error = super::resolve_execution_providers(
            &ProviderPreference::Cuda { device_id: 0 },
            false,
            false,
        )
        .expect_err("CUDA must not resolve without the feature");
        let message = error.to_string();
        assert!(message.contains("not compiled in"), "{message}");
        assert!(message.contains("cuda-provider"), "{message}");
    }

    #[test]
    #[cfg(feature = "directml-provider")]
    fn directml_with_feature_resolves_or_reports_a_runtime_reason() {
        match super::resolve_execution_providers(
            &ProviderPreference::DirectMl { device_id: 0 },
            false,
            false,
        ) {
            Ok(chain) => {
                assert!(
                    !chain.providers.is_empty(),
                    "chain must contain CPU fallback"
                );
            }
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("unavailable in the loaded ONNX Runtime"),
                    "a runtime failure must say the runtime is unavailable: {message}"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "cuda-provider")]
    fn cuda_with_feature_resolves_or_reports_a_runtime_reason() {
        match super::resolve_execution_providers(
            &ProviderPreference::Cuda { device_id: 0 },
            false,
            false,
        ) {
            Ok(chain) => assert!(!chain.providers.is_empty()),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("unavailable in the loaded ONNX Runtime"),
                    "a runtime failure must say the runtime is unavailable: {message}"
                );
            }
        }
    }
}
