//! 已加载 / 已链接 ONNX Runtime 的**指纹**：API 版本、运行库文件身份（路径、体积、SHA-256）
//! 与 provider DLL 集合。
//!
//! # 为什么需要指纹而不是一个版本字符串
//!
//! 本 crate 的 ORT 链接方式是 `rustc-link-lib=static=onnxruntime`（见
//! `target/<profile>/build/ort-sys-*/output`）：**ONNX Runtime 被静态链接进可执行文件**，
//! 进程里不存在 `onnxruntime.dll` 模块。这一点本身就是阶段 0 记录里的错误来源：
//!
//! - 报告写“加载 ORT 1.17，来自 `System32\onnxruntime.dll`”；
//! - 实际 `C:\Windows\system32\onnxruntime.dll` 只是 Windows 自带的同名文件，
//!   **从未被加载**（`GetModuleHandleW("onnxruntime.dll")` 返回 0 / `GetLastError` 126）；
//!   它的文件版本 1.17.x 与运行库自报的 API 版本 `1.28.0` 也不一致；
//! - 真正决定行为的是 ort 缓存里那份 `onnxruntime.lib`（两个已知分发分别是
//!   `341,152,186` 与 `363,923,500` 字节）。
//!
//! 因此这里把“到底是哪一份 ORT”变成可复核的指纹：API 版本字符串、运行库文件全路径、
//! 体积、SHA-256，以及 exe 旁边的 provider DLL（`DirectML.dll`、
//! `onnxruntime_providers_cuda.dll` 等）。
//!
//! # 运行库文件的确定顺序（每一步都可复核，不编造）
//!
//! 1. **动态加载的模块**：如果进程里真的加载了 `onnxruntime.dll`
//!    （用户把 DLL 放在 exe 旁、或将来换成 `load-dynamic`），用 `GetModuleHandleW` +
//!    `GetModuleFileNameW` 取真实路径；
//! 2. **静态链接的库文件**：否则用 `build.rs` 记录的缓存目录 + 体积，在缓存里定位那份
//!    `onnxruntime.lib` 并计算 SHA-256；
//! 3. **可执行文件自身**：再否则退回 exe 自身（ORT 确实被链接进它），并明确标注
//!    `runtime_source = "executable"`，让读者知道“这份哈希证明的是哪个二进制”。
//!
//! 任何一步失败都不静默：失败原因写在 [`OrtRuntimeFingerprint::link_reason`] /
//! [`LoadedModule::reason`] 里，绝不填一个看起来合理但不是实测的值。
//!
//! # 与 build.rs 的接口
//!
//! 三个编译期环境变量名与 `build.rs` 里的常量必须一致（两处都写了注释互相指向）：
//! 运行期无法引用 build script 的 `const`，因此这里重复声明。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model_store::sha256_file;

/// 与 ONNX Runtime 一起加载、用于加速的 provider DLL 名称。
pub const PROVIDER_DLL_NAMES: &[&str] = &["DirectML.dll", "onnxruntime_providers_cuda.dll"];

// 与 `build.rs` 中同名常量保持一致（build script 的常量无法被 src 引用）。
// `option_env!` 只接受字面量，所以这里用字面量并在注释里标明对应关系：
// `RAPID_OCR_ORT_LINK_DIR` / `RAPID_OCR_ORT_LINK_NAME` / `RAPID_OCR_ORT_LINK_BYTES`
// 分别对应 `build.rs` 的 `LINK_LIB_BASE_ENV` / `LINK_LIB_NAME_ENV` / `LINK_LIB_SIZE_ENV`。

/// 一个运行库/模块的身份：路径 + 体积 + SHA-256。
///
/// `reason` 非空表示“这一项没读全”，调用方必须把它当作**未验证**而不是成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadedModule {
    /// 文件全路径。
    pub path: String,
    /// 文件体积（字节）。
    pub size_bytes: u64,
    /// 文件 SHA-256（64 位小写十六进制）；读取失败时为 `None`。
    pub sha256: Option<String>,
    /// 无法完整读取时的可定位原因；成功时为 `None`。
    pub reason: Option<String>,
}

impl LoadedModule {
    /// 路径存在 + 体积为正 + 64 位十六进制哈希，三者齐全才算“已完整指纹化”。
    pub fn is_fingerprinted(&self) -> bool {
        self.size_bytes > 0
            && self.sha256.as_deref().is_some_and(is_sha256_hex)
            && self.reason.is_none()
    }
}

/// 编译期记录的静态链接信息（`build.rs` 从 ort-sys 的构建输出捕获）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrtLinkInfo {
    /// ort 缓存里分发目录的公共父目录。
    pub base_dir: String,
    /// 被链接的静态库文件名（通常是 `onnxruntime.lib`）。
    pub file_name: String,
    /// 被链接的静态库字节数（用来在缓存里唯一定位候选）。
    pub size_bytes: u64,
}

impl OrtLinkInfo {
    /// 读取编译期记录；没有记录（例如未触发 ort-sys 构建脚本）时返回 `None`。
    pub fn from_build_env() -> Option<Self> {
        let base_dir = option_env!("RAPID_OCR_ORT_LINK_DIR")?;
        let file_name = option_env!("RAPID_OCR_ORT_LINK_NAME")?;
        let size_bytes: u64 = option_env!("RAPID_OCR_ORT_LINK_BYTES")?.parse().ok()?;
        Some(Self {
            base_dir: base_dir.to_string(),
            file_name: file_name.to_string(),
            size_bytes,
        })
    }

    /// 在缓存里按“文件名 + 体积”定位被链接的那份库文件。
    pub fn locate(&self) -> Result<PathBuf, String> {
        let base = PathBuf::from(&self.base_dir);
        let entries = std::fs::read_dir(&base).map_err(|error| {
            format!(
                "read_dir({}) failed: {error}; the ort binary cache recorded at build time is not \
                 readable",
                base.display()
            )
        })?;
        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("read_dir entry failed: {error}"))?;
            let candidate = entry.path().join(&self.file_name);
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            return Err(format!(
                "no `{}` found under {} ({} entries scanned)",
                self.file_name,
                base.display(),
                std::fs::read_dir(&base).map(|it| it.count()).unwrap_or(0)
            ));
        }
        let mut observed: Vec<(PathBuf, u64)> = Vec::new();
        for candidate in &candidates {
            let size = std::fs::metadata(candidate)
                .map(|metadata| metadata.len())
                .map_err(|error| format!("metadata({}) failed: {error}", candidate.display()))?;
            if size == self.size_bytes {
                return Ok(candidate.clone());
            }
            observed.push((candidate.clone(), size));
        }
        Err(format!(
            "the linked `{}` ({} bytes, recorded at build time) was not found in {}; candidates: \
             {}",
            self.file_name,
            self.size_bytes,
            base.display(),
            observed
                .iter()
                .map(|(path, size)| format!("{}={size}", path.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// exe 旁边（或已加载）的一个 provider DLL。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDll {
    pub name: String,
    pub path: String,
    /// 文件体积；读取失败时为 `None`。
    pub size_bytes: Option<u64>,
    /// 该模块当前是否已在进程里加载（`GetModuleHandleW` 命中）。
    pub loaded: bool,
}

/// 进程当前使用的 ONNX Runtime 指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrtRuntimeFingerprint {
    /// `OrtGetApiBase()->GetVersionString()`；查询失败时为 `None`。
    pub api_version: Option<String>,
    /// 运行库文件身份的来源：`loaded_module` / `static_link` / `executable`。
    pub runtime_source: String,
    /// 运行库文件（见 [`OrtRuntimeFingerprint`] 文档里的确定顺序）。
    pub runtime_module: LoadedModule,
    /// 编译期记录的静态链接信息；动态加载或没有记录时为 `None`。
    pub link: Option<OrtLinkInfo>,
    /// 无法把运行库指纹化时的可定位原因。
    pub link_reason: Option<String>,
    /// exe 旁边的 provider DLL（按名称去重排序）。
    pub provider_dlls: Vec<ProviderDll>,
    /// 枚举 provider DLL 失败时的可定位原因。
    pub provider_reason: Option<String>,
}

impl OrtRuntimeFingerprint {
    /// 指纹是否完整：版本 + 运行库路径 + 正体积 + 64 位十六进制哈希。
    pub fn is_complete(&self) -> bool {
        self.api_version
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && self.runtime_module.is_fingerprinted()
    }

    /// 一行摘要，便于写进 human-readable 报告。
    pub fn summary(&self) -> String {
        format!(
            "api={} source={} path={} bytes={} sha256={}",
            self.api_version.as_deref().unwrap_or("<unknown>"),
            self.runtime_source,
            self.runtime_module.path,
            self.runtime_module.size_bytes,
            self.runtime_module.sha256.as_deref().unwrap_or("<unknown>")
        )
    }
}

/// 判断字符串是不是 64 位十六进制（SHA-256 的十六进制形式）。
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 采集当前进程的 ONNX Runtime 指纹。
///
/// 这个函数**不返回 `Result`**：报告需要的是“哪一项读不到、为什么”，而不是让整个基准
/// 因为指纹采集失败而中断。失败信息写在字段与 `reason` 里，调用方据此判定报告是否可信
/// （[`OrtRuntimeFingerprint::is_complete`]）。
pub fn ort_runtime_fingerprint() -> OrtRuntimeFingerprint {
    let api_version =
        crate::runtime::provider::ort_runtime_version().filter(|value| !value.trim().is_empty());

    // 1) 进程里真的加载了 onnxruntime.dll 吗？
    let loaded_module = module_file_name("onnxruntime.dll").ok().map(|path| {
        let module = fingerprint_file(path);
        ("loaded_module".to_string(), module)
    });

    let link = OrtLinkInfo::from_build_env();
    // 2) 静态链接：按 build.rs 记录定位缓存里的 onnxruntime.lib。
    let (static_module, static_reason) = match link.as_ref() {
        Some(info) => match info.locate() {
            Ok(path) => (Some(fingerprint_file(path)), None),
            Err(reason) => (None, Some(reason)),
        },
        None => (None, None),
    };

    // 3) 兜底：可执行文件自身（ORT 确实被链接进它）。
    let executable = std::env::current_exe().map(fingerprint_file).ok();

    let mut link_reason: Option<String> = None;
    let (runtime_source, runtime_module) = if let Some((source, module)) = loaded_module {
        (source, module)
    } else if let Some(module) = static_module {
        ("static_link".to_string(), module)
    } else if let Some(executable) = executable {
        // 静态链接记录缺失（或定位失败）时退回 exe，并把原因带上。
        link_reason = Some(static_reason.unwrap_or_else(|| {
            if link.is_none() {
                "no build-time ORT link information was recorded; the runtime is statically \
                 linked into the executable, so the fingerprint falls back to the executable file"
                    .to_string()
            } else {
                "the linked static library could not be located; the fingerprint falls back to the \
                 executable file, which still identifies the process that linked it"
                    .to_string()
            }
        }));
        ("executable".to_string(), executable)
    } else {
        (
            "unavailable".to_string(),
            LoadedModule {
                path: String::new(),
                size_bytes: 0,
                sha256: None,
                reason: Some(
                    "neither a loaded onnxruntime.dll, the linked static library recorded at build \
                     time, nor the current executable could be resolved"
                        .to_string(),
                ),
            },
        )
    };

    let (provider_dlls, provider_reason) = collect_provider_dlls();
    OrtRuntimeFingerprint {
        api_version,
        runtime_source,
        runtime_module,
        link,
        link_reason,
        provider_dlls,
        provider_reason,
    }
}

/// 给一个已经确定的路径算体积与哈希。
fn fingerprint_file(path: PathBuf) -> LoadedModule {
    let display = path.display().to_string();
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return LoadedModule {
                path: display.clone(),
                size_bytes: 0,
                sha256: None,
                reason: Some(format!("metadata({display}) failed: {error}")),
            };
        }
    };
    if !metadata.is_file() {
        return LoadedModule {
            path: display.clone(),
            size_bytes: metadata.len(),
            sha256: None,
            reason: Some(format!("{display} is not a regular file")),
        };
    }
    let (sha256, reason) = match sha256_file(&path) {
        Ok(hash) => (Some(hash), None),
        Err(error) => (
            None,
            Some(format!("sha256_file({display}) failed: {error}")),
        ),
    };
    LoadedModule {
        path: display,
        size_bytes: metadata.len(),
        sha256,
        reason,
    }
}

/// 在“exe 目录 → 当前目录”里找 provider DLL，并记录是否已被加载。
///
/// provider DLL 不保证被加载（CPU-only 构建里 `DirectML.dll` 旁边可能根本不存在），
/// 所以按**磁盘存在性**枚举，并额外标注是否已加载。
fn collect_provider_dlls() -> (Vec<ProviderDll>, Option<String>) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut reason: Option<String> = None;
    match std::env::current_exe() {
        Ok(exe) => {
            if let Some(dir) = exe.parent() {
                roots.push(dir.to_path_buf());
            }
        }
        Err(error) => reason = Some(format!("current_exe() failed: {error}")),
    }
    if let Ok(cwd) = std::env::current_dir()
        && !roots.contains(&cwd)
    {
        roots.push(cwd);
    }

    let mut found: Vec<ProviderDll> = Vec::new();
    for name in PROVIDER_DLL_NAMES {
        let loaded = module_file_name(name).is_ok();
        let mut on_disk: Option<PathBuf> = None;
        for root in &roots {
            let candidate = root.join(name);
            if candidate.is_file() {
                on_disk = Some(candidate);
                break;
            }
        }
        let resolved = on_disk.or_else(|| module_file_name(name).ok());
        let Some(path) = resolved else {
            continue;
        };
        let size_bytes = std::fs::metadata(&path).ok().map(|metadata| metadata.len());
        found.push(ProviderDll {
            name: (*name).to_string(),
            path: path.display().to_string(),
            size_bytes,
            loaded,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    (found, reason)
}

/// 通过 `GetModuleHandleW` + `GetModuleFileNameW` 取已加载模块的全路径。
///
/// 未加载时返回可定位的原因（含 `GetLastError`），不返回伪造路径。
fn module_file_name(dll_name: &str) -> Result<PathBuf, String> {
    let wide: Vec<u16> = dll_name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` 是以 NUL 结尾的 UTF-16 缓冲区；`GetModuleHandleW` 只读取它。
    let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
    if handle.is_null() {
        // SAFETY: `GetLastError` 没有前置条件，且紧跟失败的调用读取。
        let code = unsafe { GetLastError() };
        return Err(format!(
            "GetModuleHandleW(\"{dll_name}\") failed with Win32 error {code}: the module is not \
             loaded in this process"
        ));
    }
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: `buffer` 是可写的 UTF-16 缓冲区，长度以 u16 元素个数传入。
    let written = unsafe {
        GetModuleFileNameW(
            handle,
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };
    if written == 0 {
        // SAFETY: 同上，紧跟失败的调用。
        let code = unsafe { GetLastError() };
        return Err(format!(
            "GetModuleFileNameW(\"{dll_name}\") failed with Win32 error {code}"
        ));
    }
    let written = written as usize;
    if written >= buffer.len() {
        return Err(format!(
            "GetModuleFileNameW(\"{dll_name}\") truncated: the path needs more than {} UTF-16 \
             units",
            buffer.len()
        ));
    }
    let path = String::from_utf16_lossy(&buffer[..written]);
    if path.is_empty() {
        return Err(format!(
            "GetModuleFileNameW(\"{dll_name}\") returned an empty path"
        ));
    }
    Ok(PathBuf::from(path))
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module_name: *const u16) -> *mut core::ffi::c_void;
    fn GetModuleFileNameW(
        module: *mut core::ffi::c_void,
        file_name: *mut u16,
        capacity: u32,
    ) -> u32;
    fn GetLastError() -> u32;
}

/// 报告里必须能写清“实际用的是哪一份 ONNX Runtime”，并且**不能悄悄为空**。
///
/// 允许两种结果：完整指纹（版本 + 存在路径 + 正体积 + 64 位十六进制 SHA-256），
/// 或带可定位原因的失败。两种之外的一切（例如空字段又没有原因）都是缺陷。
#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{OrtLinkInfo, is_sha256_hex, ort_runtime_fingerprint};

    #[test]
    fn fingerprint_is_either_complete_or_explains_itself() {
        let fingerprint = ort_runtime_fingerprint();
        if fingerprint.is_complete() {
            assert_fingerprint_fields(&fingerprint);
            return;
        }

        // 不完整时必须有可定位原因，且原因必须说出到底哪一步失败。
        let reason = fingerprint
            .runtime_module
            .reason
            .clone()
            .or_else(|| fingerprint.link_reason.clone())
            .expect("an incomplete fingerprint must carry a locating reason");
        assert!(
            !reason.trim().is_empty(),
            "the locating reason must not be blank"
        );
        let missing = fingerprint.api_version.is_none()
            || fingerprint.runtime_module.size_bytes == 0
            || fingerprint.runtime_module.sha256.is_none();
        assert!(
            missing,
            "is_complete() reported false but every field looks present: {fingerprint:?}"
        );
        eprintln!(
            "ONNX Runtime fingerprint is incomplete in this environment (reason: {reason}); \
             fields were not fabricated"
        );
    }

    fn assert_fingerprint_fields(fingerprint: &super::OrtRuntimeFingerprint) {
        let version = fingerprint
            .api_version
            .as_deref()
            .expect("complete fingerprint has a version");
        assert!(
            !version.trim().is_empty(),
            "the version string must not be blank"
        );
        assert!(
            Path::new(&fingerprint.runtime_module.path).exists(),
            "the reported runtime path must exist: {}",
            fingerprint.runtime_module.path
        );
        assert!(
            fingerprint.runtime_module.size_bytes > 0,
            "the runtime module must not be empty"
        );
        let sha = fingerprint
            .runtime_module
            .sha256
            .as_deref()
            .expect("complete fingerprint has a hash");
        assert!(
            is_sha256_hex(sha),
            "the hash must be 64 hex characters, got {sha:?}"
        );
        assert!(
            !fingerprint.runtime_source.is_empty(),
            "the report must say where the runtime identity came from"
        );
        eprintln!("ONNX Runtime fingerprint: {}", fingerprint.summary());
        for dll in &fingerprint.provider_dlls {
            eprintln!(
                "  provider dll: {} bytes={:?} loaded={}",
                dll.path, dll.size_bytes, dll.loaded
            );
        }
    }

    /// 指纹必须能被基准报告内嵌（JSON 往返）。
    #[test]
    fn fingerprint_serialises_for_reports() {
        let fingerprint = ort_runtime_fingerprint();
        let value = serde_json::to_value(&fingerprint).expect("fingerprint must serialise");
        for field in [
            "api_version",
            "runtime_source",
            "runtime_module",
            "link",
            "provider_dlls",
        ] {
            assert!(
                value.get(field).is_some(),
                "the report must carry the `{field}` field"
            );
        }
        let round_trip: super::OrtRuntimeFingerprint =
            serde_json::from_value(value).expect("fingerprint must deserialise");
        assert_eq!(round_trip, fingerprint);
    }

    /// 十六进制判定本身要有明确的边界，避免“长度对就当哈希”。
    #[test]
    fn sha256_hex_detection_is_strict() {
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(is_sha256_hex(&"0123456789abcdef".repeat(4)));
        assert!(!is_sha256_hex(&"a".repeat(63)));
        assert!(!is_sha256_hex(&"a".repeat(65)));
        assert!(!is_sha256_hex(&"z".repeat(64)));
        assert!(!is_sha256_hex(""));
    }

    /// 构建期记录必须存在且能定位到真实的静态库文件：否则报告只能退回 exe，
    /// 而那说明“哪一份 ORT 被链接进来”这件事没有被记录下来。
    #[test]
    fn build_time_link_information_locates_the_static_library() {
        let Some(info) = OrtLinkInfo::from_build_env() else {
            panic!(
                "no build-time ORT link information was recorded; build.rs must capture \
                 DEP_ORT_SYS_LINK so the fingerprint can name the linked static library"
            );
        };
        assert!(info.size_bytes > 0, "the linked library must be non-empty");
        assert!(
            info.file_name.ends_with(".lib"),
            "expected a static import library, got {:?}",
            info.file_name
        );
        let path = info.locate().unwrap_or_else(|reason| {
            panic!("the linked static library must be locatable: {reason}")
        });
        let size = std::fs::metadata(&path)
            .expect("the located library must be readable")
            .len();
        assert_eq!(
            size, info.size_bytes,
            "the located library size must match the build-time record"
        );
    }
}
