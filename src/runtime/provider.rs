#[cfg(feature = "cann-provider")]
use ort::ep::CANN;
#[cfg(feature = "cuda-provider")]
use ort::ep::CUDA;
#[cfg(feature = "directml-provider")]
use ort::ep::DirectML;
#[cfg(any(
    feature = "cuda-provider",
    feature = "directml-provider",
    feature = "cann-provider"
))]
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
    Cann,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderResolution {
    pub requested: ProviderPreference,
    pub resolved: ResolvedExecutionProvider,
    pub fallback_used: bool,
}

#[derive(Debug)]
pub struct ProviderChain {
    pub providers: Vec<ExecutionProviderDispatch>,
    pub resolution: ProviderResolution,
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
                resolve_cuda_execution_providers(
                    *device_id,
                    cpu_provider(),
                    fail_if_provider_unavailable,
                )
            }
            #[cfg(not(feature = "cuda-provider"))]
            {
                let _ = (device_id, fail_if_provider_unavailable);
                Err(RapidOcrError::UnsupportedProvider(
                    "CUDA provider is disabled; enable `cuda-provider`".into(),
                ))
            }
        }
        ProviderPreference::DirectMl { device_id } => {
            #[cfg(feature = "directml-provider")]
            {
                resolve_directml_execution_providers(
                    *device_id,
                    cpu_provider(),
                    fail_if_provider_unavailable,
                )
            }
            #[cfg(not(feature = "directml-provider"))]
            {
                let _ = (device_id, fail_if_provider_unavailable);
                Err(RapidOcrError::UnsupportedProvider(
                    "DirectML is only available on Windows".into(),
                ))
            }
        }
        ProviderPreference::Cann { device_id } => {
            #[cfg(feature = "cann-provider")]
            {
                resolve_cann_execution_providers(
                    *device_id,
                    cpu_provider(),
                    fail_if_provider_unavailable,
                )
            }
            #[cfg(not(feature = "cann-provider"))]
            {
                let _ = (device_id, fail_if_provider_unavailable);
                Err(RapidOcrError::UnsupportedProvider(
                    "CANN provider is disabled; enable `cann-provider`".into(),
                ))
            }
        }
    }
}

#[cfg(any(
    feature = "cuda-provider",
    feature = "directml-provider",
    feature = "cann-provider"
))]
fn device_id_to_i32(provider_name: &str, device_id: usize) -> Result<i32> {
    i32::try_from(device_id).map_err(|_| {
        RapidOcrError::Config(format!(
            "invalid {provider_name} device_id {device_id}: value exceeds i32 range"
        ))
    })
}

#[cfg(feature = "cuda-provider")]
fn resolve_cuda_execution_providers(
    device_id: usize,
    cpu_provider: ExecutionProviderDispatch,
    fail_if_provider_unavailable: bool,
) -> Result<ProviderChain> {
    resolve_accelerator_execution_providers(
        "CUDA",
        ProviderPreference::Cuda { device_id },
        ResolvedExecutionProvider::Cuda,
        device_id,
        cpu_provider,
        fail_if_provider_unavailable,
        |id| {
            let provider = CUDA::default().with_device_id(id);
            Ok((
                provider
                    .is_available()
                    .map_err(|e| RapidOcrError::Config(e.to_string()))?,
                provider.build(),
            ))
        },
    )
}

#[cfg(feature = "directml-provider")]
fn resolve_directml_execution_providers(
    device_id: usize,
    cpu_provider: ExecutionProviderDispatch,
    fail_if_provider_unavailable: bool,
) -> Result<ProviderChain> {
    resolve_accelerator_execution_providers(
        "DirectML",
        ProviderPreference::DirectMl { device_id },
        ResolvedExecutionProvider::DirectMl,
        device_id,
        cpu_provider,
        fail_if_provider_unavailable,
        |id| {
            let provider = DirectML::default().with_device_id(id);
            Ok((
                provider
                    .is_available()
                    .map_err(|e| RapidOcrError::Config(e.to_string()))?,
                provider.build(),
            ))
        },
    )
}

#[cfg(feature = "cann-provider")]
fn resolve_cann_execution_providers(
    device_id: usize,
    cpu_provider: ExecutionProviderDispatch,
    fail_if_provider_unavailable: bool,
) -> Result<ProviderChain> {
    resolve_accelerator_execution_providers(
        "CANN",
        ProviderPreference::Cann { device_id },
        ResolvedExecutionProvider::Cann,
        device_id,
        cpu_provider,
        fail_if_provider_unavailable,
        |id| {
            let provider = CANN::default().with_device_id(id);
            Ok((
                provider
                    .is_available()
                    .map_err(|e| RapidOcrError::Config(e.to_string()))?,
                provider.build(),
            ))
        },
    )
}

#[cfg(any(
    feature = "cuda-provider",
    feature = "directml-provider",
    feature = "cann-provider"
))]
fn resolve_accelerator_execution_providers<F>(
    provider_name: &str,
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

#[cfg(any(
    feature = "cuda-provider",
    feature = "directml-provider",
    feature = "cann-provider"
))]
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
        return Err(RapidOcrError::Config(format!(
            "requested execution provider {} is unavailable and fail_if_provider_unavailable=true",
            format_provider_preference(requested)
        )));
    }

    Ok(ProviderResolution {
        requested,
        resolved: ResolvedExecutionProvider::Cpu,
        fallback_used: true,
    })
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
        ProviderPreference::Cann { device_id } => format!("cann(device_id={device_id})"),
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderResolution, require_requested_provider};
    #[cfg(any(
        feature = "cuda-provider",
        feature = "directml-provider",
        feature = "cann-provider"
    ))]
    use super::{
        ResolvedExecutionProvider, decide_provider_resolution, resolve_execution_providers,
    };
    #[cfg(any(
        feature = "cuda-provider",
        feature = "directml-provider",
        feature = "cann-provider"
    ))]
    use crate::config::ProviderPreference;
    use crate::config::ProviderPreference as AnyProviderPreference;
    use crate::runtime::provider::ResolvedExecutionProvider as AnyResolvedExecutionProvider;

    #[test]
    #[cfg(feature = "directml-provider")]
    fn directml_preference_has_cpu_fallback() {
        let providers = resolve_execution_providers(
            &ProviderPreference::DirectMl { device_id: 0 },
            false,
            false,
        )
        .expect("provider resolution should not fail");
        assert!(
            !providers.providers.is_empty(),
            "provider chain must contain at least CPU fallback"
        );
    }

    #[test]
    #[cfg(feature = "cuda-provider")]
    fn cuda_preference_has_cpu_fallback() {
        let providers =
            resolve_execution_providers(&ProviderPreference::Cuda { device_id: 0 }, false, false)
                .expect("provider resolution should not fail");
        assert!(
            !providers.providers.is_empty(),
            "provider chain must contain at least CPU fallback"
        );
    }

    #[test]
    #[cfg(feature = "cann-provider")]
    fn cann_preference_has_cpu_fallback() {
        let providers =
            resolve_execution_providers(&ProviderPreference::Cann { device_id: 0 }, false, false)
                .expect("provider resolution should not fail");
        assert!(
            !providers.providers.is_empty(),
            "provider chain must contain at least CPU fallback"
        );
    }

    #[test]
    #[cfg(any(
        feature = "cuda-provider",
        feature = "directml-provider",
        feature = "cann-provider"
    ))]
    fn strict_mode_errors_when_provider_is_unavailable() {
        let err = decide_provider_resolution(
            ProviderPreference::Cuda { device_id: 0 },
            ResolvedExecutionProvider::Cuda,
            false,
            true,
        )
        .expect_err("strict mode should reject unavailable provider");
        assert!(
            err.to_string()
                .contains("fail_if_provider_unavailable=true")
        );
    }

    #[test]
    #[cfg(any(
        feature = "cuda-provider",
        feature = "directml-provider",
        feature = "cann-provider"
    ))]
    fn non_strict_mode_falls_back_to_cpu_when_provider_is_unavailable() {
        let resolution = decide_provider_resolution(
            ProviderPreference::DirectMl { device_id: 2 },
            ResolvedExecutionProvider::DirectMl,
            false,
            false,
        )
        .expect("fallback should succeed");
        assert!(resolution.fallback_used);
        assert_eq!(resolution.resolved, ResolvedExecutionProvider::Cpu);
    }

    #[test]
    fn require_requested_provider_rejects_silent_cpu_fallback() {
        let resolution = ProviderResolution {
            requested: AnyProviderPreference::Cuda { device_id: 0 },
            resolved: AnyResolvedExecutionProvider::Cpu,
            fallback_used: true,
        };
        let error = require_requested_provider(resolution)
            .expect_err("fallback must be rejected regardless of process-wide policy");
        assert!(
            matches!(error, crate::error::RapidOcrError::UnsupportedProvider(_)),
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
            requested: AnyProviderPreference::DirectMl { device_id: 1 },
            resolved: AnyResolvedExecutionProvider::DirectMl,
            fallback_used: false,
        };
        assert_eq!(
            require_requested_provider(accelerated)
                .expect("resolved provider must pass")
                .resolved,
            AnyResolvedExecutionProvider::DirectMl
        );

        let cpu = ProviderResolution {
            requested: AnyProviderPreference::Cpu,
            resolved: AnyResolvedExecutionProvider::Cpu,
            fallback_used: false,
        };
        assert!(
            require_requested_provider(cpu).is_ok(),
            "explicit CPU must always pass"
        );
    }
}
