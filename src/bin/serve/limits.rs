//! serve 的资源上限、队列参数与存储参数的**唯一**解析与校验实现。
//!
//! 对应文档：§3（CLI 与默认值）、§4.4（请求体上限）、§4.5（保留数与字节）、
//! §4.6（结果序列化上限）、§6.2（单文件下载上限）、§8.3（双队列与连续配额）。
//!
//! 规则（M0 冻结的一部分）：
//!
//! 1. `--max-*-mb` 一律按 **MiB** 解释，换算成字节只在这里做一次：
//!    `mib * MIB`，用 `checked_mul` **绝不 wrap**；
//! 2. 上限为 0 一律非法（0 字节上限等于"服务不可用"，是配置错误而不是"关闭限制"）；
//! 3. 队列容量、连续配额、保留数、tombstone 上限、TTL 都必须 ≥ 1：
//!    0 会让 §8.3 的"每个非空队列每轮至少服务一次"与 §4.5 的 404/410 语义
//!    直接失去意义，因此必须在启动期拒绝，而不是运行期表现成随机行为；
//! 4. 每个错误都带**字段名**（`--max-body-mb` 这样的可定位文本）与实际取值。

use std::fmt;

/// 1 MiB。文档里所有 `--max-*-mb` 的唯一换算因子。
pub const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// 文档 §3 规定的默认值：唯一的默认值出处。
// CLI（clap 的 `default_value_t`）与校验（[`RawServeLimits::default`]）都引用这里，
// 因此"文档默认值"只可能有一处实现；`serve::cli` 的测试逐项对照文档断言这些常量。
// ---------------------------------------------------------------------------

/// `--port` 默认端口。占用时报可定位错误，**不静默换端口**（§3）。
pub const DEFAULT_PORT: u16 = 8760;
/// `--max-body-mb`：请求体上限，默认 32 MiB。
pub const DEFAULT_MAX_BODY_MB: u64 = 32;
/// `--max-result-mb`：单个结果序列化上限，默认 8 MiB（§4.6）。
pub const DEFAULT_MAX_RESULT_MB: u64 = 8;
/// `--max-export-mb`：单个导出文档上限（含内嵌图片），默认 32 MiB（§9.5）。
pub const DEFAULT_MAX_EXPORT_MB: u64 = 32;
/// `--max-download-mb`：单文件下载上限，默认 1024 MiB（必须大于 566 MB 公式模型，§6.2）。
///
/// **唯一来源是库常量**：库内调用方（`EngineConfig::allow_download` 分支）也要用同一个默认
/// 上限，因此这里引用 `rapid_ocr_rs::DEFAULT_MAX_DOWNLOAD_MB`，而不是再写一遍 1024。
pub const DEFAULT_MAX_DOWNLOAD_MB: u64 = rapid_ocr_rs::DEFAULT_MAX_DOWNLOAD_MB;
/// `--max-queue-text`：普通 OCR 队列上限，默认 4。
pub const DEFAULT_MAX_QUEUE_TEXT: usize = 4;
/// `--max-queue-formula`：公式 OCR 队列上限，默认 2。
pub const DEFAULT_MAX_QUEUE_FORMULA: usize = 2;
/// `--max-consecutive-text`：普通任务连续处理上限，默认 4（保公式不被饿死，§8.3）。
pub const DEFAULT_MAX_CONSECUTIVE_TEXT: usize = 4;
/// `--max-consecutive-formula`：公式任务连续处理上限，默认 1（保普通 OCR 不被饿死，§8.3）。
pub const DEFAULT_MAX_CONSECUTIVE_FORMULA: usize = 1;
/// `--max-retained`：终态任务保留数上限，默认 32。
pub const DEFAULT_MAX_RETAINED: usize = 32;
/// `--max-retained-mb`：终态任务占用字节上限，默认 64 MiB。
pub const DEFAULT_MAX_RETAINED_MB: u64 = 64;
/// `--max-tombstones`：已淘汰任务 ID 记录上限，默认 256（§4.5）。
pub const DEFAULT_MAX_TOMBSTONES: usize = 256;
/// `--job-ttl-secs`：终态任务与 tombstone 保留时长，默认 600 s。
pub const DEFAULT_JOB_TTL_SECS: u64 = 600;
/// `--allow-download` 默认关闭（§0.1：模型下载默认关闭、显式触发）。
pub const DEFAULT_ALLOW_DOWNLOAD: bool = false;
/// `--allow-provider-fallback` 默认关闭（§7.5：非 cpu provider 默认强制不回退）。
pub const DEFAULT_ALLOW_PROVIDER_FALLBACK: bool = false;
/// 内置默认 provider（§3 的 `--provider` 默认 `cpu`）。
pub const DEFAULT_PROVIDER: &str = "cpu";

/// 可定位的 serve 配置错误。
///
/// `field` 是**用户实际敲的那个开关名**（`--max-body-mb`），`value` 是原值，
/// `reason` 说明为什么非法。M1 直接打印这个 `Display` 即可满足"报可定位错误"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeConfigError {
    field: &'static str,
    value: String,
    reason: String,
}

impl ServeConfigError {
    /// 兄弟模块（调度、任务存储）复用同一套措辞，因此构造器是 crate 可见的。
    pub(crate) fn new(
        field: &'static str,
        value: impl fmt::Display,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            field,
            value: value.to_string(),
            reason: reason.into(),
        }
    }

    /// 出错字段（CLI 开关名）。
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// 失败原因（不含字段名）。
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ServeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}={}: {}", self.field, self.value, self.reason)
    }
}

impl std::error::Error for ServeConfigError {}

/// 配置校验结果。
pub type ServeConfigResult<T> = Result<T, ServeConfigError>;

/// `--max-*-mb` 的 MiB → 字节换算（**唯一**实现）。
///
/// 0、以及任何乘 1 MiB 后溢出 `u64` 的输入都返回可定位错误，不做回绕、不做截断。
fn mib_to_bytes(field: &'static str, mib: u64) -> ServeConfigResult<u64> {
    if mib == 0 {
        return Err(ServeConfigError::new(
            field,
            mib,
            "the limit must be greater than zero (MiB)",
        ));
    }
    mib.checked_mul(MIB).ok_or_else(|| {
        ServeConfigError::new(
            field,
            mib,
            format!("{mib} MiB overflows the byte limit (u64): {mib} * {MIB} does not fit"),
        )
    })
}

/// 取值必须 ≥ 1 的统一校验（队列容量、连续配额、保留数、tombstone 上限共用）。
pub(crate) fn require_positive_usize(
    field: &'static str,
    value: usize,
) -> ServeConfigResult<usize> {
    if value == 0 {
        return Err(ServeConfigError::new(
            field,
            value,
            "the value must be at least 1",
        ));
    }
    Ok(value)
}

/// 取值为毫秒/字节且必须 ≥ 1 的统一校验（TTL、保留字节共用）。
pub(crate) fn require_positive_u64(field: &'static str, value: u64) -> ServeConfigResult<u64> {
    if value == 0 {
        return Err(ServeConfigError::new(
            field,
            value,
            "the value must be at least 1",
        ));
    }
    Ok(value)
}

/// CLI 上的**原始**取值（MiB / 秒 / 个数），字段与 `ServeArgs` 一一对应。
///
/// 只有 [`RawServeLimits::validate`] 能把它变成 [`ServeLimits`]，
/// 因此运行期代码拿到的 `ServeLimits` 一定是合法值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawServeLimits {
    pub max_body_mb: u64,
    pub max_result_mb: u64,
    pub max_export_mb: u64,
    pub max_download_mb: u64,
    pub max_queue_text: usize,
    pub max_queue_formula: usize,
    pub max_consecutive_text: usize,
    pub max_consecutive_formula: usize,
    pub max_retained: usize,
    pub max_retained_mb: u64,
    pub max_tombstones: usize,
    pub job_ttl_secs: u64,
}

impl Default for RawServeLimits {
    /// 文档 §3 的默认值（**不是**全 0 的 `derive(Default)`）。
    fn default() -> Self {
        Self {
            max_body_mb: DEFAULT_MAX_BODY_MB,
            max_result_mb: DEFAULT_MAX_RESULT_MB,
            max_export_mb: DEFAULT_MAX_EXPORT_MB,
            max_download_mb: DEFAULT_MAX_DOWNLOAD_MB,
            max_queue_text: DEFAULT_MAX_QUEUE_TEXT,
            max_queue_formula: DEFAULT_MAX_QUEUE_FORMULA,
            max_consecutive_text: DEFAULT_MAX_CONSECUTIVE_TEXT,
            max_consecutive_formula: DEFAULT_MAX_CONSECUTIVE_FORMULA,
            max_retained: DEFAULT_MAX_RETAINED,
            max_retained_mb: DEFAULT_MAX_RETAINED_MB,
            max_tombstones: DEFAULT_MAX_TOMBSTONES,
            job_ttl_secs: DEFAULT_JOB_TTL_SECS,
        }
    }
}

impl RawServeLimits {
    /// 校验并换算成运行期上限。字段错误一定带着出错的开关名。
    pub fn validate(self) -> ServeConfigResult<ServeLimits> {
        Ok(ServeLimits {
            max_body_bytes: mib_to_bytes("--max-body-mb", self.max_body_mb)?,
            max_result_bytes: mib_to_bytes("--max-result-mb", self.max_result_mb)?,
            max_export_bytes: mib_to_bytes("--max-export-mb", self.max_export_mb)?,
            max_download_bytes: mib_to_bytes("--max-download-mb", self.max_download_mb)?,
            max_queue_text: require_positive_usize("--max-queue-text", self.max_queue_text)?,
            max_queue_formula: require_positive_usize(
                "--max-queue-formula",
                self.max_queue_formula,
            )?,
            max_consecutive_text: require_positive_usize(
                "--max-consecutive-text",
                self.max_consecutive_text,
            )?,
            max_consecutive_formula: require_positive_usize(
                "--max-consecutive-formula",
                self.max_consecutive_formula,
            )?,
            max_retained: require_positive_usize("--max-retained", self.max_retained)?,
            max_retained_bytes: mib_to_bytes("--max-retained-mb", self.max_retained_mb)?,
            max_tombstones: require_positive_usize("--max-tombstones", self.max_tombstones)?,
            job_ttl_ms: require_positive_u64("--job-ttl-secs", self.job_ttl_secs)?
                .checked_mul(1000)
                .ok_or_else(|| {
                    ServeConfigError::new(
                        "--job-ttl-secs",
                        self.job_ttl_secs,
                        "overflows milliseconds (u64)",
                    )
                })?,
        })
    }
}

/// 运行期资源上限：**已经换算成字节**且已经校验过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeLimits {
    pub max_body_bytes: u64,
    pub max_result_bytes: u64,
    pub max_export_bytes: u64,
    pub max_download_bytes: u64,
    pub max_queue_text: usize,
    pub max_queue_formula: usize,
    pub max_consecutive_text: usize,
    pub max_consecutive_formula: usize,
    pub max_retained: usize,
    pub max_retained_bytes: u64,
    pub max_tombstones: usize,
    pub job_ttl_ms: u64,
}

impl ServeLimits {
    /// 文档 §3 的默认值（经同一套校验）。
    pub fn defaults() -> Self {
        RawServeLimits::default()
            .validate()
            .expect("the documented serve defaults must be valid")
    }

    /// 下载任务的预算：`--max-download-mb` 同时是**单文件上限**与**整批下载的总量额度**
    /// （§6.2）。多文件集合由 [`rapid_ocr_rs::download_model_set`] 按剩余额度递减。
    pub fn download_budget(&self) -> rapid_ocr_rs::DownloadBudget {
        rapid_ocr_rs::DownloadBudget::new(self.max_download_bytes)
    }
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_JOB_TTL_SECS, DEFAULT_MAX_BODY_MB, DEFAULT_MAX_CONSECUTIVE_FORMULA,
        DEFAULT_MAX_CONSECUTIVE_TEXT, DEFAULT_MAX_DOWNLOAD_MB, DEFAULT_MAX_EXPORT_MB,
        DEFAULT_MAX_QUEUE_FORMULA, DEFAULT_MAX_QUEUE_TEXT, DEFAULT_MAX_RESULT_MB,
        DEFAULT_MAX_RETAINED, DEFAULT_MAX_RETAINED_MB, DEFAULT_MAX_TOMBSTONES, DEFAULT_PORT, MIB,
        RawServeLimits, ServeLimits,
    };

    fn default_limits() -> ServeLimits {
        ServeLimits::defaults()
    }

    /// 文档 §3 的每一个默认值都在这里被逐项锁住（字节换算同时被验证）。
    #[test]
    fn documented_defaults_are_exactly_the_documented_values() {
        assert_eq!(DEFAULT_PORT, 8760);
        assert_eq!(DEFAULT_MAX_BODY_MB, 32);
        assert_eq!(DEFAULT_MAX_RESULT_MB, 8);
        assert_eq!(DEFAULT_MAX_EXPORT_MB, 32);
        assert_eq!(DEFAULT_MAX_DOWNLOAD_MB, 1024);
        assert_eq!(DEFAULT_MAX_QUEUE_TEXT, 4);
        assert_eq!(DEFAULT_MAX_QUEUE_FORMULA, 2);
        assert_eq!(DEFAULT_MAX_CONSECUTIVE_TEXT, 4);
        assert_eq!(DEFAULT_MAX_CONSECUTIVE_FORMULA, 1);
        assert_eq!(DEFAULT_MAX_RETAINED, 32);
        assert_eq!(DEFAULT_MAX_RETAINED_MB, 64);
        assert_eq!(DEFAULT_MAX_TOMBSTONES, 256);
        assert_eq!(DEFAULT_JOB_TTL_SECS, 600);

        let limits = default_limits();
        assert_eq!(limits.max_body_bytes, 32 * MIB);
        assert_eq!(limits.max_result_bytes, 8 * MIB);
        assert_eq!(limits.max_export_bytes, 32 * MIB);
        assert_eq!(limits.max_download_bytes, 1024 * MIB);
        assert_eq!(limits.max_retained_bytes, 64 * MIB);
        assert_eq!(limits.job_ttl_ms, 600_000);
        // §6.2：默认下载上限必须大于 566 MB 的公式模型。
        assert!(limits.max_download_bytes > 566 * 1_000_000);
    }

    /// 四个 MiB 开关在收到 0 时都必须给出**带开关名**的错误。
    #[test]
    fn zero_mib_limits_are_rejected_with_the_flag_name() {
        let cases: [(RawServeLimits, &str); 5] = [
            (
                RawServeLimits {
                    max_body_mb: 0,
                    ..Default::default()
                },
                "--max-body-mb",
            ),
            (
                RawServeLimits {
                    max_result_mb: 0,
                    ..Default::default()
                },
                "--max-result-mb",
            ),
            (
                RawServeLimits {
                    max_export_mb: 0,
                    ..Default::default()
                },
                "--max-export-mb",
            ),
            (
                RawServeLimits {
                    max_download_mb: 0,
                    ..Default::default()
                },
                "--max-download-mb",
            ),
            (
                RawServeLimits {
                    max_retained_mb: 0,
                    ..Default::default()
                },
                "--max-retained-mb",
            ),
        ];
        for (raw, expected_field) in cases {
            let error = raw
                .validate()
                .expect_err(&format!("{expected_field}=0 must be rejected"));
            assert_eq!(error.field(), expected_field, "error: {error}");
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("invalid {expected_field}=0")),
                "error must locate the flag and the value: {error}"
            );
        }
    }

    /// 乘法溢出必须是可定位错误，**绝不回绕**成一个小值。
    #[test]
    fn mib_multiplication_overflow_is_rejected_and_never_wraps() {
        let raw = RawServeLimits {
            max_body_mb: u64::MAX,
            ..Default::default()
        };
        let error = raw.validate().expect_err("u64::MAX MiB must overflow");
        assert_eq!(error.field(), "--max-body-mb");
        assert!(error.reason().contains("overflows"), "error: {error}");
        assert!(
            error.reason().contains(&u64::MAX.to_string()),
            "the error must name the offending value: {error}"
        );

        // 恰好溢出的边界：2^44 MiB = 2^64 B 不可表示；2^44 - 1 MiB 可以。
        let just_over = RawServeLimits {
            max_result_mb: 1 << 44,
            ..Default::default()
        };
        assert!(just_over.validate().is_err());
        let just_under = RawServeLimits {
            max_result_mb: (1 << 44) - 1,
            ..Default::default()
        };
        let limits = just_under
            .validate()
            .expect("(2^44 - 1) MiB must still fit in u64");
        assert_eq!(limits.max_result_bytes, ((1 << 44) - 1) * MIB);
    }

    /// 队列/保留/TTL 参数为 0 时都必须拒绝（0 会让 §8.3 与 §4.5 的语义失效）。
    #[test]
    fn zero_queue_and_retention_parameters_are_rejected() {
        let cases: [(RawServeLimits, &str); 6] = [
            (
                RawServeLimits {
                    max_queue_text: 0,
                    ..Default::default()
                },
                "--max-queue-text",
            ),
            (
                RawServeLimits {
                    max_queue_formula: 0,
                    ..Default::default()
                },
                "--max-queue-formula",
            ),
            (
                RawServeLimits {
                    max_consecutive_text: 0,
                    ..Default::default()
                },
                "--max-consecutive-text",
            ),
            (
                RawServeLimits {
                    max_consecutive_formula: 0,
                    ..Default::default()
                },
                "--max-consecutive-formula",
            ),
            (
                RawServeLimits {
                    max_retained: 0,
                    ..Default::default()
                },
                "--max-retained",
            ),
            (
                RawServeLimits {
                    max_tombstones: 0,
                    ..Default::default()
                },
                "--max-tombstones",
            ),
        ];
        for (raw, expected_field) in cases {
            let error = raw
                .validate()
                .expect_err(&format!("{expected_field}=0 must be rejected"));
            assert_eq!(error.field(), expected_field, "error: {error}");
        }

        let ttl = RawServeLimits {
            job_ttl_secs: 0,
            ..Default::default()
        };
        let error = ttl
            .validate()
            .expect_err("--job-ttl-secs=0 must be rejected");
        assert_eq!(error.field(), "--job-ttl-secs");
    }

    /// TTL 的秒→毫秒换算同样不允许溢出。
    #[test]
    fn job_ttl_seconds_overflow_is_rejected() {
        let raw = RawServeLimits {
            job_ttl_secs: u64::MAX,
            ..Default::default()
        };
        let error = raw.validate().expect_err("u64::MAX seconds must overflow");
        assert_eq!(error.field(), "--job-ttl-secs");
        assert!(error.reason().contains("milliseconds"), "error: {error}");
    }

    /// `1 MiB` 正好是 `MIB` 字节；`2 MiB - 1` 这样的取值也必须线性。
    #[test]
    fn mib_to_bytes_is_exact() {
        let one = RawServeLimits {
            max_body_mb: 1,
            ..Default::default()
        }
        .validate()
        .expect("1 MiB is valid");
        assert_eq!(one.max_body_bytes, 1_048_576);

        let seven = RawServeLimits {
            max_body_mb: 7,
            ..Default::default()
        }
        .validate()
        .expect("7 MiB is valid");
        assert_eq!(seven.max_body_bytes, 7 * 1_048_576);
    }

    /// §6.2：`--max-download-mb` **同时**是单文件上限与整批下载的总量额度；
    /// 剩余额度按文件递减，且一次失败的记账不吃掉预算。
    #[test]
    fn the_download_budget_starts_at_the_single_file_cap_and_decreases_per_file() {
        let limits = ServeLimits::defaults();
        let mut budget = limits.download_budget();

        assert_eq!(budget.total_bytes(), 1024 * MIB);
        assert_eq!(budget.remaining_bytes(), 1024 * MIB);
        assert_eq!(
            budget.per_file_cap(),
            1024 * MIB,
            "the per-file cap is the whole budget until something is downloaded"
        );

        // 566 MB 的公式模型必须能放进默认预算（§6.2）。
        budget
            .charge(593_915_961)
            .expect("the formula model must fit the default budget");
        assert_eq!(budget.remaining_bytes(), 1024 * MIB - 593_915_961);
        assert_eq!(
            budget.per_file_cap(),
            budget.remaining_bytes(),
            "the next file's cap is the remaining budget"
        );

        let error = budget
            .charge(1024 * MIB)
            .expect_err("a file larger than the remaining budget must be refused");
        assert_eq!(
            budget.remaining_bytes(),
            1024 * MIB - 593_915_961,
            "a refused charge must not consume the budget: {error}"
        );
        assert!(error.to_string().contains("byte limit"), "{error}");
    }
}
